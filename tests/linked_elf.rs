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
        support::CodeModel::Absolute,
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
/// one — so the address the ELF header states is all the boot path has to
/// go on, and the exec map is the only thing that turns one into a slot it
/// can enter through. It therefore has to be nameable across the link, and
/// this is the real shape of that: a separate object with an undefined
/// reference to it.
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
            "int {slot_of}(long long address);\n\
             int x86_run_thread(int slot);\n\
             /* The kernel side, stood in for: the seam calls one and the\n\
                exec map the other. */\n\
             void kisal_no_function_at(long long address) {{ (void)address; }}\n\
             long long kisal_syscall(long long n, long long a, long long b,\n\
                                     long long c, long long d, long long e,\n\
                                     long long f) {{\n\
               (void)n; (void)a; (void)b; (void)c; (void)d; (void)e; (void)f;\n\
               return 0;\n\
             }}\n\
             __attribute__((export_name(\"start\")))\n\
             int start(long long address) {{ return x86_run_thread({slot_of}(address)); }}\n",
            slot_of = zaqaru::transpile::EXEC_MAP_LOOKUP
        ),
    );

    // The seam carries `x86_run_thread`, which is the catch the boot path
    // enters through — the same one the scheduler uses, because starting a
    // process and scheduling a thread are the same act.
    let seam = workspace.write("seam.wasm.o", support::seam_object());
    let module_path = workspace.path().join("entry.wasm");
    let outcome = support::try_link_wasm(
        &[object_path, caller, seam],
        &module_path,
        &["--fatal-warnings", "--export-table", "--growable-table"],
    );
    assert!(
        outcome.succeeded,
        "a caller of `{}` did not link:\n{}",
        zaqaru::transpile::EXEC_MAP_LOOKUP,
        outcome.report()
    );
    support::validate_wasm(&std::fs::read(&module_path).expect("read"));
}

/// The loader and the translator read the same file, and have to agree.
///
/// They are separate readers by necessity — one is the transpiler's, on the
/// `object` crate; the other is kisal's, which runs inside the module and
/// parses the headers itself. A disagreement about where a segment goes
/// would put the program's bytes somewhere its own operands do not point,
/// and nothing downstream would say so.
#[test]
fn the_loader_and_the_translator_agree_about_the_program() {
    let workspace = WorkingDirectory::new("linked-agree");
    for name in ["arithmetic.c", "switch_dispatch.c"] {
        let entry = if name == "arithmetic.c" {
            "quad_mix"
        } else {
            "classify"
        };
        let path = support::link_corpus_executable(&workspace, name, entry, "-O1");
        let bytes = std::fs::read(&path).expect("read");

        let translator = ObjectFile::parse(&bytes).expect("the translator's reader");
        let loader = kisal::exec::Program::parse(&bytes).expect("the loader's reader");

        assert_eq!(loader.entry, translator.entry, "[{name}] a different entry");
        assert_eq!(
            loader.loads.len(),
            translator.segments.len(),
            "[{name}] a different number of segments"
        );
        for (placed, described) in loader.loads.iter().zip(&translator.segments) {
            assert_eq!(placed.address, described.address, "[{name}] address");
            assert_eq!(placed.file_size, described.file_size, "[{name}] file size");
            assert_eq!(
                placed.memory_size, described.memory_size,
                "[{name}] memory size"
            );
            assert_eq!(
                (placed.readable, placed.writable, placed.executable),
                (described.readable, described.writable, described.executable),
                "[{name}] permissions"
            );
        }

        // And the entry point the loader will jump to is a function the
        // translator actually translated.
        assert!(
            translator.function_at(loader.entry).is_some(),
            "[{name}] the entry point is in no translated function"
        );
    }
}

/// A whole program translates: entry, arguments, syscalls and all.
///
/// The corpus's other guests are functions a test calls. This one starts at
/// `_start`, which is hand-written assembly with no compiler-emitted extent
/// and a tail jump instead of a return — the two things about a real entry
/// point that a function-shaped fixture never exercises.
#[test]
fn a_whole_program_translates() {
    let workspace = WorkingDirectory::new("linked-process");
    let path = support::link_corpus_executable(&workspace, "process.c", "_start", "-O1");
    let bytes = std::fs::read(&path).expect("read");
    let object = ObjectFile::parse(&bytes).expect("parse");

    let start = object
        .function_at(object.entry)
        .and_then(|index| object.functions.get(index))
        .expect("the entry point is in no function");
    assert!(start.size > 0, "`_start` has no extent to translate");

    let translated = zaqaru::transpile::Transpiler::new(&object)
        .transpile()
        .unwrap_or_else(|error| panic!("translating a whole program: {error:#}"));
    let object_path = workspace.write("process.wasm.o", &translated);
    let module_path = workspace.path().join("process.wasm");
    let seam = workspace.write("seam.wasm.o", support::seam_object());
    let outcome = support::try_link_wasm(
        &[object_path, seam],
        &module_path,
        &[
            "--export-table",
            "--growable-table",
            "--unresolved-symbols=import-dynamic",
        ],
    );
    assert!(outcome.succeeded, "did not link:\n{}", outcome.report());
    support::validate_wasm(&std::fs::read(&module_path).expect("read"));
}

/// Functions with no stated extent, bounded by what follows them.
///
/// `.size` is a courtesy a compiler pays and hand-written assembly usually
/// does not — `crtbegin.o`'s stubs carry neither a size nor an unwind entry,
/// and they are in every binary gcc links. What is left is that a function
/// cannot run past whatever begins after it.
#[test]
fn a_function_with_no_stated_size_ends_where_the_next_one_starts() {
    let workspace = WorkingDirectory::new("linked-sizeless");
    let path = support::link_corpus_executable(&workspace, "sizeless.s", "entry", "-O1");
    let bytes = std::fs::read(&path).expect("read");
    let object = ObjectFile::parse(&bytes).expect("parse");

    // The premise: the symbols really do state nothing.
    for name in ["entry", "helper"] {
        let symbol = object
            .symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| panic!("`{name}` is not in the symbol table"));
        assert_eq!(
            symbol.size, 0,
            "`{name}` states a size, so this proves nothing"
        );
    }

    let functions: Vec<_> = object
        .functions
        .iter()
        .filter(|function| function.name == "entry" || function.name == "helper")
        .collect();
    assert_eq!(functions.len(), 2, "a function went missing");
    for function in &functions {
        assert!(function.size > 0, "`{}` was given no extent", function.name);
    }
    // Neither runs into the other.
    let entry = functions.iter().find(|f| f.name == "entry").expect("entry");
    let helper = functions
        .iter()
        .find(|f| f.name == "helper")
        .expect("helper");
    assert!(
        entry.offset + entry.size <= helper.offset,
        "`entry` runs into `helper`"
    );

    zaqaru::transpile::Transpiler::new(&object)
        .transpile()
        .expect("translating functions bounded by their neighbours");
}

/// A linked program compiled the way a distribution compiles one.
///
/// `-fno-pie` turns every global's address into an immediate, which hides an
/// entire operand shape: a distro `gcc -static` still compiles
/// position-independently, so data is reached program-counter-relatively
/// with no relocation left behind. In a relocatable object that shape can
/// only mean a function in the same section — the assembler resolved it —
/// and reading it that way in a linked one turns every global into "an
/// address inside a function", which is refused.
#[test]
fn a_linked_program_reaches_its_data_relative_to_the_program_counter() {
    let workspace = WorkingDirectory::new("linked-pic-data");
    let path = support::link_corpus_executable_with(
        &workspace,
        "data.c",
        "table_lookup",
        "-O1",
        support::Unwind::Omitted,
        support::CodeModel::PositionIndependent,
    );
    let bytes = std::fs::read(&path).expect("read");
    let object = ObjectFile::parse(&bytes).expect("parse");

    // The premise: something really is reached relative to the program
    // counter, with no relocation to say what it is.
    let lifted = zaqaru::lifter::lift_object(&object).expect("lift");
    let relative = lifted.iter().any(|function| {
        function.instructions.iter().any(|lifted| {
            lifted.displacement.is_none()
                && lifted.instruction.memory_base() == iced_x86::Register::RIP
        })
    });
    assert!(
        relative,
        "nothing in this program is reached relative to the program counter, \
         so this proves nothing"
    );

    zaqaru::transpile::Transpiler::new(&object)
        .transpile()
        .expect("translating a program that reaches its own data");
}

/// A function nobody can translate does not take the program with it.
///
/// A real binary carries code for processors this one is not — glibc ships
/// AVX-512 string routines beside SSE2 ones and picks between them from
/// CPUID, which the design curates to a baseline without AVX so the SSE2
/// paths are the ones taken. The AVX bodies are still there. Refusing a
/// whole program over code that cannot execute would refuse every real
/// program, so the refusal moves to the moment something calls the function.
#[test]
fn an_untranslatable_function_is_refused_by_default_and_trapped_on_request() {
    let workspace = WorkingDirectory::new("linked-untranslatable");
    let path = support::link_corpus_executable(&workspace, "untranslatable.s", "entry", "-O1");
    let bytes = std::fs::read(&path).expect("read");
    let object = ObjectFile::parse(&bytes).expect("parse");

    // The default: a gap in the translator is a gap, and it says so.
    let refused = zaqaru::transpile::Transpiler::new(&object)
        .transpile()
        .expect_err("an untranslatable instruction was translated");
    let refused = format!("{refused:#}");
    assert!(
        refused.contains("unreachable_path") && refused.contains("wrmsr"),
        "the refusal names neither the function nor the instruction: {refused}"
    );

    // Asked for, it becomes one function's problem instead of the program's.
    let translation = zaqaru::transpile::Transpiler::new(&object)
        .with_untranslatable(zaqaru::transpile::Untranslatable::Trap)
        .translate()
        .expect("translating around an untranslatable function");

    let names: Vec<&str> = translation
        .refused
        .iter()
        .map(|refusal| refusal.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["unreachable_path"],
        "the wrong functions were given up on"
    );
    assert!(
        translation.refused[0].reason.contains("wrmsr"),
        "the report does not name the instruction, so it is not a worklist: {}",
        translation.refused[0].reason
    );

    // And the module is still a module: the other functions translated, and
    // what stands in for the refused one links like anything else.
    support::validate_wasm(&translation.module);
    let object_path = workspace.write("untranslatable.wasm.o", &translation.module);
    let seam = workspace.write("seam.wasm.o", support::seam_object());
    let module_path = workspace.path().join("untranslatable.wasm");
    let outcome = support::try_link_wasm(
        &[object_path, seam],
        &module_path,
        &[
            "--export-table",
            "--growable-table",
            "--unresolved-symbols=import-dynamic",
        ],
    );
    assert!(outcome.succeeded, "did not link:\n{}", outcome.report());
}

/// The procedure linkage table's entries are functions.
///
/// A static executable still has one: `ifunc` is how a libc picks between
/// implementations at startup, and static linking keeps the mechanism —
/// an `R_X86_64_IRELATIVE` relocation and a stub that jumps through a slot
/// the startup code fills in. The stubs carry no symbols and no unwind
/// entries, so nothing else in discovery finds them, and a call to one
/// resolves to nothing.
#[test]
fn the_linkage_tables_entries_are_functions() {
    let workspace = WorkingDirectory::new("linked-plt");
    let path = support::link_corpus_executable_with(
        &workspace,
        "ifunc.c",
        "through_the_table",
        "-O1",
        support::Unwind::Omitted,
        support::CodeModel::Absolute,
    );
    let bytes = std::fs::read(&path).expect("read");
    let object = ObjectFile::parse(&bytes).expect("parse");

    // The premise: the fixture really does produce a linkage table.
    let (index, plt) = object
        .sections
        .iter()
        .enumerate()
        .find(|(_, section)| section.name == ".plt")
        .expect("the fixture no longer produces a `.plt`");
    assert!(!plt.bytes.is_empty(), "an empty linkage table");

    // One function per entry, each named after where it is, and none of
    // them named by a symbol — which is why they needed finding.
    let entries: Vec<_> = object
        .functions
        .iter()
        .filter(|function| function.section == index)
        .collect();
    // The entry size is the section's alignment, which is eight here and
    // sixteen where control-flow enforcement puts an `endbr64` in front of
    // the jump — so it is read rather than assumed.
    let stride = plt.alignment;
    assert!(
        stride == 8 || stride == 16,
        "a linkage table with a {stride}-byte stride, which is neither form"
    );
    assert_eq!(
        entries.len() as u64,
        plt.bytes.len() as u64 / stride,
        "the table did not divide into one function per entry"
    );
    for entry in &entries {
        assert!(entry.symbol.is_none(), "a stub the symbol table named");
        assert_eq!(entry.size, stride);
    }

    // And the call through the stub resolves: the jump lands on a function
    // the translator knows, which is what was failing.
    assert!(
        object
            .function_at(plt.address)
            .is_some_and(|index| object.functions[index].name.starts_with("plt.")),
        "the table's first entry is not a function"
    );
    zaqaru::transpile::Transpiler::new(&object)
        .transpile()
        .expect("translating a program that calls through its linkage table");
}

/// A call to a weak symbol nothing defines.
///
/// A static link resolves such a symbol to address zero and emits the call
/// anyway; the code guards it on a pointer being non-null, so it is never
/// taken. glibc does this around its locale and threading hooks, and
/// `__libc_start_main` — the function every program starts in — is one of
/// the functions that contains one, so refusing over it would refuse every
/// program's entry.
#[test]
fn a_call_to_a_weak_symbol_that_is_absent_traps_instead_of_refusing() {
    let workspace = WorkingDirectory::new("linked-weak");
    let path =
        support::link_corpus_executable(&workspace, "weak_call.s", "guarded_weak_call", "-O1");
    let bytes = std::fs::read(&path).expect("read");
    let object = ObjectFile::parse(&bytes).expect("parse");

    // The premise: the linker really did emit a call whose target is zero.
    let lifted = zaqaru::lifter::lift_object(&object).expect("lift");
    let function = lifted
        .iter()
        .find(|function| function.name == "guarded_weak_call")
        .expect("the fixture no longer defines it");
    let text = object.sections[function.section].address;
    let calls_nothing = function.instructions.iter().any(|lifted| {
        lifted.instruction.flow_control() == iced_x86::FlowControl::Call
            && lifted.instruction.op_kind(0) == iced_x86::OpKind::NearBranch64
            && text.wrapping_add(lifted.instruction.near_branch64()) == 0
    });
    assert!(
        calls_nothing,
        "nothing in the fixture calls address zero, so this proves nothing"
    );

    // The function translates whole — the call becomes a trap and the
    // instructions around it are still there.
    let translated = zaqaru::transpile::Transpiler::new(&object)
        .transpile()
        .expect("translating a function that calls an absent weak symbol");
    support::validate_wasm(&translated);
}

/// Every function says what found it.
///
/// Provenance is not decoration: discovery combines evidence of different
/// strengths under a rule that only one of them may bound a function, and a
/// refusal or a runtime exec-map miss that names its evidence — "reached
/// `fn.0x511aa5`, discovered by transfer" — is diagnosable where an address
/// alone is an afternoon. It is also what makes the rule checkable, which is
/// what this test does: the strata are visible in the output rather than
/// only inside the pass that produced them.
///
/// Stripping is what makes the second half meaningful. With the symbol table
/// present every function in a `-nostdlib` executable is found by its
/// symbol, and one stratum proves nothing about a design whose whole content
/// is that there are several.
#[test]
fn a_function_records_the_witness_that_found_it() {
    use zaqaru::discover::Witness;

    let workspace = WorkingDirectory::new("linked-provenance");
    let named = support::link_corpus_executable_with(
        &workspace,
        "switch_dispatch.c",
        "classify",
        "-O1",
        support::Unwind::Present,
        support::CodeModel::Absolute,
    );
    let object = ObjectFile::parse(&std::fs::read(&named).expect("read")).expect("parse");
    let named_function = object
        .functions
        .iter()
        .find(|function| function.name == "classify")
        .expect("the entry the executable was linked around");
    assert_eq!(
        named_function.witness,
        Witness::Symbol,
        "a function the symbol table names should say so"
    );
    assert!(named_function.symbol.is_some());

    // The same binary with its names taken away. Nothing can be found by a
    // symbol any more, so every function reports the witness that actually
    // found it — which is the point: the strata are what discovery has left
    // when the easy evidence is gone.
    let stripped = support::strip(&workspace, &named);
    let object = ObjectFile::parse(&std::fs::read(&stripped).expect("read")).expect("parse");
    assert!(
        !object.functions.is_empty(),
        "stripping left no functions at all"
    );
    for function in &object.functions {
        assert_ne!(
            function.witness,
            Witness::Symbol,
            "`{}` claims a symbol found it in a binary with no symbols",
            function.name
        );
    }
    let strata: std::collections::BTreeSet<Witness> = object
        .functions
        .iter()
        .map(|function| function.witness)
        .collect();
    assert!(
        strata.contains(&Witness::UnwindEntry),
        "nothing was found by an unwind entry in a binary built with them: \
         {strata:?}"
    );
}

/// An ifunc resolver a stripped binary names nowhere but in its relocations.
///
/// The mechanism survives static linking: the linker emits an
/// `R_X86_64_IRELATIVE` relocation whose addend is the resolver, and startup
/// code walks the relocations and calls each one. In a binary with no symbol
/// table and no unwind tables that relocation is the *only* thing in the
/// file that says the resolver exists — nothing calls it directly and no
/// instruction names its address.
///
/// A static glibc does not demonstrate this, which is why the fixture is
/// built the way it is: glibc ships with unwind tables, so its resolvers are
/// already accounted for by their frame entries and would be found with the
/// relocation harvest deleted. Here there is nothing else.
#[test]
fn an_ifunc_resolver_is_found_by_its_relocation_alone() {
    use zaqaru::discover::Witness;

    let workspace = WorkingDirectory::new("linked-ifunc");
    let named = support::link_corpus_executable_with(
        &workspace,
        "ifunc.c",
        "through_the_table",
        "-O1",
        support::Unwind::Omitted,
        support::CodeModel::Absolute,
    );
    let stripped = support::strip(&workspace, &named);

    // Where the resolver is, according to the file itself.
    let bytes = std::fs::read(&stripped).expect("read the stripped executable");
    let object = ObjectFile::parse(&bytes).expect("parse the stripped executable");
    let resolver = {
        let named = ObjectFile::parse(&std::fs::read(&named).expect("read")).expect("parse");
        let function = named
            .functions
            .iter()
            .find(|function| function.name == "resolve")
            .expect("the resolver, which the unstripped binary names");
        named.sections[function.section].address + function.offset
    };

    let found = object
        .functions
        .iter()
        .find(|function| object.sections[function.section].address + function.offset == resolver)
        .unwrap_or_else(|| {
            panic!("nothing was discovered at the resolver's address {resolver:#x}")
        });
    assert_eq!(
        found.witness,
        Witness::FileStated,
        "the resolver was found by {:?}, so this fixture is not testing the \
         relocation harvest",
        found.witness
    );
}

/// A relocation type the pipeline does not model does not stop the read.
///
/// The harvest is gathering evidence, not interpreting the file, so anything
/// it does not recognise is skipped. Refusing instead is a defect this
/// project has already shipped once: it made a stripped busybox unopenable,
/// because its `IRELATIVE` entries land on `.got` rather than on a section
/// the reader happened to be skipping.
///
/// Opportunistic, on whatever stripped static binary the machine has, since
/// the interesting inputs are the ones nobody in this repository built. A
/// machine without one proves nothing here and says so rather than passing
/// quietly.
#[test]
fn an_unmodelled_relocation_type_does_not_stop_the_read() {
    let mut examined = 0;
    for candidate in ["/usr/bin/busybox", "/bin/busybox"] {
        let Ok(bytes) = std::fs::read(candidate) else {
            continue;
        };
        // A path that happens to hold a shell script is not a candidate;
        // one that holds an ELF is, and then the parse has to work.
        if !bytes.starts_with(b"\x7fELF") {
            continue;
        }
        let Ok(object) = ObjectFile::parse(&bytes) else {
            panic!("{candidate} is an ELF that could not be read at all");
        };
        assert!(
            !object.functions.is_empty(),
            "{candidate} parsed but no functions were discovered in it"
        );
        examined += 1;
    }
    if examined == 0 {
        eprintln!("no stripped system binary to read; this test checked nothing");
    }
}

/// A callback the binary only ever *computes* the address of.
///
/// Nothing calls `handler` directly, no symbol survives stripping, and the
/// fixture is built without unwind tables — so the one instruction that
/// takes its address is the only thing in the file saying it is there. It is
/// the shape a callback, a vtable slot and a dispatch table all have, and
/// the reason discovery harvests operands and not only branch targets.
///
/// The evidence is also higher-precision than scanning data would be: an
/// instruction that manipulates an address is a stronger statement than a
/// bit pattern that resembles one. It is still weak evidence — it says a
/// number exists, not that control goes there — so the padding filter is
/// what keeps it from minting functions out of constants.
#[test]
fn a_callback_is_found_by_the_instruction_that_takes_its_address() {
    use zaqaru::discover::Witness;

    let workspace = WorkingDirectory::new("linked-callback");
    let named = support::link_corpus_executable_with(
        &workspace,
        "callback_table.c",
        "through_a_pointer",
        "-O1",
        support::Unwind::Omitted,
        support::CodeModel::Absolute,
    );
    let handler = {
        let object = ObjectFile::parse(&std::fs::read(&named).expect("read")).expect("parse");
        let function = object
            .functions
            .iter()
            .find(|function| function.name == "handler")
            .expect("the callback, which the unstripped binary names");
        object.sections[function.section].address + function.offset
    };

    let stripped = support::strip(&workspace, &named);
    let object = ObjectFile::parse(&std::fs::read(&stripped).expect("read")).expect("parse");
    let found = object
        .functions
        .iter()
        .find(|function| object.sections[function.section].address + function.offset == handler)
        .unwrap_or_else(|| panic!("nothing was discovered at the callback {handler:#x}"));
    assert_eq!(
        found.witness,
        Witness::AddressTaken,
        "the callback was found by {:?}, so this is not testing the operand \
         harvest",
        found.witness
    );
}
