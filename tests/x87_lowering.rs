//! What an x87 instruction becomes, checked in the emitted module rather
//! than through its answer.
//!
//! The differential corpus proves the arithmetic agrees with the hardware.
//! It cannot see either property this file is about, because a translation
//! that got them wrong would still compute the right number in the fixture
//! that asked:
//!
//! 1. **The helpers are typed imports.** The `x87` crate and the translator
//!    each hold half of a contract, and a disagreement about a signature is
//!    meant to be a link error rather than a wrong answer. That only works
//!    if the import carries the type the crate defines.
//! 2. **A helper call carries no ceremony.** The helpers cannot name the
//!    register-file globals, so promoted state stays valid across one and
//!    the machine file must *not* be flushed around it. A translation that
//!    flushed anyway would be correct, and would spend a dozen instructions
//!    on every x87 instruction in the program.

mod support;

use std::collections::HashMap;

use support::{WorkingDirectory, compile_corpus_object, transpile_object};

/// One transpiled module, in the printed form these assertions read.
struct Printed {
    lines: Vec<String>,
}

impl Printed {
    fn build(source: &str, mode: zaqaru::structurer::Mode) -> Self {
        let workspace = WorkingDirectory::new("x87-lowering");
        let object = compile_corpus_object(&workspace, source);
        let wasm = workspace.path().join("out.wasm.o");
        transpile_object(&object, &wasm, mode);
        let bytes = std::fs::read(&wasm).expect("read the transpiled object");
        let text = wasmprinter::print_bytes(&bytes).expect("print the transpiled object");
        Self {
            lines: text.lines().map(|line| line.trim().to_string()).collect(),
        }
    }

    /// Each declared type, by index, as the printer spells its signature.
    fn types(&self) -> HashMap<u32, String> {
        self.lines
            .iter()
            .filter_map(|line| {
                let rest = line.strip_prefix("(type (;")?;
                let (index, rest) = rest.split_once(";) (func")?;
                // Two closing brackets belong to the `(func` and the
                // `(type`; the rest of them belong to the signature.
                let signature = rest.trim().strip_suffix("))")?.trim();
                Some((index.parse().ok()?, signature.to_string()))
            })
            .collect()
    }

    /// The index of the imported global the linker's shadow stack lives in.
    ///
    /// It is the one global a helper call is *supposed* to write: the helper
    /// is an ordinary wasm callee and must not allocate its frame from the
    /// guest's stack pointer, so the call switches this and switches it
    /// back. Nothing else about the machine may move around a call.
    fn linker_stack_pointer(&self) -> u32 {
        self.lines
            .iter()
            .find_map(|line| {
                let rest = line.strip_prefix(r#"(import "env" "__stack_pointer" (global (;"#)?;
                rest.split_once(";)")?.0.parse().ok()
            })
            .expect("the linker's stack pointer is imported by every module")
    }

    /// Each imported function's field name, paired with its index in the
    /// function index space and the index of its type.
    fn imports(&self) -> HashMap<String, (u32, u32)> {
        self.lines
            .iter()
            .filter_map(|line| {
                let rest = line.strip_prefix(r#"(import "env" ""#)?;
                let (field, rest) = rest.split_once('"')?;
                let rest = rest.strip_prefix(" (func (;")?;
                let (function, rest) = rest.split_once(";) (type ")?;
                let type_index = rest.trim_end_matches(')').trim();
                Some((
                    field.to_string(),
                    (function.parse().ok()?, type_index.parse().ok()?),
                ))
            })
            .collect()
    }
}

/// Every helper is an import from the environment, carrying the type the
/// crate defines.
#[test]
fn helpers_are_declared_as_typed_imports() {
    let printed = Printed::build("long_double.c", zaqaru::structurer::Mode::Structured);
    let imports = printed.imports();
    let types = printed.types();

    // The load and the store of a `double` through the stack, which every
    // long-double expression in the corpus begins and ends with, and the
    // arithmetic between them.
    for (field, expected) in [
        ("x87_fld64", "(param i64)"),
        ("x87_fst64", "(param i32) (result i64)"),
        ("x87_arith_sti", "(param i32 i32 i32 i32)"),
    ] {
        let (_, type_index) = imports
            .get(field)
            .unwrap_or_else(|| panic!("the module does not import `{field}`"));
        let signature = types
            .get(type_index)
            .unwrap_or_else(|| panic!("`{field}` names type {type_index}, which is not declared"));
        assert_eq!(
            signature, expected,
            "`{field}` is imported as `{signature}`, which is not the type \
             the `x87` crate defines"
        );
    }
}

/// Nothing but the stack switch stands between two adjacent x87
/// instructions.
///
/// Two helper calls with only argument traffic between them is the shape a
/// chain of x87 instructions has when the lowering is right. A flush writes
/// the register file back to its globals, so if one were being emitted
/// around helper calls it would land in exactly that gap — and the only
/// thing legitimately there is the linker stack pointer being moved off the
/// guest's stack and put back, because an x87 instruction's *operands* are
/// read and never written on the way in.
///
/// So both halves are asserted: no machine-file global is written, and the
/// stack pointer is.
///
/// Both control-flow translations are checked: promotion decides where the
/// machine file lives, and the two modes promote separately.
#[test]
fn nothing_is_flushed_between_two_x87_instructions() {
    for mode in [
        zaqaru::structurer::Mode::Structured,
        zaqaru::structurer::Mode::Dispatcher,
    ] {
        let printed = Printed::build("long_double.c", mode);
        let helpers: Vec<u32> = printed
            .imports()
            .iter()
            .filter(|(field, _)| field.starts_with("x87_"))
            .map(|(_, (index, _))| *index)
            .collect();
        assert!(!helpers.is_empty(), "{mode:?}: no helper was imported");

        let calls: Vec<usize> = printed
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                line.strip_prefix("call ")
                    .and_then(|index| index.parse::<u32>().ok())
                    .is_some_and(|index| helpers.contains(&index))
            })
            .map(|(index, _)| index)
            .collect();
        assert!(
            calls.len() > 1,
            "{mode:?}: fewer than two helper calls, so this asserted nothing"
        );

        let stack_pointer = printed.linker_stack_pointer();
        let mut adjacent = 0usize;
        for pair in calls.windows(2) {
            let (first, second) = (pair[0], pair[1]);
            // Only pairs close enough to be one chain of x87 instructions:
            // a wider gap is a different basic block, whose contents this
            // says nothing about.
            if second - first > 24 {
                continue;
            }
            adjacent += 1;
            let mut switched = false;
            for line in &printed.lines[first + 1..second] {
                let Some(global) = line.strip_prefix("global.set ") else {
                    continue;
                };
                if global.trim().parse::<u32>() == Ok(stack_pointer) {
                    switched = true;
                    continue;
                }
                panic!(
                    "{mode:?}: `{line}` sits between two x87 helper calls, \
                     which is the flush a helper call does not need"
                );
            }
            assert!(
                switched,
                "{mode:?}: the linker's stack pointer is not restored between \
                 two x87 helper calls, so a helper frame is being allocated \
                 from the guest's stack"
            );
        }
        assert!(
            adjacent > 0,
            "{mode:?}: no two helper calls were close enough to compare, so \
             this asserted nothing"
        );
    }
}
