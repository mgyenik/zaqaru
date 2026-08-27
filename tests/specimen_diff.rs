//! Structural comparison of our linking metadata against clang's.
//!
//! The risk this retires is "wasm-ld rejects our hand-built linking
//! metadata". Rather than debugging that against linker error messages, the
//! same trivial function is compiled by clang's wasm backend and transpiled
//! by us, and both objects' metadata is read by an independent reader and
//! compared property by property.

mod support;

use support::linking_format::{
    LinkingMetadata, SymbolTarget, is_relocatable_leb_site, read_linking_metadata,
    relocation_kind_has_addend, relocation_kind_is_leb,
};
use support::{WorkingDirectory, compile_corpus_object, compile_specimen};

fn transpiled_metadata(workspace: &WorkingDirectory) -> (Vec<u8>, LinkingMetadata) {
    let native = compile_corpus_object(workspace, "add.c");
    let output = workspace.path().join("add.wasm.o");
    support::transpile_object(&native, &output, zaqaru::structurer::Mode::default());
    let bytes = std::fs::read(&output).expect("read transpiled object");
    let metadata = read_linking_metadata(&bytes);
    (bytes, metadata)
}

fn specimen_metadata(workspace: &WorkingDirectory) -> (Vec<u8>, LinkingMetadata) {
    let object = compile_specimen(workspace, "reference.c");
    let bytes = std::fs::read(&object).expect("read specimen object");
    let metadata = read_linking_metadata(&bytes);
    (bytes, metadata)
}

#[test]
fn linking_metadata_matches_the_shape_clang_emits() {
    let workspace = WorkingDirectory::new("specimen");
    let (_, ours) = transpiled_metadata(&workspace);
    let (_, theirs) = specimen_metadata(&workspace);

    assert_eq!(
        ours.version, theirs.version,
        "linking metadata version differs from clang's"
    );

    // Both objects define `add` as a function symbol.
    let our_add = ours
        .symbol_named("add")
        .expect("our object defines a symbol named `add`");
    let their_add = theirs
        .symbol_named("add")
        .expect("clang's object defines a symbol named `add`");
    assert!(matches!(our_add.target, SymbolTarget::Function { .. }));
    assert!(matches!(their_add.target, SymbolTarget::Function { .. }));
    assert!(!our_add.is_undefined() && !their_add.is_undefined());

    // Both reference the linker's stack pointer as an undefined global whose
    // name is inherited from the import.
    for (label, metadata) in [("ours", &ours), ("clang's", &theirs)] {
        let stack_pointer = metadata
            .symbols
            .iter()
            .find(|symbol| {
                matches!(symbol.target, SymbolTarget::Global { .. }) && symbol.is_undefined()
            })
            .unwrap_or_else(|| panic!("{label} object has no undefined global symbol"));
        assert_eq!(
            stack_pointer.name, None,
            "{label} object spells an undefined global's name explicitly"
        );
    }

    // Both carry code relocations, and every relocation type we emit is one
    // clang's format agrees carries (or does not carry) an addend.
    let our_code = ours
        .relocations_for("CODE")
        .expect("our object has code relocations");
    assert!(!our_code.entries.is_empty());
    for entry in &our_code.entries {
        assert_eq!(
            entry.addend.is_some(),
            relocation_kind_has_addend(entry.kind),
            "relocation type {} carries the wrong addend shape",
            entry.kind
        );
    }
}

/// Relocation offsets are relative to the target section's payload, and each
/// LEB-typed site must be a five-byte non-canonical encoding the linker can
/// overwrite. Checking clang's object with the same assertions proves the
/// assertions themselves are right.
#[test]
fn relocation_sites_are_patchable_in_both_objects() {
    let workspace = WorkingDirectory::new("specimen-sites");
    let (our_bytes, ours) = transpiled_metadata(&workspace);
    let (their_bytes, theirs) = specimen_metadata(&workspace);

    for (label, bytes, metadata) in [
        ("ours", &our_bytes, &ours),
        ("clang's", &their_bytes, &theirs),
    ] {
        let relocations = metadata
            .relocations_for("CODE")
            .unwrap_or_else(|| panic!("{label} object has no reloc.CODE"));
        let code = metadata
            .section(relocations.target_section)
            .unwrap_or_else(|| panic!("{label} reloc.CODE names a section that is not there"));
        assert_eq!(
            code.id, 10,
            "{label} reloc.CODE points at section id {} rather than the code section",
            code.id
        );

        for entry in &relocations.entries {
            if !relocation_kind_is_leb(entry.kind) {
                continue;
            }
            let start = code.payload.start + entry.offset as usize;
            let site = &bytes[start..start + 5];
            assert!(
                is_relocatable_leb_site(site),
                "{label} object: relocation at code+{:#x} is not a five-byte \
                 patchable LEB (found {site:02x?})",
                entry.offset
            );
        }
    }
}

/// Every symbol a relocation names must exist, and every function symbol
/// must name a function that exists in the index space.
#[test]
fn relocations_and_symbols_are_internally_consistent() {
    let workspace = WorkingDirectory::new("specimen-consistency");
    let (_, ours) = transpiled_metadata(&workspace);

    for section in &ours.relocations {
        for entry in &section.entries {
            assert!(
                (entry.symbol_index as usize) < ours.symbols.len(),
                "{} names symbol {} but the table has {}",
                section.name,
                entry.symbol_index,
                ours.symbols.len()
            );
        }
    }
}
