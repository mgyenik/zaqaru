//! The low region a loaded program occupies, and what keeps it empty.
//!
//! A linked guest's `PT_LOAD` segments have to land at the addresses its own
//! operands name. Those addresses are low — a `-no-pie` x86-64 executable
//! starts at four megabytes — and the module's own data starts at a
//! kilobyte and grows upward through them, `__image_blob` first and largest.
//! Nothing kisal does at run time can move data the linker already placed,
//! so the bake is what has to decide, and this is the check that it did.

mod support;

use support::WorkingDirectory;

/// Every address the module's own data occupies, as the linker placed it.
fn data_extent(module: &[u8]) -> (u64, u64) {
    let mut lowest = u64::MAX;
    let mut highest = 0u64;
    for payload in wasmparser::Parser::new(0).parse_all(module) {
        let wasmparser::Payload::DataSection(section) = payload.expect("parse") else {
            continue;
        };
        for entry in section {
            let entry = entry.expect("a data segment");
            let wasmparser::DataKind::Active { offset_expr, .. } = entry.kind else {
                continue;
            };
            let mut reader = offset_expr.get_operators_reader();
            let start = match reader.read().expect("an offset expression") {
                wasmparser::Operator::I32Const { value } => value as u32 as u64,
                other => panic!("a data segment placed by {other:?}, which is not a constant"),
            };
            lowest = lowest.min(start);
            highest = highest.max(start + entry.data.len() as u64);
        }
    }
    assert_ne!(lowest, u64::MAX, "the module carries no data at all");
    (lowest, highest)
}

/// The premise, which is what makes the rest of this necessary: an ordinary
/// container's data begins low enough to cover a program's load address.
#[test]
fn a_container_without_a_program_puts_its_data_where_a_program_would_go() {
    let workspace = WorkingDirectory::new("region-default");
    let object = support::compile_corpus_object(&workspace, "arithmetic.c");
    let guest = workspace.path().join("arithmetic.wasm.o");
    support::transpile_object(&object, &guest, zaqaru::structurer::Mode::default());
    let module = support::link_container(&workspace, &[guest], "default");
    let (lowest, highest) = data_extent(&std::fs::read(&module).expect("read"));

    assert!(
        lowest < 0x400000,
        "the linker no longer starts data low, so this test proves nothing"
    );
    // The image here is empty and the guest is one small object, so the
    // data does not actually reach four megabytes — but it starts below it
    // and grows upward, and a real image is the hundred megabytes that
    // closes the gap.
    let _ = highest;
}

/// And with a program to load, the data is above every address that program
/// will occupy.
#[test]
fn a_container_carrying_a_program_leaves_its_addresses_alone() {
    let workspace = WorkingDirectory::new("region-reserved");
    let path = support::link_corpus_executable(&workspace, "arithmetic.c", "quad_mix", "-O1");
    let bytes = std::fs::read(&path).expect("read the executable");
    let object = zaqaru::reader::ObjectFile::parse(&bytes).expect("parse");

    let top = object
        .segments
        .iter()
        .map(|segment| segment.address + segment.memory_size)
        .max()
        .expect("a linked executable has segments");
    assert!(
        top > 0x400000,
        "the fixture does not load where a real executable does"
    );

    let guest = workspace.path().join("program.wasm.o");
    support::transpile_object(&path, &guest, zaqaru::structurer::Mode::default());
    let image = baker::object::empty().expect("an image object");
    let module = support::link_container_for_program(
        &workspace,
        std::slice::from_ref(&guest),
        &image,
        "reserved",
        Some(top),
    );
    let (lowest, _) = data_extent(&std::fs::read(&module).expect("read"));

    assert!(
        lowest >= top,
        "the module's data starts at {lowest:#x}, inside the program's own \
         addresses (which reach {top:#x})"
    );

    // The same container without the reservation, so that the check above
    // is a difference the bake made rather than something that was going to
    // be true anyway.
    let unreserved = support::link_container_for_program(
        &workspace,
        std::slice::from_ref(&guest),
        &image,
        "unreserved",
        None,
    );
    let (collides, _) = data_extent(&std::fs::read(&unreserved).expect("read"));
    assert!(
        collides < top,
        "without the reservation the data already avoided the program, so \
         the reservation is not what is being tested"
    );
}
