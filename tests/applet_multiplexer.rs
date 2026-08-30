//! busybox: a stripped static binary that decides what to be from `argv[0]`.
//!
//! The first rung of the acceptance ladder in `container-build-plan.md`'s M6,
//! and the binary `docs/code-discovery.md` was largely written about. It is
//! here because it is hard in a way the other tests are not:
//!
//! - **It is stripped, and 38% of its text has no unwind entry.** So a large
//!   part of it is found by no strong witness at all, and the extents there
//!   are guesses bounded by whatever begins next.
//! - **Its applets are reached through a table.** `applet_main[]` is 278
//!   pointers in `.data.rel.ro` naming functions that no symbol, no unwind
//!   entry and no instruction ever names — and which land *inside* those
//!   guessed extents rather than in the gaps between them. Finding them is
//!   the data-array witness; being allowed to cut for them is the
//!   stated-versus-guessed distinction.
//! - **It reads `argv[0]` to choose one.** Which is why the bake records a
//!   command line at all.
//!
//! `ls` is the applet chosen because it is the widest one that does not
//! fork: the table lookup, a directory read, a `stat` per entry, the clock,
//! and formatted output through the write path.
//!
//! Skipped where the system has no busybox — a machine without one is not
//! this test's failure. One container, because each applet would need its
//! own bake and the ladder is a gate rather than a survey.

mod support;

use support::WorkingDirectory;

fn path(segments: &[&[u8]]) -> Vec<Vec<u8>> {
    segments.iter().map(|segment| segment.to_vec()).collect()
}

#[test]
fn a_stripped_applet_multiplexer_finds_its_applet() {
    let Some(program) = present("/usr/bin/busybox").or_else(|| present("/bin/busybox")) else {
        eprintln!("skipped: no busybox on this system");
        return;
    };
    let workspace = WorkingDirectory::new("busybox-ls");

    // Something to list. The name is checked for exactly, so a stray file
    // in the image would be a failure rather than a surprise.
    let mut tree = baker::tree::Tree::new();
    let (directory, name) = tree
        .place(b"/etc/sample.txt")
        .expect("place")
        .expect("a path a file can go at");
    let file = tree.add(
        baker::tree::Meta {
            mode: kisal::image::file_type::REGULAR | 0o644,
            ..Default::default()
        },
        baker::tree::Body::Regular(b"alpha\nbeta\n".to_vec()),
    );
    tree.link(directory, &name, file).expect("link");

    let argv: Vec<Vec<u8>> = [b"busybox".as_slice(), b"ls", b"/etc"]
        .iter()
        .map(|argument| argument.to_vec())
        .collect();
    let baked = baker::bake::container_with_command(
        &program,
        std::path::Path::new("/"),
        tree,
        &argv,
    )
    .expect("bake busybox");

    let guest = workspace.write("program.wasm.o", &baked.module);
    let module = support::link_container_for_program(
        &workspace,
        std::slice::from_ref(&guest),
        &baked.image,
        "busybox-ls",
        Some(baked.top),
    );
    let mut mounts = support::mounts_seeded(&[0x77; 32]);
    mounts.mount(&[b"iso", b"shutdown"], Box::new(runner::store::Sink::new()));
    mounts.mount(&[b"iso", b"time"], Box::new(runner::store::Clock::new()));
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
        panic!(
            "the container did not finish: {error:?}\nkernel log: {}",
            String::from_utf8_lossy(&log)
        )
    });
    assert_eq!(status, 0, "busybox did not exit cleanly");

    let out = container
        .mounts()
        .read(&path(&[b"iso", b"console", b"stdout"]))
        .expect("console")
        .unwrap_or_default();
    assert_eq!(String::from_utf8_lossy(&out), "sample.txt\n");
}

fn present(path: &str) -> Option<std::path::PathBuf> {
    std::fs::metadata(path)
        .is_ok()
        .then(|| std::path::PathBuf::from(path))
}
