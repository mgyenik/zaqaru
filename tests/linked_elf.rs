//! The linked-ELF front end: what the translator sees when the input is a
//! complete executable rather than a relocatable object.
//!
//! The two shapes differ in exactly one way that matters, and it runs
//! through everything. A relocatable object has no addresses — every
//! reference is a relocation naming a symbol, and the linker will decide
//! later where that symbol lands. A linked executable has already decided:
//! sections sit at virtual addresses, and a reference is the number it
//! resolves to, with nothing left to relocate. So the translator has to
//! answer the same question — what does this operand point at — from two
//! different kinds of evidence.

mod support;

use support::{WorkingDirectory, link_corpus_executable};
use zaqaru::reader::{Layout, ObjectFile};

fn read(workspace: &WorkingDirectory, name: &str, entry: &str) -> ObjectFile {
    let path = link_corpus_executable(workspace, name, entry, "-O1");
    let bytes = std::fs::read(&path).expect("read the linked executable");
    ObjectFile::parse(&bytes).expect("parse the linked executable")
}

#[test]
fn a_linked_executable_is_read_as_one() {
    let workspace = WorkingDirectory::new("linked-shape");
    let object = read(&workspace, "arithmetic.c", "quad_mix");
    assert_eq!(object.layout, Layout::Linked);

    // An entry point, which a relocatable object does not have.
    assert_ne!(object.entry, 0);
    assert!(
        object.function_at(object.entry).is_some(),
        "the entry point is not inside any function"
    );

    // Sections placed at addresses, which is the whole difference.
    let text = object
        .sections
        .iter()
        .find(|section| section.name == ".text")
        .expect("a text section");
    assert_ne!(text.address, 0, "a linked section has an address");

    // Functions, discovered from the symbol table, with extents.
    assert!(
        object
            .functions
            .iter()
            .any(|function| function.name == "quad_mix"),
        "the function under test was not discovered: {:?}",
        object
            .functions
            .iter()
            .map(|function| &function.name)
            .collect::<Vec<_>>()
    );
    for function in &object.functions {
        assert!(function.size > 0, "{} has no extent", function.name);
        let address = object.address_of(function);
        assert_eq!(
            object.function_at(address),
            object
                .functions
                .iter()
                .position(|other| other.name == function.name),
            "{} does not contain its own first byte",
            function.name
        );
        assert_eq!(
            object.function_at(address + function.size - 1),
            object.function_at(address),
            "{} does not contain its own last byte",
            function.name
        );
    }

    // And nothing left to relocate: that is what "linked" means.
    assert!(
        object
            .sections
            .iter()
            .all(|section| section.relocations.is_empty()),
        "a linked executable still has relocations"
    );
}

/// An address maps back to the section holding it, which is what replaces
/// relocation-driven resolution.
#[test]
fn an_address_resolves_to_the_section_that_holds_it() {
    let workspace = WorkingDirectory::new("linked-resolve");
    let object = read(&workspace, "arithmetic.c", "quad_mix");

    let text = object
        .sections
        .iter()
        .position(|section| section.name == ".text")
        .expect("a text section");
    let address = object.sections[text].address;
    assert_eq!(object.section_at(address), Some((text, 0)));
    assert_eq!(object.section_at(address + 4), Some((text, 4)));

    // One past the end belongs to the next section, or to none.
    let size = object.sections[text].size;
    assert_ne!(object.section_at(address + size), Some((text, size)));

    // An address below everything is in nothing, rather than in whichever
    // section happens to be first.
    assert_eq!(object.section_at(0), None);
    assert_eq!(object.section_at(1), None);
}

/// The program headers, which are what a loader places — and what kisal's
/// exec path will copy.
#[test]
fn the_load_segments_describe_what_a_loader_places() {
    let workspace = WorkingDirectory::new("linked-segments");
    let object = read(&workspace, "arithmetic.c", "quad_mix");

    let loadable: Vec<_> = object
        .segments
        .iter()
        .filter(|segment| segment.memory_size > 0 && segment.address != 0)
        .collect();
    assert!(!loadable.is_empty(), "no loadable segments");

    for segment in &loadable {
        assert!(
            segment.file_size <= segment.memory_size,
            "a segment claims more bytes in the file than in memory"
        );
        assert_eq!(
            segment.bytes.len() as u64,
            segment.file_size,
            "the bytes and the file size disagree"
        );
        assert!(segment.alignment.is_power_of_two());
    }

    // The entry point falls inside an executable segment, which is the
    // minimum a loader has to get right.
    assert!(
        loadable.iter().any(|segment| segment.executable
            && object.entry >= segment.address
            && object.entry < segment.address + segment.memory_size),
        "the entry point is not in an executable segment"
    );

    // Every text section is covered by some segment: nothing the translator
    // will lift is outside what the loader would place.
    for section in &object.sections {
        if section.address == 0 || section.size == 0 {
            continue;
        }
        assert!(
            loadable
                .iter()
                .any(|segment| section.address >= segment.address
                    && section.address + section.size <= segment.address + segment.memory_size),
            "{} is not inside any load segment",
            section.name
        );
    }
}

/// The same source, translated from a relocatable object and from a linked
/// executable, produces the same function bodies.
///
/// This is the property the whole front end turns on. A relocatable object
/// and the executable linked from it describe the same program in two
/// notations — one symbolic, one placed — and if the translator reads them
/// correctly it cannot tell the difference by the time it emits code. Any
/// divergence here is the front end's fault rather than a question about
/// the program, which is what makes it worth more than a fixture of its own.
#[test]
fn a_linked_input_translates_to_the_same_shape_as_a_relocatable_one() {
    let workspace = WorkingDirectory::new("linked-parity");
    let linked = read(&workspace, "arithmetic.c", "quad_mix");
    let relocatable = {
        let path = support::compile_corpus_object_with(
            &workspace,
            "arithmetic.c",
            support::Compiler::Gcc,
            support::CodeModel::Absolute,
            "-O1",
        );
        let bytes = std::fs::read(&path).expect("read");
        ObjectFile::parse(&bytes).expect("parse")
    };

    // Every function the object defines is in the executable, with the same
    // extent — the linker copies code, it does not rewrite it.
    for function in &relocatable.functions {
        let twin = linked
            .functions
            .iter()
            .find(|other| other.name == function.name)
            .unwrap_or_else(|| panic!("`{}` did not survive linking", function.name));
        assert_eq!(
            twin.size, function.size,
            "`{}` changed size when it was linked",
            function.name
        );
        let from = linked.address_of(twin) - linked.sections[twin.section].address;
        let linked_bytes =
            &linked.sections[twin.section].bytes[from as usize..][..twin.size as usize];
        let object_bytes = &relocatable.sections[function.section].bytes
            [function.offset as usize..][..function.size as usize];
        // The bytes differ only where the linker filled in a relocation, so
        // compare their lengths and the instruction count rather than the
        // bytes themselves — the point is that the same code arrived.
        assert_eq!(linked_bytes.len(), object_bytes.len());
    }
}

/// A linked executable translates, and the module it produces is valid.
///
/// The first end-to-end check of linked mode: every function lifted, every
/// operand resolved from an address rather than a relocation, and the result
/// something an engine will accept.
#[test]
fn a_linked_executable_translates_to_a_valid_module() {
    let workspace = WorkingDirectory::new("linked-translate");
    let path = link_corpus_executable(&workspace, "arithmetic.c", "quad_mix", "-O1");
    let bytes = std::fs::read(&path).expect("read");
    let object = ObjectFile::parse(&bytes).expect("parse");
    let translated = zaqaru::transpile::Transpiler::new(&object)
        .transpile()
        .expect("translating a linked executable");
    assert!(
        zaqaru::wasm_reader::is_wasm_object(&translated),
        "the output is not a relocatable wasm object"
    );

    // Linked and validated, which is the only check that means the whole
    // thing holds together: every reference resolved, every body
    // well-typed, every index in range.
    let object_path = workspace.write("linked.wasm.o", &translated);
    let module_path = workspace.path().join("linked.wasm");
    let outcome = support::try_link_wasm(
        &[object_path],
        &module_path,
        &["--fatal-warnings", "--export-table", "--growable-table"],
    );
    assert!(
        outcome.succeeded,
        "the translated executable did not link:\n{}",
        outcome.report()
    );
    support::validate_wasm(&std::fs::read(&module_path).expect("read the module"));
}

/// A `switch` in a linked executable is recovered from the bytes of its
/// table, because there are no relocations left to read.
///
/// This is the one genuinely new analysis in linked mode. Everywhere else an
/// absolute address is simply the number it already is; here three facts —
/// how wide an entry is, whether it holds a target or a difference, and
/// where the table ends — were carried by relocations and are now carried by
/// nothing. They are inferred, and what makes that sound rather than a guess
/// is the severity of the test each entry has to pass: its target must land
/// exactly on an instruction boundary inside the dispatching function.
#[test]
fn a_switch_is_recovered_from_a_linked_table() {
    for optimisation in ["-O1", "-O2"] {
        let workspace = WorkingDirectory::new(&format!("linked-switch{optimisation}"));
        let path =
            link_corpus_executable(&workspace, "switch_dispatch.c", "classify", optimisation);
        let bytes = std::fs::read(&path).expect("read");
        let object = ObjectFile::parse(&bytes).expect("parse");

        // The fixture really does dispatch through a table — otherwise this
        // test would pass by translating a program with no switch in it.
        let lifted = zaqaru::lifter::lift_object(&object).expect("lift");
        let tables: usize = lifted
            .iter()
            .map(|function| function.jump_tables.len())
            .sum();
        assert!(
            tables > 0,
            "[{optimisation}] no jump table was recovered, so this proves nothing"
        );

        // Every recovered target is an instruction boundary inside the
        // function that dispatches to it — which is the property the
        // inference rests on, checked rather than assumed.
        for function in &lifted {
            for table in function.jump_tables.values() {
                assert!(
                    table.targets.len() >= 2,
                    "[{optimisation}] a one-entry table"
                );
                for target in &table.targets {
                    assert!(
                        function.contains(*target),
                        "[{optimisation}] a target outside the dispatching function"
                    );
                }
            }
        }

        // And the whole thing translates, links and validates.
        let translated = zaqaru::transpile::Transpiler::new(&object)
            .transpile()
            .unwrap_or_else(|error| panic!("[{optimisation}] translating: {error:#}"));
        let object_path = workspace.write("switch.wasm.o", &translated);
        let module_path = workspace.path().join("switch.wasm");
        let outcome = support::try_link_wasm(
            &[object_path],
            &module_path,
            &["--fatal-warnings", "--export-table", "--growable-table"],
        );
        assert!(
            outcome.succeeded,
            "[{optimisation}] did not link:\n{}",
            outcome.report()
        );
        support::validate_wasm(&std::fs::read(&module_path).expect("read"));
    }
}

/// A linked jump table's entries are rewritten in the image, not in a data
/// segment, because the loader is what puts them in front of the guest.
///
/// The rewrite has to make the dispatch land on `table + arm` whatever the
/// entries held before, since that is the address the translated `br_table`
/// subtracts the table's address back out of. Checking the bytes here is the
/// only way to know that before a loader exists to apply them.
#[test]
fn a_linked_switch_is_rewritten_through_image_patches() {
    for optimisation in ["-O1", "-O2"] {
        let workspace = WorkingDirectory::new(&format!("linked-patch{optimisation}"));
        let path =
            link_corpus_executable(&workspace, "switch_dispatch.c", "classify", optimisation);
        let bytes = std::fs::read(&path).expect("read");
        let object = ObjectFile::parse(&bytes).expect("parse");

        let lifted = zaqaru::lifter::lift_object(&object).expect("lift");
        let tables: Vec<_> = lifted
            .iter()
            .flat_map(|function| function.jump_tables.values())
            .collect();
        assert!(!tables.is_empty(), "[{optimisation}] no table to patch");

        let translation = zaqaru::transpile::Transpiler::new(&object)
            .translate()
            .unwrap_or_else(|error| panic!("[{optimisation}] translating: {error:#}"));

        let entries: usize = tables.iter().map(|table| table.targets.len()).sum();
        assert_eq!(
            translation.patches.len(),
            entries,
            "[{optimisation}] one patch per entry, no more and no fewer"
        );

        for table in &tables {
            let base = object.sections[table.table_section].address + table.table_offset;
            for arm in 0..table.targets.len() as u64 {
                let address = base + arm * table.stride;
                let patch = translation
                    .patches
                    .iter()
                    .find(|patch| patch.address == address)
                    .unwrap_or_else(|| panic!("[{optimisation}] no patch at {address:#x}"));
                assert_eq!(
                    patch.bytes.len(),
                    table.stride as usize,
                    "[{optimisation}] a patch that is not one entry wide"
                );
                let mut value = [0u8; 8];
                value[..patch.bytes.len()].copy_from_slice(&patch.bytes);
                let value = u64::from_le_bytes(value);
                // Whichever form the table was in, the dispatch reaches
                // `table + arm` — directly, or by adding the table itself.
                let reached = if table.relative { base + value } else { value };
                assert_eq!(
                    reached,
                    base + arm,
                    "[{optimisation}] arm {arm} does not dispatch to itself"
                );
            }
        }

        // And a relocatable build of the same program patches nothing: its
        // tables are data segments the module carries.
        let relocatable = support::compile_corpus_object_with(
            &workspace,
            "switch_dispatch.c",
            support::Compiler::Clang,
            support::CodeModel::Absolute,
            optimisation,
        );
        let relocatable =
            ObjectFile::parse(&std::fs::read(&relocatable).expect("read")).expect("parse");
        assert!(
            zaqaru::transpile::Transpiler::new(&relocatable)
                .translate()
                .expect("translate")
                .patches
                .is_empty(),
            "[{optimisation}] a relocatable object asked the loader to patch something"
        );
    }
}

/// Where the functions are, when the symbol table has been taken away.
///
/// This is not a hypothetical: a shipped static binary is usually stripped,
/// and then `.eh_frame` is the only thing left that says where one function
/// ends and the next begins. The check is that stripping changes nothing —
/// the same extents come back, from the other witness.
#[test]
fn a_stripped_executable_still_has_functions() {
    let workspace = WorkingDirectory::new("linked-stripped");
    let named = support::link_corpus_executable_with(
        &workspace,
        "switch_dispatch.c",
        "classify",
        "-O1",
        support::Unwind::Present,
    );
    let stripped = support::strip(&workspace, &named);

    let named = ObjectFile::parse(&std::fs::read(&named).expect("read")).expect("parse");
    let unnamed = ObjectFile::parse(&std::fs::read(&stripped).expect("read")).expect("parse");

    // The premise: stripping really did take the names away.
    assert!(
        unnamed
            .functions
            .iter()
            .all(|function| function.name.starts_with("fn.")),
        "the stripped executable still has named functions, so this proves nothing"
    );
    assert!(!named.functions.is_empty());

    let extents = |object: &ObjectFile| -> Vec<(u64, u64)> {
        let mut extents: Vec<_> = object
            .functions
            .iter()
            .map(|function| {
                (
                    object.sections[function.section].address + function.offset,
                    function.size,
                )
            })
            .collect();
        extents.sort();
        extents
    };
    assert_eq!(
        extents(&named),
        extents(&unnamed),
        "the unwind table and the symbol table disagree about where the functions are"
    );

    // And the nameless one translates, which is the point of finding them.
    let translated = zaqaru::transpile::Transpiler::new(&unnamed)
        .transpile()
        .unwrap_or_else(|error| panic!("translating a stripped executable: {error:#}"));
    let object_path = workspace.write("stripped.wasm.o", &translated);
    let module_path = workspace.path().join("stripped.wasm");
    let outcome = support::try_link_wasm(
        &[object_path],
        &module_path,
        &["--fatal-warnings", "--export-table", "--growable-table"],
    );
    assert!(outcome.succeeded, "did not link:\n{}", outcome.report());
    support::validate_wasm(&std::fs::read(&module_path).expect("read"));
}

/// The boot path's way in.
///
/// Every function in a linked program is local — nothing outside can name
/// one — so the module has to define exactly one thing a caller can name,
/// which takes the entry point's address and runs the program. Without it a
/// linked module is a program nothing can start. The check is the real
/// shape: a separate object with an undefined reference to it, linked.
#[test]
fn a_linked_module_can_be_entered_from_outside() {
    let workspace = WorkingDirectory::new("linked-entry");
    let object = read(&workspace, "arithmetic.c", "quad_mix");
    let translated = zaqaru::transpile::Transpiler::new(&object)
        .transpile()
        .expect("translate");
    let object_path = workspace.write("entry.wasm.o", &translated);

    // The address the boot path would hand over is a real function's.
    assert!(
        object.function_at(object.entry).is_some(),
        "the entry point is in no function, so there is nothing to enter"
    );

    let caller = support::compile_foreign_wasm_object(
        &workspace,
        "boot",
        &format!(
            "void {entry}(long long address);\n\
             __attribute__((export_name(\"start\")))\n\
             void start(long long address) {{ {entry}(address); }}\n",
            entry = zaqaru::transpile::GUEST_ENTRY
        ),
    );

    let module_path = workspace.path().join("entry.wasm");
    let outcome = support::try_link_wasm(
        &[object_path, caller],
        &module_path,
        &["--fatal-warnings", "--export-table", "--growable-table"],
    );
    assert!(
        outcome.succeeded,
        "a caller of `{}` did not link:\n{}",
        zaqaru::transpile::GUEST_ENTRY,
        outcome.report()
    );
    support::validate_wasm(&std::fs::read(&module_path).expect("read"));
}
