//! Milestone 1: the reader and the lifter, checked against real `gcc` output.
//!
//! The invariant under test is that no relocated operand survives as a
//! number. Each assertion resolves a lifted operand back to *section +
//! offset* and compares it against where the referenced object actually
//! lives, which is what makes a wrong program-counter addend visible here
//! rather than three milestones later.

mod support;

use support::{WorkingDirectory, compile_corpus_object};
use zaqaru::lifter::{self, LiftedFunction, SymbolReference};
use zaqaru::reader::ObjectFile;

struct LiftedObject {
    object: ObjectFile,
    functions: Vec<LiftedFunction>,
}

fn lift_corpus(name: &str) -> LiftedObject {
    let workspace = WorkingDirectory::new("lifting");
    let object_path = compile_corpus_object(&workspace, name);
    let bytes = std::fs::read(&object_path).expect("read compiled object");
    let object = ObjectFile::parse(&bytes).expect("parse object");
    let functions = lifter::lift_object(&object).expect("lift object");
    LiftedObject { object, functions }
}

impl LiftedObject {
    fn function(&self, name: &str) -> &LiftedFunction {
        self.functions
            .iter()
            .find(|function| function.name == name)
            .unwrap_or_else(|| panic!("no function `{name}` in the lifted object"))
    }

    /// Every symbolic operand in a function, in instruction order.
    fn references(&self, name: &str) -> Vec<SymbolReference> {
        self.function(name)
            .instructions
            .iter()
            .flat_map(|lifted| [lifted.displacement, lifted.immediate])
            .flatten()
            .collect()
    }

    /// Resolves a reference to `(section name, byte offset in section)`.
    fn locate(&self, reference: SymbolReference) -> (String, i64) {
        let (section, offset) = self
            .object
            .resolve(reference.symbol, reference.addend)
            .expect("reference to a symbol defined in this object");
        (self.object.sections[section].name.clone(), offset)
    }

    /// Where a named symbol is defined.
    fn location_of(&self, name: &str) -> (String, i64) {
        let symbol = self
            .object
            .symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| panic!("no symbol `{name}`"));
        let section = symbol.section.expect("symbol is defined in a section");
        (
            self.object.sections[section].name.clone(),
            symbol.offset as i64,
        )
    }
}

#[test]
fn functions_are_found_with_their_extents() {
    let lifted = lift_corpus("two_functions.c");
    let names: Vec<&str> = lifted
        .functions
        .iter()
        .map(|function| function.name.as_str())
        .collect();
    assert_eq!(names, ["scale", "scale_twice"]);

    for function in &lifted.functions {
        let decoded: u64 = function
            .instructions
            .iter()
            .map(zaqaru::lifter::LiftedInstruction::length)
            .sum();
        assert_eq!(
            decoded, function.size,
            "decoding `{}` did not cover the whole function",
            function.name
        );
    }
}

#[test]
fn a_direct_call_resolves_to_the_callee_symbol() {
    let lifted = lift_corpus("two_functions.c");
    let references = lifted.references("scale_twice");
    assert_eq!(
        references.len(),
        2,
        "expected both calls to carry a symbolic target"
    );
    for reference in references {
        assert_eq!(
            lifted.locate(reference),
            lifted.location_of("scale"),
            "a call did not land exactly on the callee's entry point"
        );
    }
}

#[test]
fn a_data_reference_resolves_to_the_object_it_names() {
    let lifted = lift_corpus("two_functions.c");
    let references = lifted.references("scale");
    assert_eq!(references.len(), 1);
    assert_eq!(
        lifted.locate(references[0]),
        lifted.location_of("weights"),
        "the base address of `weights` was lifted to the wrong offset"
    );
}

/// The program-counter-relative addend is the one piece of arithmetic that
/// fails silently: a mishandled `-4` reads a neighbouring element instead of
/// crashing. These references must land on exact element offsets.
#[test]
fn data_references_at_non_zero_offsets_keep_their_addend() {
    let lifted = lift_corpus("data_addend.c");
    let (table_section, table_offset) = lifted.location_of("table");

    let third = lifted.references("third_element");
    assert_eq!(third.len(), 1);
    assert_eq!(
        lifted.locate(third[0]),
        (table_section.clone(), table_offset + 2 * 4),
        "`table[2]` did not lift to the third element"
    );

    let sum: Vec<(String, i64)> = lifted
        .references("sum_of_two")
        .into_iter()
        .map(|reference| lifted.locate(reference))
        .collect();
    assert!(
        sum.contains(&(table_section.clone(), table_offset + 4))
            && sum.contains(&(table_section.clone(), table_offset + 5 * 4)),
        "`table[1] + table[5]` lifted to {sum:?}, expected offsets {} and {}",
        table_offset + 4,
        table_offset + 5 * 4
    );
}
