//! Every probe in the corpus, run against hardware one instruction at a
//! time.
//!
//! This is the acceptance test the interpreter is built against: not "the
//! program printed the right thing" but "every register and every defined
//! flag matched real silicon after every retired instruction". A semantics
//! arm that is subtly wrong — a carry preserved that should have been
//! written, a byte written eight bits too low, an overflow computed from the
//! wrong pair — fails here at the instruction that did it, with the address
//! in the message.
//!
//! See `support::lockstep` for how the two machines are brought into the
//! same state to begin with.
//!
//! # Two tiers, and why
//!
//! Every comparison costs a `fork`, an `exec` and one `ptrace` stop per
//! instruction, so the full sweep — three corpora, three optimisation
//! levels, twelve argument pairs each — is seconds rather than
//! milliseconds. Seconds do not belong in the loop somebody runs after
//! every edit: a suite that is slow enough to think about is a suite people
//! stop running, and the whole value of this instrument is that it is the
//! thing you run *first*.
//!
//! So the default tier is one optimisation level and three argument pairs,
//! which is the same instrument at a tenth of the cost and catches the
//! overwhelming majority of what it would catch anyway. The full sweep and
//! the coverage assertions are `#[ignore]`d and run deliberately:
//!
//! ```text
//! cargo test -p targum                          # the fast tier, well under a second
//! cargo test -p targum -- --ignored             # the whole sweep, seconds
//! ```

#![cfg(target_os = "linux")]
#![cfg(target_arch = "x86_64")]

mod support;

use std::collections::BTreeSet;
use std::path::Path;

use support::{WorkingDirectory, compile, compile_with, lockstep, probes, require_coverage};

/// Inputs worth trying for any integer probe: the identities, the signed
/// boundaries, the values that make carries and overflows happen, and a
/// couple of dense bit patterns.
const ARGUMENTS: [(u64, u64); 12] = [
    (0, 0),
    (1, 1),
    (0, 1),
    (1, 0),
    (u64::MAX, 1),
    (1, u64::MAX),
    (0x7fff_ffff_ffff_ffff, 1),
    (0x8000_0000_0000_0000, u64::MAX),
    (0x5555_5555_5555_5555, 0x3333_3333_3333_3333),
    (0xffff_ffff, 0xffff_ffff),
    (0x0000_0000_8000_0000, 0x0000_0000_0000_0021),
    (0x1234_5678_9abc_def0, 0x0fed_cba9_8765_4321),
];

/// The fast tier's slice of them: zero, the sign boundary against minus one,
/// and a dense pattern. Between them they exercise both carry directions,
/// both overflow directions, a NaN and a denormal in the floating-point
/// corpora, and the divide that traps.
const FAST_ARGUMENTS: [(u64, u64); 3] = [
    (0, 1),
    (0x8000_0000_0000_0000, u64::MAX),
    (0x1234_5678_9abc_def0, 0x0fed_cba9_8765_4321),
];

/// Optimisation levels the corpus is compiled at.
///
/// Not thoroughness for its own sake: the levels emit different instructions
/// for the same source. `-O0` keeps everything in memory and branches where
/// `-O2` uses `setcc` and `cmovcc`; `-Os` reaches for string instructions
/// and whole-register moves; `-O2` splits cold paths into tail calls. An
/// engine meant for binaries it did not compile has to handle what it is
/// given.
const LEVELS: [&str; 3] = ["-O0", "-O2", "-Os"];

/// The fast tier's level. `-O2` because it is what a real binary is built
/// at, and because it is the level that reaches for the conditional moves
/// and the folded flag consumers.
const FAST_LEVEL: &str = "-O2";

/// Long enough for the deepest probe's recursion, short enough that a
/// runaway names itself rather than hanging the suite.
const LIMIT: u64 = 20_000;

/// Instructions the integer corpus exists to exercise.
///
/// Asserted rather than hoped for: a probe whose compiler folded its point
/// away passes every comparison in this file and tests nothing, and that is
/// indistinguishable from success without this list.
const INTEGER_COVERAGE: &[&str] = &[
    "Add", "Sub", "Cmp", "And", "Or", "Xor", "Test", "Adc", "Sbb", "Inc", "Dec", "Neg", "Not",
    "Imul", "Idiv", "Div", "Shl", "Shr", "Sar", "Rol", "Ror", "Rcl", "Rcr", "Shld", "Shrd", "Bt",
    "Bts", "Btr", "Btc", "Bsf", "Bsr", "Movzx", "Movsx", "Movsxd", "Lea", "Xchg", "Xadd",
    "Cmpxchg", "Push", "Pop", "Pushfq", "Popfq", "Lahf", "Sahf", "Clc", "Stc", "Cmc", "Cld",
    "Std", "Cqo", "Cdqe", "Call", "Ret", "Jmp", "Movsb", "Stosb", "Cmpsb", "Scasb",
];

/// Instructions the vector corpus exists to exercise.
const VECTOR_COVERAGE: &[&str] = &[
    "Movd", "Movq", "Movsd", "Movss", "Movdqa", "Movdqu", "Movapd", "Addsd", "Subsd", "Mulsd",
    "Divsd", "Sqrtsd", "Minsd", "Maxsd", "Addss", "Mulss", "Ucomisd", "Cvttsd2si", "Cvtsi2sd",
    "Cvtsd2ss", "Cvtss2sd", "Addpd", "Mulpd", "Divpd", "Addps", "Mulps", "Paddb", "Paddw",
    "Paddd", "Paddq", "Psubd", "Pmullw", "Pcmpeqd", "Pcmpgtd", "Psllw", "Psrlw", "Psrad",
    "Pmovmskb", "Movmskps", "Movmskpd", "Pshufd", "Pshuflw", "Pshufhw", "Punpcklbw", "Packsswb",
    "Packuswb", "Paddsw", "Psubsw", "Paddusw", "Pandn", "Pand", "Por", "Pxor", "Subss", "Divss", "Minss", "Maxss", "Sqrtss",
    "Movddup", "Movshdup", "Movsldup", "Haddpd", "Hsubpd", "Addsubpd", "Haddps", "Addsubps",
];

/// Instructions the floating-point corpus exists to exercise.
const FLOATING_COVERAGE: &[&str] = &[
    "Fld", "Fld1", "Fldz", "Fldpi", "Fldl2t", "Fldl2e", "Fldlg2", "Fldln2", "Fild", "Fst",
    "Fstp", "Fistp", "Fadd", "Faddp", "Fsub", "Fsubp", "Fsubr", "Fmul", "Fmulp", "Fdiv", "Fdivp",
    "Fchs", "Fabs", "Fsqrt", "Frndint", "Fprem", "Fscale", "Fxch", "Fincstp", "Fdecstp",
    "Fucomp", "Fcomi", "Fnstsw", "Fnstcw", "Fldcw", "Fnstenv", "Fldenv", "Fnsave", "Frstor",
    "Fxsave", "Fxrstor", "Fninit",
];

/// One corpus program, at the levels and over the arguments given.
///
/// Answers which mnemonics the sweep actually put through both machines, so
/// a caller can hold the corpus to what it was written to cover.
fn sweep(
    workspace: &WorkingDirectory,
    source: &str,
    extra: &[&str],
    levels: &[&str],
    arguments: &[(u64, u64)],
) -> BTreeSet<&'static str> {
    let mut seen = BTreeSet::new();
    for level in levels {
        let program = compile_with(workspace, source, level, extra);
        let found = probes(&program);
        assert!(!found.is_empty(), "{source} has no probes");
        for (name, _) in &found {
            for (left, right) in arguments.iter().copied() {
                seen.extend(one(&program, name, left, right));
            }
        }
    }
    seen
}

fn one(program: &Path, name: &str, left: u64, right: u64) -> BTreeSet<&'static str> {
    // The recursive probe is bounded by its first argument, so it gets a
    // small one rather than four billion frames.
    let left = match name {
        "probe_nested_calls" => left & 0x1f,
        _ => left,
    };
    lockstep(program, name, [left, right, 0, 0, 0, 0], LIMIT).mnemonics
}

/// The integer corpus keeps the vector registers switched off: a `movdqa`
/// that gcc emitted while vectorising a byte loop would be a test of SSE
/// hiding inside a test of `for`, and a failure in it would name the loop
/// rather than the instruction. The other two corpora want them on.
const INTEGER_ONLY: &[&str] = &["-mgeneral-regs-only"];

// ---- the fast tier ------------------------------------------------------

#[test]
fn the_integer_core_matches_hardware() {
    let workspace = WorkingDirectory::new("lockstep-integer");
    sweep(
        &workspace,
        "integer.c",
        INTEGER_ONLY,
        &[FAST_LEVEL],
        &FAST_ARGUMENTS,
    );
}

#[test]
fn the_vector_core_matches_hardware() {
    let workspace = WorkingDirectory::new("lockstep-vector");
    sweep(&workspace, "vector.c", &[], &[FAST_LEVEL], &FAST_ARGUMENTS);
}

#[test]
fn the_floating_point_core_matches_hardware() {
    let workspace = WorkingDirectory::new("lockstep-floating");
    sweep(&workspace, "floating.c", &[], &[FAST_LEVEL], &FAST_ARGUMENTS);
}

/// The negative control: the oracle has to *see*.
///
/// A comparison that passes proves nothing unless a broken machine fails it,
/// and "the harness silently compared nothing" is a failure mode that looks
/// exactly like success. So this deliberately desynchronises the two
/// machines — one extra native step, which no correct interpreter can
/// match — and requires the panic.
#[test]
fn the_oracle_fails_when_the_machines_disagree() {
    let workspace = WorkingDirectory::new("lockstep-control");
    let program = compile(&workspace, "integer.c", "-O2");
    let outcome = std::panic::catch_unwind(|| {
        support::lockstep_desynchronised(&program, "probe_arithmetic", [3, 5, 0, 0, 0, 0], LIMIT)
    });
    assert!(
        outcome.is_err(),
        "the oracle compared a deliberately divergent machine and said nothing"
    );
}

// ---- the comprehensive tier ---------------------------------------------

/// The whole sweep: three optimisation levels, twelve argument pairs, and
/// the coverage each corpus was written to reach.
///
/// One test rather than three so that the run is one decision. Ignored by
/// default; `cargo test -p targum -- --ignored` is the deliberate check, and it
/// is what a change to the semantics should be finished against.
#[test]
#[ignore = "the full sweep: seconds, not milliseconds — run it deliberately"]
fn every_corpus_matches_hardware_at_every_optimisation_level() {
    let workspace = WorkingDirectory::new("lockstep-full");
    let integer = sweep(&workspace, "integer.c", INTEGER_ONLY, &LEVELS, &ARGUMENTS);
    let vector = sweep(&workspace, "vector.c", &[], &LEVELS, &ARGUMENTS);
    let floating = sweep(&workspace, "floating.c", &[], &LEVELS, &ARGUMENTS);
    // Coverage is aggregated across levels rather than required of each:
    // which instructions a level emits is the compiler's business — `-O2`
    // inlines the calls away and reaches for `movapd` where `-O0` reaches
    // for `movsd` — and demanding the same set from every level would be
    // demanding that the compiler stop optimising.
    require_coverage("the integer corpus", &integer, INTEGER_COVERAGE);
    require_coverage("the vector corpus", &vector, VECTOR_COVERAGE);
    require_coverage("the floating-point corpus", &floating, FLOATING_COVERAGE);
}
