//! The optimisation sweep: every corpus source, both compilers, both code
//! models, every optimisation level, both control-flow translations.
//!
//! This is the breadth tier. The differential tests answer "does the
//! translation compute the right thing"; this one answers "is there anything
//! in the corpus the transpiler refuses", which is a different question and
//! one that only gets interesting at the optimisation levels the differential
//! tests would otherwise be slow to cover. Every instruction the transpiler
//! does not model is a hard error naming it, so a sweep with no failures is a
//! statement about coverage rather than about luck.
//!
//! It transpiles and validates; it does not link. Linker acceptance across
//! the same matrix is what the differential fixtures do — each of them builds
//! and links every one of its sources at every configuration — so repeating
//! it here would buy nothing.
//!
//! A failure names every configuration that failed, not just the first,
//! because the useful question after a change is "what does this not handle
//! yet" rather than "what stopped it first".

mod support;

use support::{
    ALL_CODE_MODELS, ALL_MODES, ALL_OPTIMISATION_LEVELS, Compiler, WorkingDirectory,
    compile_corpus_object_with, corpus_sources, try_transpile_object,
};

/// The bar the MVP set and this restores: sixteen sources across two code
/// models, five optimisation levels and two control-flow translations. The
/// corpus has grown since, and a second compiler has joined, so the real
/// figure is well past it — but the floor is worth asserting, because a sweep
/// that silently stopped enumerating would otherwise pass.
const CONFIGURATION_FLOOR: usize = 320;

fn sweep(compiler: Compiler) {
    let workspace = WorkingDirectory::new(&format!("sweep-{}", compiler.label()));
    let mut failures: Vec<String> = Vec::new();
    let mut configurations = 0usize;

    for source in corpus_sources() {
        for model in ALL_CODE_MODELS {
            for optimisation in ALL_OPTIMISATION_LEVELS {
                let object =
                    compile_corpus_object_with(&workspace, &source, compiler, model, optimisation);
                for mode in ALL_MODES {
                    configurations += 1;
                    if let Err(error) = try_transpile_object(&object, mode) {
                        failures.push(format!(
                            "{source} [{}/{}{optimisation}/{mode:?}]: {error:?}",
                            compiler.label(),
                            model.label()
                        ));
                    }
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {configurations} configurations failed to transpile:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        configurations >= CONFIGURATION_FLOOR,
        "the sweep covered only {configurations} configurations, below the \
         {CONFIGURATION_FLOOR} the corpus should reach"
    );
}

#[test]
fn every_gcc_configuration_transpiles() {
    sweep(Compiler::Gcc);
}

#[test]
fn every_clang_configuration_transpiles() {
    sweep(Compiler::Clang);
}
