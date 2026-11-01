//! The reconciliation rules: what one row's counts and match mean, and the
//! per-row work of a hash-join engine.
//!
//! One source for every engine: the reference, CPU single-thread, CPU
//! multithread and SIMD engines call these functions, and the GPU device
//! crate (`crates/payment-gpu/device`) compiles this same file to PTX with
//! rustc_codegen_nvvm. [`classify_internal`] and [`classify_external`] are
//! extracted by Aeneas and proven in `proofs/reconcile`
//! (`h5i app check crates/payment-reconcile`); [`find_slot`],
//! [`internal_row`] and [`external_row`] (probing the hash tables) are the
//! trusted, tested shell. It uses only `core`, and no allocation.

/// One record, internal or external. 24 bytes, no padding, so a slice of
/// records is copied to the GPU as is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
#[repr(C)]
pub struct Record {
    /// The join key: kind and id (see [`key`]). `u64::MAX` is reserved.
    pub key: u64,
    pub amount: u64,
    /// ISO 4217 numeric.
    pub currency: u32,
    /// 0 settled, 1 pending, 2 reversed.
    pub status: u32,
}

/// The reserved key (empty slot in the hash tables).
pub const EMPTY: u64 = u64::MAX;

/// Record kinds, in the top byte of a key.
pub const KIND_CAPTURE: u64 = 1;
pub const KIND_REFUND: u64 = 2;

/// The join key of object `id` of `kind` in `tenant`'s data. Ids are below
/// 2^56, so the key never equals [`EMPTY`].
pub const fn key(kind: u64, id: u64) -> u64 {
    (kind << 56) | (id & ((1 << 56) - 1))
}

pub const MATCHED: u32 = 0;
pub const AMOUNT_MISMATCH: u32 = 1;
pub const CURRENCY_MISMATCH: u32 = 2;
pub const STATUS_MISMATCH: u32 = 3;
/// Internal record with no external one.
pub const MISSING_EXTERNAL: u32 = 4;
pub const DUPLICATE_EXTERNAL: u32 = 5;
pub const DUPLICATE_INTERNAL: u32 = 6;
/// External record with no internal one.
pub const EXTRA_EXTERNAL: u32 = 7;

/// No matching external record.
pub const NONE: u32 = u32::MAX;

pub const CODES: usize = 8;

/// The code of an internal record, given how many internal and external
/// records share its key and the first (lowest-index) external one.
#[inline]
pub fn classify_internal(rec: &Record, internal_count: u32, external_count: u32, first_external: Option<&Record>) -> u32 {
    if internal_count > 1 {
        return DUPLICATE_INTERNAL;
    }
    match first_external {
        None => MISSING_EXTERNAL,
        Some(e) => {
            if external_count > 1 {
                DUPLICATE_EXTERNAL
            } else if e.currency != rec.currency {
                CURRENCY_MISMATCH
            } else if e.amount != rec.amount {
                AMOUNT_MISMATCH
            } else if e.status != rec.status {
                STATUS_MISMATCH
            } else {
                MATCHED
            }
        }
    }
}

/// The code of an external record. Field mismatches are reported once, on
/// the internal side; the external side says only whether it pairs up.
#[inline]
pub fn classify_external(internal_count: u32, external_count: u32) -> u32 {
    if internal_count == 0 {
        EXTRA_EXTERNAL
    } else if internal_count > 1 {
        DUPLICATE_INTERNAL
    } else if external_count > 1 {
        DUPLICATE_EXTERNAL
    } else {
        MATCHED
    }
}

/// The hash every table implementation uses (CPU and CUDA alike, so their
/// tables have the same shape; the results do not depend on it). Built from
/// 32×32→64 multiplies, which AVX2 has.
#[inline]
pub fn hash(k: u64) -> u64 {
    let lo = k & 0xffff_ffff;
    let hi = k >> 32;
    let a = lo.wrapping_mul(0x9e37_79b1) ^ hi.wrapping_mul(0x85eb_ca77);
    a ^ (a >> 29) ^ (k >> 17)
}

/// Table capacity for `n` keys: a power of two, at most half full.
pub fn capacity(n: usize) -> usize {
    (2 * n.max(1)).next_power_of_two().max(16)
}

/// The slot of key `k` in an open-addressing table (`keys`, a power-of-two
/// length `mask + 1`, linear probing from [`hash`], [`EMPTY`] slots), or
/// `None`. The table must have an empty slot (it is at most half full).
#[inline(always)]
pub fn find_slot(keys: &[u64], mask: u64, k: u64) -> Option<usize> {
    let mut s = hash(k) & mask;
    loop {
        let x = keys[s as usize];
        if x == k {
            return Some(s as usize);
        }
        if x == EMPTY {
            return None;
        }
        s = (s + 1) & mask;
    }
}

/// One side's hash table: keys, how many rows share each key, the lowest
/// row index among them.
#[derive(Clone, Copy)]
pub struct TableRef<'a> {
    pub keys: &'a [u64],
    pub count: &'a [u32],
    pub first: &'a [u32],
    pub mask: u64,
}

impl TableRef<'_> {
    /// `(count, first)` for key `k`; `(0, NONE)` if absent.
    #[inline(always)]
    pub fn get(&self, k: u64) -> (u32, u32) {
        match find_slot(self.keys, self.mask, k) {
            Some(s) => (self.count[s], self.first[s]),
            None => (0, NONE),
        }
    }
}

/// Internal row `x`: its code and the index of the first external record
/// with its key ([`NONE`] if none). The per-row work of every hash-join
/// engine, CPU and GPU.
#[inline(always)]
pub fn internal_row(x: &Record, external: &[Record], int: TableRef<'_>, ext: TableRef<'_>) -> (u32, u32) {
    let (ic, _) = int.get(x.key);
    let (ec, ef) = ext.get(x.key);
    let first = if ec == 0 { None } else { Some(&external[ef as usize]) };
    (classify_internal(x, ic, ec, first), ef)
}

/// External row with key `k`: its code.
#[inline(always)]
pub fn external_row(k: u64, int: TableRef<'_>, ext: TableRef<'_>) -> u32 {
    classify_external(int.get(k).0, ext.get(k).0)
}
