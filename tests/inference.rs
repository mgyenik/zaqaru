//! Grading signature inference against ground truth.
//!
//! Milestone 3 of [the interop plan](../docs/archive/interop-plan.md). The transpiler
//! needs no calling-convention knowledge and interop needs nothing else, and
//! since the binaries this project aims at are stripped, that knowledge has to
//! come out of the machine code. This is where the recovering is checked
//! against what the source actually said.
//!
//! `signatures.expected` holds the true C signature of every function in
//! `signatures.c`, and inference has to produce exactly that — at every
//! optimisation level, from both compilers, in both code models. Twenty
//! configurations of the same twenty functions, and no allowance for "close
//! enough": a signature at a boundary is either right or it is a link error.
//!
//! Two functions are deliberately excluded, and the reasons are below rather
//! than in the data file, because a limitation recorded as an expected value
//! stops looking like a limitation.

mod support;

use std::collections::BTreeMap;

use support::{
    ALL_CODE_MODELS, ALL_COMPILERS, ALL_OPTIMISATION_LEVELS, WorkingDirectory,
    compile_corpus_object_with, corpus_signatures, foreign_wasm_signatures,
};
use zaqaru::abi::{Signature, SignatureTable};

const SOURCE: &str = "signatures.c";
const EXPECTED: &str = "signatures.expected";

/// Functions whose inferred signature is not the C one, with why.
///
/// Both are limits of what a callee's own code can say, not gaps in the
/// analysis — which is why they are named here with reasons instead of being
/// pinned to whatever inference currently produces. If either starts matching
/// its C signature at every configuration, this test fails and the entry
/// should be deleted; that is the intended way to find out.
const BEYOND_REACH: [(&str, &str); 2] = [
    (
        "ignores_second",
        "a trailing parameter the body never reads is invisible: SysV assigns \
         argument registers in order and records nothing about how many were \
         assigned, so `f(int, int)` ignoring its second is byte-identical to \
         `f(int)`",
    ),
    (
        "narrow",
        "the function leaves all sixty-four bits of a division in rax and only \
         the low thirty-two are the `int` result. Which half matters is the \
         caller's knowledge, and at two configurations no explicit narrowing \
         is emitted to reveal it",
    ),
];

fn infer_signatures(object: &std::path::Path) -> BTreeMap<String, Option<Signature>> {
    let bytes = std::fs::read(object).expect("read native object");
    let parsed = zaqaru::reader::ObjectFile::parse(&bytes).expect("parse native object");
    let inference =
        zaqaru::abi::infer::infer(&parsed, &SignatureTable::new()).expect("run inference");
    inference
        .functions
        .iter()
        .filter(|function| function.is_global)
        .map(|function| (function.name.clone(), function.signature.clone()))
        .collect()
}

/// Every global function must be either graded or explicitly excused.
///
/// Without this, a function could quietly fall out of `signatures.expected`
/// and take its coverage with it — the corpus would still pass while testing
/// less than it claims.
#[test]
fn every_function_is_either_graded_or_excused() {
    let workspace = WorkingDirectory::new("inference-coverage");
    let expected = corpus_signatures(EXPECTED);
    let object = compile_corpus_object_with(
        &workspace,
        SOURCE,
        ALL_COMPILERS[0],
        ALL_CODE_MODELS[0],
        "-O1",
    );

    for name in infer_signatures(&object).keys() {
        let graded = expected.get(name).is_some();
        let excused = BEYOND_REACH.iter().any(|(excused, _)| excused == name);
        assert!(
            graded ^ excused,
            "`{name}` is {} in {EXPECTED} and {} in the excused list; every \
             global function must be in exactly one",
            if graded { "present" } else { "absent" },
            if excused { "present" } else { "absent" },
        );
    }

    for (name, _) in BEYOND_REACH {
        assert!(
            infer_signatures(&object).contains_key(name),
            "`{name}` is excused from grading but no longer exists in {SOURCE}"
        );
    }
}

/// The grading itself.
#[test]
fn inferred_signatures_match_the_source() {
    let workspace = WorkingDirectory::new("inference-grading");
    let expected = corpus_signatures(EXPECTED);
    let mut mismatches: Vec<String> = Vec::new();
    let mut configurations = 0usize;

    for compiler in ALL_COMPILERS {
        for model in ALL_CODE_MODELS {
            for optimisation in ALL_OPTIMISATION_LEVELS {
                let object =
                    compile_corpus_object_with(&workspace, SOURCE, compiler, model, optimisation);
                let inferred = infer_signatures(&object);
                configurations += 1;
                let where_ = format!("{}/{}{optimisation}", compiler.label(), model.label());

                for (name, truth) in expected.iter() {
                    let Some(found) = inferred.get(name) else {
                        mismatches.push(format!("[{where_}] `{name}` was not inferred at all"));
                        continue;
                    };
                    match found {
                        Some(found) if found == truth => {}
                        Some(found) => mismatches.push(format!(
                            "[{where_}] {} — expected {}",
                            found.render(name),
                            truth.render(name)
                        )),
                        None => mismatches.push(format!(
                            "[{where_}] `{name}` produced no signature; expected {}",
                            truth.render(name)
                        )),
                    }
                }
            }
        }
    }

    assert!(
        mismatches.is_empty(),
        "{} of {} graded signatures across {configurations} configurations \
         disagreed with the source:\n{}",
        mismatches.len(),
        expected.len() * configurations,
        mismatches.join("\n")
    );
}

/// A signature that spans both register files has an unrecoverable parameter
/// order, and inference has to say so rather than pick one.
///
/// `f(int, double)` and `f(double, int)` produce byte-identical register
/// assignments — SysV fills the two files independently and nothing records
/// how they interleaved. Guessing would be a wrong signature, which at a
/// boundary is worse than none: no signature keeps the uniform wrapper, which
/// always works.
#[test]
fn a_signature_spanning_both_register_files_is_refused() {
    let workspace = WorkingDirectory::new("inference-mixed");
    // Seeded the way the real path is: with what the far side's own object
    // says about itself, so that the only thing left unknown is the one thing
    // no object records — which order the two register files interleaved in.
    let mut signatures = foreign_wasm_signatures(&["interop_foreign.c"]);
    for (name, signature) in corpus_signatures("interop.signatures").iter() {
        signatures.insert(name.clone(), signature.clone());
    }

    for compiler in ALL_COMPILERS {
        for optimisation in ALL_OPTIMISATION_LEVELS {
            let object = compile_corpus_object_with(
                &workspace,
                "interop_guest.c",
                compiler,
                ALL_CODE_MODELS[0],
                optimisation,
            );
            let bytes = std::fs::read(&object).expect("read native object");
            let parsed = zaqaru::reader::ObjectFile::parse(&bytes).expect("parse");
            let inference = zaqaru::abi::infer::infer(&parsed, &signatures).expect("infer");

            let mixed = inference
                .functions
                .iter()
                .find(|function| function.name == "guest_uses_mixed")
                .expect("guest_uses_mixed is in the corpus");
            assert!(
                mixed.signature.is_none(),
                "[{}{optimisation}] guest_uses_mixed was given the signature {:?}, \
                 but its parameter order cannot be recovered — inference must \
                 refuse rather than choose",
                compiler.label(),
                mixed.signature
            );
            assert!(
                mixed
                    .obstacle
                    .as_ref()
                    .is_some_and(|reason| reason.contains("not recoverable")),
                "guest_uses_mixed was refused without saying why"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Milestone 4: what the *callers* of a function say about it.
// ---------------------------------------------------------------------------

fn call_sites_of(source: &str, label: &str) -> zaqaru::abi::infer::ForeignSignatures {
    let workspace = WorkingDirectory::new(label);
    let object = compile_corpus_object_with(
        &workspace,
        source,
        ALL_COMPILERS[0],
        ALL_CODE_MODELS[0],
        "-O1",
    );
    let bytes = std::fs::read(&object).expect("read native object");
    let parsed = zaqaru::reader::ObjectFile::parse(&bytes).expect("parse native object");
    let inference =
        zaqaru::abi::infer::infer(&parsed, &SignatureTable::new()).expect("run inference");
    zaqaru::abi::infer::merge_call_sites(&inference.call_sites)
}

/// Call sites that agree produce a signature; the agreement is the evidence.
#[test]
fn agreeing_call_sites_settle_a_signature() {
    let merged = call_sites_of("call_sites.s", "call-sites-agree");
    assert_eq!(
        merged.signatures.get("agreed").map(|s| s.render("agreed")),
        Some("agreed(i32, i32)".to_string()),
        "two call sites passing the same pair should have settled it; \
         refusals were {:?}",
        merged.refusals
    );
}

/// Call sites that disagree are refused, and the refusal names the callers.
///
/// A majority vote would be the wrong instinct: two sites disagreeing means
/// the evidence is being misread somewhere, and picking the popular reading
/// would bury that under a signature that looks authoritative.
#[test]
fn disagreeing_call_sites_are_refused_by_name() {
    let merged = call_sites_of("call_sites.s", "call-sites-disagree");
    assert!(
        merged.signatures.get("disputed").is_none(),
        "a symbol its callers disagree about was given a signature anyway"
    );
    let reason = merged
        .refusals
        .iter()
        .find(|(name, _)| name == "disputed")
        .map(|(_, reason)| reason.clone())
        .expect("`disputed` should have been refused");
    for caller in ["passes_one", "passes_two"] {
        assert!(
            reason.contains(caller),
            "the refusal for `disputed` does not name `{caller}`, so the \
             declaration that resolves it would have to be written blind:\n{reason}"
        );
    }
}

/// The variadic protocol is detected and refused rather than guessed.
///
/// `al` set immediately before a call is SysV saying how many vector
/// registers were used, which only happens for a variadic callee — and a
/// variadic callee's arguments do not all travel in registers, so no thunk
/// can carry them.
#[test]
fn a_variadic_call_is_refused() {
    let merged = call_sites_of("call_sites.s", "call-sites-variadic");
    assert!(
        merged.signatures.get("formatted").is_none(),
        "a variadic callee was given a fixed signature"
    );
    let reason = merged
        .refusals
        .iter()
        .find(|(name, _)| name == "formatted")
        .map(|(_, reason)| reason.clone())
        .expect("`formatted` should have been refused");
    assert!(
        reason.contains("variadic"),
        "the refusal does not say what the problem is:\n{reason}"
    );
}
