//! What the transpiler actually emits for function pointers and `switch`
//! dispatches.
//!
//! The differential tests prove the programs compute the right answers, but
//! not *how*. A jump table that quietly failed to be recognised, or a
//! function pointer that never reached the indirect function table, would
//! show up here and nowhere else.

mod support;

use support::linking_format::{SymbolTarget, read_linking_metadata, read_sections};
use support::{ALL_CODE_MODELS, CodeModel, WorkingDirectory, compile_corpus_object_as};
use zaqaru::lifter;
use zaqaru::reader::ObjectFile;

fn lift(source: &str, model: CodeModel) -> (ObjectFile, Vec<zaqaru::lifter::LiftedFunction>) {
    let workspace = WorkingDirectory::new("indirect");
    let object_path = compile_corpus_object_as(&workspace, source, model);
    let bytes = std::fs::read(&object_path).expect("read compiled object");
    let object = ObjectFile::parse(&bytes).expect("parse object");
    let functions = lifter::lift_object(&object).expect("lift object");
    (object, functions)
}

fn transpile(source: &str, model: CodeModel) -> Vec<u8> {
    let workspace = WorkingDirectory::new("indirect-emit");
    let object_path = compile_corpus_object_as(&workspace, source, model);
    let output = workspace.path().join("out.wasm.o");
    support::transpile_object(&object_path, &output, zaqaru::structurer::Mode::default());
    std::fs::read(&output).expect("read transpiled object")
}

/// Every dense `switch` in the corpus must come back as a recovered table, in
/// both code models — which emit different dispatch shapes — with the arm
/// count the source implies.
#[test]
fn dense_switches_are_recovered_as_jump_tables() {
    for model in ALL_CODE_MODELS {
        let (_, functions) = lift("switch_dispatch.c", model);

        // How many arms a table has depends on where the compiler put its
        // bounds check, so what is asserted is the shape rather than a count:
        // one table per dispatch, every arm inside the function, and enough
        // arms to be the switch it came from.
        for (name, dispatches) in [
            ("classify", 1),
            ("accumulate", 1),
            ("fold", 1),
            ("from_byte", 1),
            ("nested", 1),
            ("twice", 2),
        ] {
            let function = functions
                .iter()
                .find(|function| function.name == name)
                .unwrap_or_else(|| panic!("`{name}` is in the corpus"));
            assert_eq!(
                function.jump_tables.len(),
                dispatches,
                "`{name}` [{}] recovered {} dispatches, expected {dispatches}",
                model.label(),
                function.jump_tables.len()
            );
            for table in function.jump_tables.values() {
                assert!(
                    table.targets.len() >= 4,
                    "`{name}` [{}] recovered only {} arms",
                    model.label(),
                    table.targets.len()
                );
                assert!(
                    table
                        .targets
                        .iter()
                        .all(|target| function.contains(*target)),
                    "`{name}` [{}] has a jump-table arm outside the function",
                    model.label()
                );
            }
        }

        // The invariant that matters: tables abut one another in `.rodata`,
        // and reading one as another's yields wrong targets rather than
        // surplus ones, so no two may overlap.
        let mut spans: Vec<(usize, u64, u64, String)> = Vec::new();
        for function in &functions {
            for table in function.jump_tables.values() {
                spans.push((
                    table.table_section,
                    table.table_offset,
                    table.table_offset + table.byte_length(),
                    function.name.clone(),
                ));
            }
        }
        spans.sort_by_key(|(section, start, _, _)| (*section, *start));
        for pair in spans.windows(2) {
            let (left_section, _, left_end, left_name) = &pair[0];
            let (right_section, right_start, _, right_name) = &pair[1];
            assert!(
                left_section != right_section || left_end <= right_start,
                "[{}] the table in `{left_name}` runs past {right_start:#x} into \
                 the one in `{right_name}`",
                model.label()
            );
        }
    }
}

/// A jump table's entries name code, which has no address in wasm. None of
/// their original relocations may survive into the emitted object, and the
/// table itself must have become a data symbol the dispatch can subtract.
#[test]
fn jump_table_entries_are_consumed_not_translated() {
    for model in ALL_CODE_MODELS {
        let bytes = transpile("switch_dispatch.c", model);
        let metadata = read_linking_metadata(&bytes);

        let tables: Vec<&support::linking_format::Symbol> = metadata
            .symbols
            .iter()
            .filter(|symbol| {
                symbol
                    .name
                    .as_deref()
                    .is_some_and(|name| name.contains(".switch."))
            })
            .collect();
        assert!(
            tables.len() >= 7,
            "[{}] only {} jump tables became data symbols",
            model.label(),
            tables.len()
        );
        for table in &tables {
            assert!(
                table.is_local(),
                "a jump table is private to its object, but {:?} is not local",
                table.name
            );
            assert!(matches!(table.target, SymbolTarget::Data(Some(_))));
        }
    }
}

/// Taking a function's address has to reach the indirect function table:
/// the object imports one, places the address-taken functions in it, and
/// refers to their slots with table-index relocations.
#[test]
fn function_addresses_reach_the_indirect_function_table() {
    const TABLE_INDEX_SLEB: u8 = 1;
    const TABLE_INDEX_I32: u8 = 2;
    const TYPE_INDEX_LEB: u8 = 6;

    for model in ALL_CODE_MODELS {
        let bytes = transpile("function_pointers.c", model);
        let metadata = read_linking_metadata(&bytes);

        let sections = read_sections(&bytes);
        assert!(
            sections.iter().any(|section| section.id == 9),
            "[{}] no element section: nothing was placed in the table",
            model.label()
        );

        let code = metadata
            .relocations_for("CODE")
            .expect("the object has code relocations");
        assert!(
            code.entries
                .iter()
                .any(|entry| entry.kind == TABLE_INDEX_SLEB),
            "[{}] no function address is computed into a table slot",
            model.label()
        );
        assert!(
            code.entries
                .iter()
                .any(|entry| entry.kind == TYPE_INDEX_LEB),
            "[{}] nothing calls indirectly",
            model.label()
        );

        let data = metadata
            .relocations_for("DATA")
            .expect("the vtables put function pointers in data");
        assert!(
            data.entries
                .iter()
                .any(|entry| entry.kind == TABLE_INDEX_I32),
            "[{}] no function pointer was stored in data",
            model.label()
        );
    }
}

/// An object that only *receives* function pointers still needs the table to
/// call through, even though it puts nothing in it.
#[test]
fn calling_indirectly_imports_the_table_without_filling_it() {
    let bytes = transpile("cross_object_caller.c", CodeModel::PositionIndependent);
    let sections = read_sections(&bytes);
    assert!(
        !sections.iter().any(|section| section.id == 9),
        "the caller defines no function pointers, so it needs no element section"
    );

    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("__indirect_function_table"),
        "an object that calls indirectly must import the table"
    );
}
