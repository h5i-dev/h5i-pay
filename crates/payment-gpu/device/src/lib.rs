//! h5i-pay's GPU entry points. Every decision is made by the same Rust the
//! CPU engines run and the Lean proofs are about, included here verbatim:
//!
//! * `policy::check` (`crates/payment-domain/src/policy.rs`, extracted by
//!   Aeneas, `Policies.check_sound`);
//! * `rules::classify_internal` / `classify_external` and the per-row probe
//!   `internal_row` / `external_row` (`crates/payment-reconcile/src/rules.rs`;
//!   the classification is extracted and proven in `proofs/reconcile`).
//!
//! What is written here is the launch shell: one thread per row, bounds
//! checks, the raw-pointer ABI, and `recon_build`, the parallel hash-table
//! build with device atomics (trusted, tested against the CPU engines in
//! `tests/differential`).
#![cfg(feature = "gpu")]
#![allow(improper_ctypes_definitions, clippy::missing_safety_doc)]
#![no_std]

#[path = "../../../payment-domain/src/policy.rs"]
#[allow(dead_code)]
mod policy;
#[path = "../../../payment-reconcile/src/rules.rs"]
#[allow(dead_code)]
mod rules;

use core::slice::from_raw_parts;
use core::sync::atomic::Ordering::Relaxed;
use cuda_std::atomic::mid::{atomic_compare_and_swap_u64_device, atomic_fetch_add_u32_device, atomic_fetch_min_u32_device};
use cuda_std::{kernel, thread};
use policy::{Limits, PaymentReq};
use rules::{Record, TableRef, EMPTY};

/// One request per thread: `out[i] = policy::check(...)`.
#[kernel]
pub unsafe fn policy_check(
    reqs: *const PaymentReq,
    n: usize,
    limits: *const Limits,
    merchants: *const u64,
    n_merchants: usize,
    blocked: *const u64,
    blocked_len: usize,
    n_blocked: usize,
    out: *mut u32,
) {
    let i = thread::index_1d() as usize;
    if i < n {
        unsafe {
            let merchants = from_raw_parts(merchants, n_merchants);
            let blocked = from_raw_parts(blocked, blocked_len);
            *out.add(i) = policy::check(&*limits, merchants, blocked, n_blocked, &*reqs.add(i));
        }
    }
}

/// Insert row `i`'s key; count the rows per key and keep the lowest index.
/// The table (`cap = mask + 1` slots) starts all `EMPTY` / 0 / `NONE` and is
/// at most half full, so the probe ends.
#[kernel]
pub unsafe fn recon_build(rows: *const Record, n: usize, keys: *mut u64, count: *mut u32, first: *mut u32, mask: u64) {
    let i = thread::index_1d() as usize;
    if i < n {
        unsafe {
            let k = (*rows.add(i)).key;
            let mut s = rules::hash(k) & mask;
            loop {
                let prev = atomic_compare_and_swap_u64_device(keys.add(s as usize), EMPTY, k, Relaxed);
                if prev == EMPTY || prev == k {
                    atomic_fetch_add_u32_device(count.add(s as usize), Relaxed, 1);
                    atomic_fetch_min_u32_device(first.add(s as usize), Relaxed, i as u32);
                    return;
                }
                s = (s + 1) & mask;
            }
        }
    }
}

/// A table built by `recon_build`, as the shared per-row code sees it.
unsafe fn table<'a>(keys: *const u64, count: *const u32, first: *const u32, mask: u64) -> TableRef<'a> {
    let cap = (mask + 1) as usize;
    unsafe { TableRef { keys: from_raw_parts(keys, cap), count: from_raw_parts(count, cap), first: from_raw_parts(first, cap), mask } }
}

/// Internal row `i`: `rules::internal_row` (code and matched index).
#[kernel]
pub unsafe fn recon_internal(
    internal: *const Record,
    ni: usize,
    external: *const Record,
    ne: usize,
    ikeys: *const u64,
    icount: *const u32,
    ifirst: *const u32,
    imask: u64,
    ekeys: *const u64,
    ecount: *const u32,
    efirst: *const u32,
    emask: u64,
    codes: *mut u32,
    matched: *mut u32,
) {
    let i = thread::index_1d() as usize;
    if i < ni {
        unsafe {
            let int = table(ikeys, icount, ifirst, imask);
            let ext = table(ekeys, ecount, efirst, emask);
            let (code, m) = rules::internal_row(&*internal.add(i), from_raw_parts(external, ne), int, ext);
            *codes.add(i) = code;
            *matched.add(i) = m;
        }
    }
}

/// External row `j`: `rules::external_row`.
#[kernel]
pub unsafe fn recon_external(
    external: *const Record,
    ne: usize,
    ikeys: *const u64,
    icount: *const u32,
    ifirst: *const u32,
    imask: u64,
    ekeys: *const u64,
    ecount: *const u32,
    efirst: *const u32,
    emask: u64,
    codes: *mut u32,
) {
    let j = thread::index_1d() as usize;
    if j < ne {
        unsafe {
            let int = table(ikeys, icount, ifirst, imask);
            let ext = table(ekeys, ecount, efirst, emask);
            *codes.add(j) = rules::external_row((*external.add(j)).key, int, ext);
        }
    }
}

// Column entry points (J1): one side as four columns, the layout of the
// batch worker's binary records file, so the host copies each column from
// the mapped file to the device as is. Codes are one byte per row.

/// `recon_build` for keys `keys_in[0..n]`, which are rows `base..base + n`
/// of their side (the host builds a side in chunks as they arrive).
#[kernel]
pub unsafe fn recon_build_cols(keys_in: *const u64, n: usize, base: u32, keys: *mut u64, count: *mut u32, first: *mut u32, mask: u64) {
    let i = thread::index_1d() as usize;
    if i < n {
        unsafe {
            let k = *keys_in.add(i);
            let row = base + i as u32;
            let mut s = rules::hash(k) & mask;
            loop {
                let prev = atomic_compare_and_swap_u64_device(keys.add(s as usize), EMPTY, k, Relaxed);
                if prev == EMPTY || prev == k {
                    atomic_fetch_add_u32_device(count.add(s as usize), Relaxed, 1);
                    atomic_fetch_min_u32_device(first.add(s as usize), Relaxed, row);
                    return;
                }
                s = (s + 1) & mask;
            }
        }
    }
}

unsafe fn cols<'a>(key: *const u64, amount: *const u64, currency: *const u32, status: *const u32, n: usize) -> rules::Cols<'a> {
    unsafe {
        rules::Cols { key: from_raw_parts(key, n), amount: from_raw_parts(amount, n), currency: from_raw_parts(currency, n), status: from_raw_parts(status, n) }
    }
}

/// Internal row `i`: `rules::internal_row_cols`; `matched` may be null.
#[kernel]
pub unsafe fn recon_internal_cols(
    ikey: *const u64,
    iamount: *const u64,
    icurrency: *const u32,
    istatus: *const u32,
    ni: usize,
    ekey: *const u64,
    eamount: *const u64,
    ecurrency: *const u32,
    estatus: *const u32,
    ne: usize,
    ikeys: *const u64,
    icount: *const u32,
    ifirst: *const u32,
    imask: u64,
    ekeys: *const u64,
    ecount: *const u32,
    efirst: *const u32,
    emask: u64,
    codes: *mut u8,
    matched: *mut u32,
) {
    let i = thread::index_1d() as usize;
    if i < ni {
        unsafe {
            let int = table(ikeys, icount, ifirst, imask);
            let ext = table(ekeys, ecount, efirst, emask);
            let x = Record { key: *ikey.add(i), amount: *iamount.add(i), currency: *icurrency.add(i), status: *istatus.add(i) };
            let (code, m) = rules::internal_row_cols(&x, cols(ekey, eamount, ecurrency, estatus, ne), int, ext);
            *codes.add(i) = code as u8;
            if !matched.is_null() {
                *matched.add(i) = m;
            }
        }
    }
}

/// External row `j`: `rules::external_row` on its key.
#[kernel]
pub unsafe fn recon_external_cols(
    ekey: *const u64,
    ne: usize,
    ikeys: *const u64,
    icount: *const u32,
    ifirst: *const u32,
    imask: u64,
    ekeys: *const u64,
    ecount: *const u32,
    efirst: *const u32,
    emask: u64,
    codes: *mut u8,
) {
    let j = thread::index_1d() as usize;
    if j < ne {
        unsafe {
            let int = table(ikeys, icount, ifirst, imask);
            let ext = table(ekeys, ecount, efirst, emask);
            *codes.add(j) = rules::external_row(*ekey.add(j), int, ext) as u8;
        }
    }
}
