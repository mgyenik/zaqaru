//! Which instructions a real program actually retires, and which of them the
//! fast path already handles.
//!
//! [`crate::quick`] lowers eleven mnemonics, and the choice of eleven was
//! made by reading code and guessing. Guessing has a poor record in this
//! area: a link-time-optimisation build was worth 2% and a permission cache
//! was worth nothing, both of them confidently predicted to be worth much
//! more. So the next mnemonic to lower should be chosen by counting.
//!
//! Two counts per mnemonic, and the second is the one worth having. *Total*
//! says how much of a run an instruction is. *Lowered* says how much of that
//! took the fast path — and the gap between them is the interesting number,
//! because a mnemonic the lowering claims to handle can still fall back on
//! an operand shape it declines. A `mov` that is 12% of a run and only 9%
//! lowered has a quarter of itself hiding somewhere, and no amount of
//! reading `quick.rs` will say where.
//!
//! **Off unless asked for.** Without the `histogram` feature [`record`] is
//! an empty inlined function and the counters do not exist, because this
//! runs once per retired instruction and the engine's whole problem is what
//! it does once per retired instruction. With the feature on it is slower
//! than the thing it is measuring the shape of, which is fine: the shape is
//! a property of the guest program, not of how fast the engine ran it.
//!
//! It also does not need to run in wasm. What each mnemonic costs differs
//! between the two builds; how *often* the guest executes one does not. So
//! the natural place to take this measurement is the native
//! `interpret` example, which runs the same guest several times faster.

#[cfg(feature = "histogram")]
use std::cell::RefCell;

#[cfg(feature = "histogram")]
#[derive(Clone, Copy, Default)]
struct Row {
    /// Kept rather than recovered from the index. The index *is* the
    /// mnemonic's discriminant, so it could be turned back into one with a
    /// transmute — and that would be undefined behaviour the first time a
    /// row existed for a value iced does not define, to save storing two
    /// bytes a diagnostic build will never notice.
    mnemonic: Option<iced_x86::Mnemonic>,
    /// Retired, whichever path ran it.
    total: u64,
    /// Retired through [`crate::quick`] rather than the general path.
    lowered: u64,
}

#[cfg(feature = "histogram")]
thread_local! {
    /// Indexed by the mnemonic's own discriminant, grown on demand rather
    /// than sized against a constant iced does not publish.
    ///
    /// Thread-local rather than global, and that is right rather than
    /// convenient: a container's processes all run on the one thread the
    /// scheduler owns, so this accumulates across the whole process tree —
    /// which is what a program that forks needs it to do.
    static ROWS: RefCell<Vec<Row>> = const { RefCell::new(Vec::new()) };
}

/// Counts one retired instruction.
#[cfg(feature = "histogram")]
pub fn record(instruction: &iced_x86::Instruction, lowered: bool) {
    let index = instruction.mnemonic() as usize;
    ROWS.with_borrow_mut(|rows| {
        if index >= rows.len() {
            rows.resize(index + 1, Row::default());
        }
        rows[index].mnemonic = Some(instruction.mnemonic());
        rows[index].total += 1;
        rows[index].lowered += u64::from(lowered);
    });
}

/// The same call, costing nothing, when nobody asked to measure.
#[cfg(not(feature = "histogram"))]
#[inline(always)]
pub fn record(_instruction: &iced_x86::Instruction, _lowered: bool) {}

/// The ranked table, or `None` when the feature is off.
///
/// `None` rather than an empty string so that a caller prints nothing at all
/// rather than a heading over no rows, which reads like a run that retired
/// nothing.
#[cfg(not(feature = "histogram"))]
pub fn report() -> Option<String> {
    None
}

/// Mnemonics by how much of the run they are, and how much of each the fast
/// path already takes.
#[cfg(feature = "histogram")]
pub fn report() -> Option<String> {
    use core::fmt::Write;

    let mut rows: Vec<Row> = ROWS.with_borrow(|rows| {
        rows.iter().filter(|row| row.total > 0).copied().collect()
    });
    if rows.is_empty() {
        return None;
    }
    rows.sort_by(|left, right| right.total.cmp(&left.total));
    let retired: u64 = rows.iter().map(|row| row.total).sum();
    let lowered: u64 = rows.iter().map(|row| row.lowered).sum();

    let mut out = String::new();
    let _ = writeln!(
        out,
        "\n{retired} instructions retired, {:.1}% of them through the fast path.\n\
         The gap in a partly-lowered row is an operand shape `quick.rs` declines.\n",
        lowered as f64 / retired as f64 * 100.0
    );
    let _ = writeln!(
        out,
        "  {:<16} {:>14} {:>7} {:>7}  {}",
        "mnemonic", "retired", "share", "cumul.", "lowered"
    );
    let mut running = 0u64;
    for row in rows.iter().take(40) {
        running += row.total;
        let name = match row.mnemonic {
            Some(mnemonic) => format!("{mnemonic:?}"),
            None => String::from("?"),
        };
        let state = match (row.lowered, row.total) {
            (0, _) => String::from("no"),
            (l, t) if l == t => String::from("yes"),
            (l, t) => format!("{:.0}%", l as f64 / t as f64 * 100.0),
        };
        let _ = writeln!(
            out,
            "  {name:<16} {:>14} {:>6.2}% {:>6.2}%  {state}",
            row.total,
            row.total as f64 / retired as f64 * 100.0,
            running as f64 / retired as f64 * 100.0,
        );
    }
    Some(out)
}
