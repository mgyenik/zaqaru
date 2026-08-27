//! Differential execution of programs that are only *partly* transpiled.
//!
//! Every other differential test compares a wholly transpiled program against
//! a wholly native one. Here half the program is neither: `interop_foreign.c`
//! is compiled by clang's own wasm backend and never goes through the
//! transpiler, so the linked module contains ordinary wasm functions sitting
//! beside emulated-convention ones, calling each other in both directions.
//!
//! The native oracle is built the ordinary way — both halves compiled for
//! x86-64 and linked — which is what makes the comparison meaningful: it is
//! the *same program*, and the two builds differ only in what the interop
//! machinery does.
//!
//! Signatures come entirely from `interop.signatures` at this milestone.
//! Inference deletes that file in two halves later, and this test is what
//! says the deletion changed nothing.

mod support;

use support::{MixedFixture, native_function};

const GUEST_SOURCES: [&str; 1] = ["interop_guest.c"];
const FOREIGN_SOURCES: [&str; 1] = ["interop_foreign.c"];
const SIGNATURES: &str = "interop.signatures";

fn fixture() -> MixedFixture {
    MixedFixture::build("interop", &GUEST_SOURCES, &FOREIGN_SOURCES, SIGNATURES)
}

/// Inputs chosen to make every arithmetic path produce a different number,
/// including the negative and zero cases that a sign-extension mistake would
/// show up in.
const INTEGER_INPUTS: [i32; 7] = [0, 1, -1, 7, -13, 1000, -100000];

#[test]
fn integer_calls_across_the_boundary_match_native() {
    let mut fixture = fixture();
    let scale = unsafe {
        native_function::<unsafe extern "C" fn(i32) -> i32>(&fixture.native, "guest_uses_scale")
    };
    let pointer = unsafe {
        native_function::<unsafe extern "C" fn(i32) -> i32>(&fixture.native, "guest_uses_pointer")
    };
    let array = unsafe {
        native_function::<unsafe extern "C" fn(i32) -> i32>(&fixture.native, "guest_uses_array")
    };
    let round_trip = unsafe {
        native_function::<unsafe extern "C" fn(i32) -> i32>(&fixture.native, "guest_round_trip")
    };
    let widen = unsafe {
        native_function::<unsafe extern "C" fn(i32) -> i64>(&fixture.native, "guest_uses_widen")
    };
    let foreign_pointer = unsafe {
        native_function::<unsafe extern "C" fn(i32) -> i32>(
            &fixture.native,
            "guest_uses_foreign_pointer",
        )
    };
    let round_trip_pointer = unsafe {
        native_function::<unsafe extern "C" fn(i32) -> i32>(
            &fixture.native,
            "guest_round_trip_pointer",
        )
    };

    for (variant, module) in &mut fixture.transpiled {
        for input in INTEGER_INPUTS {
            for (name, expected) in [
                ("guest_uses_scale", unsafe { scale(input) }),
                ("guest_uses_pointer", unsafe { pointer(input) }),
                ("guest_uses_array", unsafe { array(input) }),
                ("guest_round_trip", unsafe { round_trip(input) }),
                ("guest_uses_foreign_pointer", unsafe {
                    foreign_pointer(input)
                }),
                ("guest_round_trip_pointer", unsafe {
                    round_trip_pointer(input)
                }),
            ] {
                assert_eq!(
                    module.call::<(i32,), i32>(name, (input,)),
                    expected,
                    "{name}({input}) disagreed with native in {variant}"
                );
            }
            assert_eq!(
                module.call::<(i32,), i64>("guest_uses_widen", (input,)),
                unsafe { widen(input) },
                "guest_uses_widen({input}) disagreed with native in {variant}"
            );
        }
    }
}

/// A foreign wasm function calling *into* the transpiled half, through the
/// typed host-entry wrapper its declaration gives it. Nothing generated
/// bridges this direction — a typed wrapper is already an ordinary wasm
/// function — so what is under test is whether the wrapper really is one.
#[test]
fn foreign_wasm_calling_into_the_guest_matches_native() {
    let mut fixture = fixture();
    let doubled = unsafe {
        native_function::<unsafe extern "C" fn(i32) -> i32>(&fixture.native, "guest_double")
    };

    for (variant, module) in &mut fixture.transpiled {
        for input in INTEGER_INPUTS {
            assert_eq!(
                module.call::<(i32,), i32>("guest_double", (input,)),
                unsafe { doubled(input) },
                "guest_double({input}) disagreed with native in {variant}"
            );
        }
    }
}

/// Floating point crosses in both widths, compared as raw bits.
///
/// `float` is the interesting one: it occupies the low *32* bits of an XMM
/// register, so every marshalling step has to move exactly those bits rather
/// than convert between widths. A conversion instead of a reinterpretation
/// would pass for whole numbers and fail here.
#[test]
fn float_calls_across_the_boundary_match_native() {
    let mut fixture = fixture();
    let blend = unsafe {
        native_function::<unsafe extern "C" fn(f64, f64) -> f64>(
            &fixture.native,
            "guest_uses_blend",
        )
    };
    let narrow = unsafe {
        native_function::<unsafe extern "C" fn(f32) -> f32>(&fixture.native, "guest_uses_narrow")
    };

    let doubles: [f64; 7] = [0.0, -0.0, 1.5, -2.25, 1e300, 1e-300, 0.1];
    let singles: [f32; 7] = [0.0, -0.0, 1.5, -2.25, 1e30, 1e-30, 0.1];

    for (variant, module) in &mut fixture.transpiled {
        for &first in &doubles {
            for &second in &doubles {
                let expected = unsafe { blend(first, second) };
                let observed = module.call::<(f64, f64), f64>("guest_uses_blend", (first, second));
                assert_eq!(
                    observed.to_bits(),
                    expected.to_bits(),
                    "guest_uses_blend({first}, {second}) disagreed with native in {variant}"
                );
            }
        }
        for &value in &singles {
            let expected = unsafe { narrow(value) };
            let observed = module.call::<(f32,), f32>("guest_uses_narrow", (value,));
            assert_eq!(
                observed.to_bits(),
                expected.to_bits(),
                "guest_uses_narrow({value}) disagreed with native in {variant}"
            );
        }
    }
}

/// A mixed argument list, which is where the two register files being counted
/// separately stops being a detail: the `f32` here is the *second* SSE
/// argument slot only because the `f64` before it took the first, while the
/// `i64` after it takes the second integer slot regardless.
#[test]
fn mixed_argument_lists_match_native() {
    let mut fixture = fixture();
    let mixed = unsafe {
        native_function::<unsafe extern "C" fn(i32, f64, f32, i64) -> i32>(
            &fixture.native,
            "guest_uses_mixed",
        )
    };

    let cases: [(i32, f64, f32, i64); 5] = [
        (0, 0.0, 0.0, 0),
        (1, 2.5, 3.5, 4),
        (-7, -2.5, 0.25, 1000),
        (100, 1.75, -8.5, -12345),
        (-1, 0.5, 0.5, 96),
    ];

    for (variant, module) in &mut fixture.transpiled {
        for &(first, second, third, fourth) in &cases {
            assert_eq!(
                module.call::<(i32, f64, f32, i64), i32>(
                    "guest_uses_mixed",
                    (first, second, third, fourth)
                ),
                unsafe { mixed(first, second, third, fourth) },
                "guest_uses_mixed({first}, {second}, {third}, {fourth}) disagreed \
                 with native in {variant}"
            );
        }
    }
}

/// What happens when a foreign signature is inferred and inferred *wrong*.
///
/// Call-site inference cannot recover the arity of a function whose arguments
/// are all passed straight through: `guest_uses_scale` compiles at `-O2` to
/// `call foreign_scale; add $1,%eax; ret`, which does not mention rdi at all.
/// Inference therefore says `foreign_scale()` — no arguments — and a thunk
/// built from that makes a call the far side will not recognise.
///
/// That is the whole safety argument made concrete. The mistake is not one
/// the analysis could have avoided; what matters is that it cannot become a
/// program that runs and returns the wrong answer. It has to stop the build,
/// and this is where that is checked rather than asserted.
///
/// Only `foreign_scale` is left wrong. Everything else takes its real type,
/// so that what the linker objects to is unambiguous — a link that failed for
/// three reasons at once would not show that it failed for this one.
#[test]
fn a_mis_inferred_foreign_signature_stops_the_link() {
    use support::{
        ALL_CODE_MODELS, ALL_COMPILERS, WorkingDirectory, compile_corpus_object_with,
        compile_foreign_wasm_object, foreign_wasm_signatures, transpile_object_inferring,
        try_link_wasm,
    };
    use zaqaru::abi::SignatureTable;

    let workspace = WorkingDirectory::new("interop-misinferred");
    let guest = compile_corpus_object_with(
        &workspace,
        GUEST_SOURCES[0],
        ALL_COMPILERS[0],
        ALL_CODE_MODELS[0],
        "-O2",
    );

    // What call sites alone can say, which for a pass-through is nothing.
    let bytes = std::fs::read(&guest).expect("read native object");
    let parsed = zaqaru::reader::ObjectFile::parse(&bytes).expect("parse native object");
    let nothing = SignatureTable::new();
    let inferred = zaqaru::thunks::foreign_signatures(&nothing, &nothing, &[parsed])
        .expect("settle the foreign signatures");
    let mistaken = inferred
        .get("foreign_scale")
        .expect("call sites do say *something* about foreign_scale")
        .clone();
    assert!(
        mistaken.parameters.is_empty(),
        "this test is only meaningful while inference gets `foreign_scale` \
         wrong; it now says {}, so the mis-inference has to be staged some \
         other way",
        mistaken.render("foreign_scale")
    );

    let mut signatures = foreign_wasm_signatures(FOREIGN_SOURCES.as_slice());
    signatures.insert("foreign_scale", mistaken);

    let bytes = std::fs::read(&guest).expect("read native object");
    let parsed = zaqaru::reader::ObjectFile::parse(&bytes).expect("parse native object");
    let names = zaqaru::thunks::foreign_functions(&[parsed]).expect("classify foreign functions");
    let thunks = workspace.write(
        "thunks.wasm.o",
        zaqaru::thunks::build_thunk_object(&names, &signatures).expect("build the thunk object"),
    );

    let transpiled = workspace.path().join("guest.wasm.o");
    transpile_object_inferring(
        &guest,
        &transpiled,
        zaqaru::structurer::Mode::Structured,
        &signatures,
    );

    let text = std::fs::read_to_string(support::corpus_source(FOREIGN_SOURCES[0]))
        .expect("read the foreign corpus source");
    let foreign = compile_foreign_wasm_object(&workspace, "misinferred_foreign", &text);

    let linked = workspace.path().join("linked.wasm");
    let outcome = try_link_wasm(
        &[transpiled, thunks, foreign],
        &linked,
        &["--fatal-warnings"],
    );
    assert!(
        !outcome.succeeded,
        "a thunk built from a wrong signature linked cleanly, so the mistake \\
         would have reached run time:\n{}",
        outcome.report()
    );
    assert!(
        outcome.mentions("foreign_scale"),
        "the link failed without naming the symbol whose signature was wrong, \\
         which is what a person would need in order to fix it:\n{}",
        outcome.report()
    );
}

/// How many times to cross the boundary when checking that the stack comes
/// back. Enough to walk out of a 64 KiB stack region if each crossing leaks
/// even a few dozen bytes.
const CROSSINGS: usize = 3000;

/// The guest stack has to be *balanced* across a foreign call, not merely
/// correct once.
///
/// A thunk that hands `__stack_pointer` down to a foreign callee and never
/// puts it back still gets the right answer the first time. The damage is
/// cumulative: the next host entry starts its guest stack from wherever the
/// last call left the pointer, so every crossing begins a little lower than
/// the one before, and the descent eventually reaches the data segments —
/// silently, because nothing in wasm objects to a store below the stack.
///
/// No single call can see that, which is exactly why this test exists and why
/// it repeats. It is the difference between testing that the sync happens and
/// testing that it is undone.
#[test]
fn repeated_crossings_do_not_walk_the_stack_down() {
    let mut fixture = fixture();
    let native = unsafe {
        native_function::<unsafe extern "C" fn(i32) -> i32>(&fixture.native, "guest_round_trip")
    };
    let expected = unsafe { native(5) };

    for (variant, module) in &mut fixture.transpiled {
        for crossing in 0..CROSSINGS {
            let observed = module.call::<(i32,), i32>("guest_round_trip", (5,));
            assert_eq!(
                observed, expected,
                "guest_round_trip(5) gave {observed} on crossing {crossing} of \
                 {CROSSINGS} in {variant}, having given {expected} before — the \
                 stack is not coming back from a foreign call"
            );
        }
    }
}
