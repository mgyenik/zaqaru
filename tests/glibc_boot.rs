//! Booting a real binary: a static glibc `hello`, start to finish.
//!
//! Every other test drives code this repository compiled for the purpose.
//! This one takes a program built the way a distribution builds one — the
//! whole C library statically linked in, a thousand functions, hand-written
//! assembly, ifunc resolvers, hot/cold splitting — translates it, bakes it
//! into an image, and runs it.

mod support;

use support::WorkingDirectory;

fn path(segments: &[&[u8]]) -> Vec<Vec<u8>> {
    segments.iter().map(|segment| segment.to_vec()).collect()
}

/// Not yet passing, and kept as the standing measure of how far a real
/// binary gets. What stops it today is a jump table whose arms leave the
/// function: `read_encoded_value_with_base` switches on a DWARF encoding
/// byte and some arms land in its `.cold` fragment, so the entry scan —
/// which stops where targets stop being instruction boundaries *inside* the
/// function — reads a table shorter than the real one, and a valid encoding
/// dispatches past its end.
#[test]
#[ignore = "the boot reaches DWARF frame registration and stops at a truncated jump table"]
fn a_static_glibc_hello_runs() {
    let workspace = WorkingDirectory::new("glibc-hello");

    // Built the way a distribution builds one, which is the point.
    let source = workspace.write(
        "hello.c",
        "#include <stdio.h>\nint main(void){ puts(\"hello\"); return 0; }\n",
    );
    let elf = workspace.path().join("hello.elf");
    support::run_tool(
        "gcc",
        &[
            "-static",
            "-O1",
            &source.to_string_lossy(),
            "-o",
            &elf.to_string_lossy(),
        ],
    );

    let bytes = std::fs::read(&elf).expect("read the program");
    let object = zaqaru::reader::ObjectFile::parse(&bytes).expect("parse");
    let top = object
        .segments
        .iter()
        .map(|segment| segment.address + segment.memory_size)
        .max()
        .expect("a linked program has segments");

    let translation = zaqaru::transpile::Transpiler::new(&object)
        .with_untranslatable(zaqaru::transpile::Untranslatable::Trap)
        .translate()
        .expect("translate");
    eprintln!(
        "translated {} functions, {} refused",
        object.functions.len(),
        translation.refused.len()
    );
    // The worklist, written where a failing run can be read afterwards: the
    // workspace goes away with the test.
    let worklist: String = translation
        .refused
        .iter()
        .map(|refusal| format!("{}: {}\n", refusal.name, refusal.reason))
        .collect();
    std::fs::write("/tmp/glibc-refused.txt", &worklist).expect("write the worklist");
    let guest = workspace.write("hello.wasm.o", &translation.module);

    let root = workspace.path().join("image");
    std::fs::create_dir_all(&root).expect("create the image tree");
    let mut placed = bytes.clone();
    baker::program::apply(&mut placed, &translation.patches).expect("apply the patches");
    std::fs::write(root.join("init"), &placed).expect("place the program");
    let image = baker::object::emit(&baker::bake_directory(&root).expect("bake"))
        .expect("emit the image object");

    let module = support::link_container_for_program(
        &workspace,
        std::slice::from_ref(&guest),
        &image,
        "glibc",
        Some(top),
    );

    let mut mounts = support::mounts_seeded(&[0x77; 32]);
    mounts.mount(&[b"iso", b"shutdown"], Box::new(runner::store::Sink::new()));
    let mut container = runner::Container::instantiate(
        &std::fs::read(&module).expect("read the container"),
        mounts,
    )
    .expect("instantiate");

    let status = container.boot().unwrap_or_else(|error| {
        let log = container
            .mounts()
            .read(&path(&[b"iso", b"log", b"error"]))
            .ok()
            .flatten()
            .unwrap_or_default();
        let out = container
            .mounts()
            .read(&path(&[b"iso", b"console", b"stdout"]))
            .ok()
            .flatten()
            .unwrap_or_default();
        panic!(
            "the container did not finish: {error:?}\nkernel log: {}\nstdout: {:?}",
            String::from_utf8_lossy(&log),
            String::from_utf8_lossy(&out)
        )
    });

    let stream = |container: &mut runner::Container, name: &[u8]| -> String {
        String::from_utf8_lossy(
            &container
                .mounts()
                .read(&path(&[b"iso", b"console", name]))
                .expect("console")
                .unwrap_or_default(),
        )
        .into_owned()
    };
    let out = stream(&mut container, b"stdout");
    let err = stream(&mut container, b"stderr");
    let log = String::from_utf8_lossy(
        &container
            .mounts()
            .read(&path(&[b"iso", b"log", b"error"]))
            .ok()
            .flatten()
            .unwrap_or_default(),
    )
    .into_owned();
    assert_eq!(
        out, "hello\n",
        "status {status}\nstderr: {err:?}\nkernel log: {log}"
    );
    assert_eq!(status, 0);
}
