//! The single-thread hash join, in the Rust subset Charon and Aeneas
//! translate (`Vec`, index loops, no closures or iterators), so that
//! `proofs/reconcile/Reconcile/Join.lean` can prove it computes, for every
//! row on both sides, what the `BTreeMap` reference ([`crate::reference`])
//! computes. [`crate::cpu_single`] is this code; [`crate::cpu_parallel`]
//! runs it on each partition.
//!
//! An open-addressing table per side: linear probing from [`start`], at
//! most half full, holding per key how many rows have it and the lowest row
//! index among them.

// The kernel style of the other extracted crates: index loops, `&Vec`.
#![allow(clippy::ptr_arg, clippy::manual_range_contains, clippy::len_zero)]

use crate::rules::{classify_external, classify_internal, hash, Record, EMPTY, NONE};

/// One side's table: slot `s` holds key `keys[s]` (or [`EMPTY`]), how many
/// rows have it (`count[s]`) and the lowest index among them (`first[s]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinTable {
    pub keys: Vec<u64>,
    pub count: Vec<u32>,
    pub first: Vec<u32>,
}

/// The per-row results of a join, in input order (see [`crate::Report`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinOut {
    pub internal: Vec<u32>,
    pub matched: Vec<u32>,
    pub external: Vec<u32>,
}

/// Slots for `n` rows: a power of two, at least 16 and at least `2n`.
pub fn table_cap(n: usize) -> usize {
    let mut c: usize = 16;
    while c < 2 * n {
        c *= 2;
    }
    c
}

/// An empty table of `cap` slots.
pub fn empty_table(cap: usize) -> JoinTable {
    JoinTable { keys: vec![EMPTY; cap], count: vec![0; cap], first: vec![NONE; cap] }
}

/// Where probing for `k` starts in a table of `cap` (> 0) slots: the same
/// slot as [`crate::Table`], [`crate::rules::find_slot`] and the GPU tables
/// ([`hash`] masked to the capacity, a power of two). The results never
/// depend on where probing starts (the proof takes any start below `cap`).
#[inline(always)]
pub fn start(k: u64, cap: usize) -> usize {
    (hash(k) & (cap as u64 - 1)) as usize
}

/// The slot holding `k`, or else the first empty slot probing for `k`
/// reaches. Probes each slot at most once; a table with an empty slot
/// (every table here) always ends in one of those two cases.
#[inline(always)]
pub fn probe(keys: &Vec<u64>, k: u64) -> usize {
    let cap = keys.len();
    let mut s = start(k, cap);
    let mut n: usize = 0;
    while n < cap {
        if keys[s] == k {
            return s;
        }
        if keys[s] == EMPTY {
            return s;
        }
        s = if s + 1 == cap { 0 } else { s + 1 };
        n += 1;
    }
    s
}

/// Add row `i` with key `k`.
#[inline(always)]
pub fn insert(t: &mut JoinTable, k: u64, i: u32) {
    let s = probe(&t.keys, k);
    if t.keys[s] == EMPTY {
        t.keys[s] = k;
        t.first[s] = i;
        t.count[s] = 1;
    } else {
        t.count[s] += 1;
    }
}

/// The table of `rows`, row `i` inserted as index `i`.
pub fn build(rows: &Vec<Record>) -> JoinTable {
    let mut t = empty_table(table_cap(rows.len()));
    let mut i = 0;
    while i < rows.len() {
        insert(&mut t, rows[i].key, i as u32);
        i += 1;
    }
    t
}

/// `(count, first)` for key `k`; `(0, NONE)` if no row has it.
#[inline(always)]
pub fn lookup(t: &JoinTable, k: u64) -> (u32, u32) {
    let s = probe(&t.keys, k);
    if t.keys[s] == k {
        (t.count[s], t.first[s])
    } else {
        (0, NONE)
    }
}

/// Internal row `x`: its code and its first external match (or `NONE`).
#[inline(always)]
pub fn internal_code(x: &Record, external: &Vec<Record>, int: &JoinTable, ext: &JoinTable) -> (u32, u32) {
    let (ic, _) = lookup(int, x.key);
    let (ec, ef) = lookup(ext, x.key);
    if ec == 0 {
        (classify_internal(x, ic, ec, None), ef)
    } else {
        (classify_internal(x, ic, ec, Some(&external[ef as usize])), ef)
    }
}

/// The join of whole inputs. Needs keys other than `EMPTY` and fewer than
/// `u32::MAX` rows a side ([`crate::validate`]).
pub fn join(internal: &Vec<Record>, external: &Vec<Record>) -> JoinOut {
    let ext = build(external);
    let int = build(internal);
    let mut codes = Vec::with_capacity(internal.len());
    let mut matched = Vec::with_capacity(internal.len());
    let mut i = 0;
    while i < internal.len() {
        let (c, m) = internal_code(&internal[i], external, &int, &ext);
        codes.push(c);
        matched.push(m);
        i += 1;
    }
    let mut ecodes = Vec::with_capacity(external.len());
    let mut j = 0;
    while j < external.len() {
        let k = external[j].key;
        let (ic, _) = lookup(&int, k);
        let (ec, _) = lookup(&ext, k);
        ecodes.push(classify_external(ic, ec));
        j += 1;
    }
    JoinOut { internal: codes, matched, external: ecodes }
}
