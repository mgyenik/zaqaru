//! Where a guest spends its instructions, by address.
//!
//! The mnemonic histogram says what *kind* of instruction a run is made of.
//! It cannot say which code, and for a container that is two orders of
//! magnitude off native the useful question is which guest function is
//! costing that — a Python interpreter loop being a Python interpreter loop
//! is one answer and nothing to be done about it, and half a run inside
//! `memcpy` is a completely different answer with an obvious response.
//!
//! Counts are exact and keyed by instruction address. Turning an address
//! back into a name is not this module's job and cannot be: the address
//! space, its mappings and the files behind them belong to the kernel, so
//! this hands back raw addresses and kisal attributes them.
//!
//! **Off unless asked for**, like the histogram, and for the same reason —
//! it runs once per retired instruction. With the feature on, a run takes
//! several times longer. That does not distort the answer: which
//! instructions a deterministic guest executes is a property of the guest,
//! and the counting cannot change it.

#[cfg(feature = "profile")]
use std::cell::RefCell;
#[cfg(feature = "profile")]
use std::collections::HashMap;

#[cfg(feature = "profile")]
thread_local! {
    /// Address to instructions retired there. Thread-local so that a
    /// container's whole process tree accumulates into one profile, which
    /// is what a tree that forks needs.
    static COUNTS: RefCell<HashMap<u64, u64>> = RefCell::new(HashMap::new());
}

/// Counts one retired instruction at `address`.
#[cfg(feature = "profile")]
pub fn record(address: u64) {
    COUNTS.with_borrow_mut(|counts| *counts.entry(address).or_insert(0) += 1);
}

#[cfg(not(feature = "profile"))]
#[inline(always)]
pub fn record(_address: u64) {}

/// Every address that retired anything, hottest first, with the total.
///
/// `None` when the feature is off, so a caller prints nothing rather than
/// an empty profile — which reads like a run that executed nothing.
#[cfg(not(feature = "profile"))]
pub fn hot() -> Option<(Vec<(u64, u64)>, u64)> {
    None
}

#[cfg(feature = "profile")]
pub fn hot() -> Option<(Vec<(u64, u64)>, u64)> {
    COUNTS.with_borrow(|counts| {
        if counts.is_empty() {
            return None;
        }
        let total = counts.values().sum();
        let mut rows: Vec<(u64, u64)> = counts.iter().map(|(a, c)| (*a, *c)).collect();
        rows.sort_by(|left, right| right.1.cmp(&left.1));
        Some((rows, total))
    })
}
