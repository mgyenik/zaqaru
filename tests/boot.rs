//! Booting a container: the whole path, once, end to end.
//!
//! Every other test in the suite takes one seam and drives it. This takes
//! all of them at once, in the order a real run uses them — the image is
//! baked, the program in it is translated, kisal loads it, builds the stack
//! it starts on, enters it through the exec map, serves its syscalls, and
//! catches the throw its `exit_group` becomes. A failure anywhere in that
//! chain shows up here as a wrong status or missing output, which is why the
//! narrower tests exist to say *where*.

mod support;

use support::WorkingDirectory;

fn path(segments: &[&[u8]]) -> Vec<Vec<u8>> {
    segments.iter().map(|segment| segment.to_vec()).collect()
}

/// The program's own name inside the image. `kisal_boot` runs this and
/// nothing else, which is the whole of M6's process model.
const PROGRAM: &str = "init";

fn container(workspace: &WorkingDirectory) -> runner::Container {
    let elf = support::link_corpus_executable(workspace, "process.c", "_start", "-O1");
    let bytes = std::fs::read(&elf).expect("read the program");
    let object = zaqaru::reader::ObjectFile::parse(&bytes).expect("parse the program");
    let top = object
        .segments
        .iter()
        .map(|segment| segment.address + segment.memory_size)
        .max()
        .expect("a linked program has segments");

    let translation = zaqaru::transpile::Transpiler::new(&object)
        .translate()
        .expect("translate the program");
    let guest = workspace.write("process.wasm.o", &translation.module);

    // The image the container carries, holding the program at the path the
    // boot path runs — with the translator's rewrites applied to it, which
    // is the half of translating a linked program that lands in the bytes
    // rather than in the module.
    let root = workspace.path().join("image");
    std::fs::create_dir_all(&root).expect("create the image tree");
    let mut placed = bytes.clone();
    baker::program::apply(&mut placed, &translation.patches).expect("apply the patches");
    std::fs::write(root.join(PROGRAM), &placed).expect("place the program in the image");
    let image = baker::object::emit(&baker::bake_directory(&root).expect("bake"))
        .expect("emit the image object");
    let module = support::link_container_for_program(
        workspace,
        std::slice::from_ref(&guest),
        &image,
        "boot",
        Some(top),
    );

    // Where the exit status goes. A container with no such mount still
    // runs and still returns its status to the host — the write is
    // best-effort, like the kernel log — but then there is nothing to
    // check, so the test that checks it mounts one.
    let mut mounts = support::mounts_seeded(&[0x11; 32]);
    mounts.mount(&[b"iso", b"shutdown"], Box::new(runner::store::Sink::new()));

    runner::Container::instantiate(&std::fs::read(&module).expect("read the container"), mounts)
        .expect("instantiate the container")
}

#[test]
fn a_container_runs_its_program_and_reports_how_it_ended() {
    let workspace = WorkingDirectory::new("boot");
    let mut container = container(&workspace);

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
            "the container did not finish: {error:?}\nkernel log: {}\nstdout so far: {:?}",
            String::from_utf8_lossy(&log),
            String::from_utf8_lossy(&out)
        )
    });

    let output = container
        .mounts()
        .read(&path(&[b"iso", b"console", b"stdout"]))
        .expect("the console mount failed")
        .unwrap_or_default();
    let output = String::from_utf8(output).expect("the guest wrote something unreadable");

    // What the program printed about the stack it was given. Each line is a
    // separate thing the kernel had to get right for a libc to start at all.
    for expected in [
        // The program headers are readable at the address `AT_PHDR` named,
        // which is what musl's TLS setup walks.
        "phdr:yes",
        "phnum:yes",
        "pagesz:4096",
        "entry:yes",
        "random:yes",
        // And no vDSO, which is what makes the clock arrive as a syscall.
        "vdso:no",
    ] {
        assert!(
            output.contains(expected),
            "the guest did not report `{expected}`; it wrote:\n{output}"
        );
    }

    // Its own name, off the stack the kernel built.
    assert!(
        output.starts_with("/init\n"),
        "the guest did not read its arguments; it wrote:\n{output}"
    );

    // And the status, both ways it travels: the host's return value, and
    // the payload a host that only speaks through the store would read.
    assert_eq!(status, 7, "the exit status did not reach the host");
    let complete = container
        .mounts()
        .read(&path(&[b"iso", b"shutdown", b"complete"]))
        .expect("the shutdown mount failed")
        .unwrap_or_default();
    assert_eq!(
        String::from_utf8(complete).expect("an unreadable status"),
        "7",
        "the status did not reach `/iso/shutdown/complete`"
    );
}

/// Natively the same program prints the same things — except the vDSO, which
/// Linux supplies and kisal deliberately does not.
///
/// Without this the test above is a description of what was implemented
/// rather than a comparison against what Linux does.
#[test]
fn the_program_says_the_same_things_natively() {
    let workspace = WorkingDirectory::new("boot-native");
    let elf = support::link_corpus_executable(&workspace, "process.c", "_start", "-O1");
    use std::os::unix::process::CommandExt;
    let native = std::process::Command::new(&elf)
        .arg0("/init")
        .env_clear()
        .output()
        .expect("run the program natively");
    let native = String::from_utf8(native.stdout).expect("unreadable native output");

    for expected in [
        "phdr:yes",
        "phnum:yes",
        "pagesz:4096",
        "entry:yes",
        "random:yes",
    ] {
        assert!(
            native.contains(expected),
            "natively the program does not report `{expected}`:\n{native}"
        );
    }
    assert!(
        native.contains("vdso:yes"),
        "Linux did not supply a vDSO, so the container's `vdso:no` compares \
         against nothing:\n{native}"
    );
}

/// The exec map's slots survive being linked with anything else.
///
/// Every object numbers its own table entries from one, and the linker
/// renumbers them as it merges the tables. The map is data, so a slot
/// written there as a constant stays whatever this object called it — and in
/// a container that number belongs to the seam, whose `kisal_yield` takes the
/// first slot. Entering the program would then throw instead of running it,
/// which is what this catches without needing the whole program to run.
#[test]
fn the_exec_map_names_the_program_and_not_the_seam() {
    let workspace = WorkingDirectory::new("boot-slots");
    let mut container = container(&workspace);

    let elf = support::link_corpus_executable(&workspace, "process.c", "_start", "-O1");
    let object =
        zaqaru::reader::ObjectFile::parse(&std::fs::read(&elf).expect("read")).expect("parse");

    let entry: i32 = container
        .call("x86_slot_of", object.entry as i64)
        .expect("the exec map is not callable");
    let yield_slot: i32 = container
        .call("x86_yield_slot", ())
        .expect("the seam does not report its yield slot");

    assert_ne!(
        entry, yield_slot,
        "the entry point resolves to the seam's throw, so entering the program \
         would unwind instead of running"
    );
    assert!(entry > 0, "the entry point resolves to slot {entry}");
}

/// A container built without a program says so, rather than trapping.
///
/// The kernel's boot path names the exec map unconditionally — it is Rust,
/// compiled once, and cannot know whether the container it ends up in has a
/// program in it. So the seam carries a weak one that answers "there is no
/// program", the linker prefers a real one where there is, and a bake that
/// skipped the translator gets a sentence instead of a trap.
#[test]
fn a_container_with_no_program_refuses_to_boot() {
    let workspace = WorkingDirectory::new("boot-empty");

    // The inconsistent bake this exists for: an image that has the program,
    // and a module that was never given it. The missing-file path is a
    // different refusal with its own message, so the image really does have
    // to carry one for this to be the thing under test.
    let elf = support::link_corpus_executable(&workspace, "process.c", "_start", "-O1");
    let root = workspace.path().join("image");
    std::fs::create_dir_all(&root).expect("create the image tree");
    std::fs::copy(&elf, root.join(PROGRAM)).expect("place the program");
    let image = baker::object::emit(&baker::bake_directory(&root).expect("bake"))
        .expect("emit the image object");

    // Reserved, so the loader gets as far as looking for the entry point:
    // an unreserved region is a different refusal, with its own message.
    let placed =
        zaqaru::reader::ObjectFile::parse(&std::fs::read(&elf).expect("read")).expect("parse");
    let top = placed
        .segments
        .iter()
        .map(|segment| segment.address + segment.memory_size)
        .max()
        .expect("segments");

    let object = support::compile_corpus_object(&workspace, "add.c");
    let guest = workspace.path().join("add.wasm.o");
    support::transpile_object(&object, &guest, zaqaru::structurer::Mode::default());
    let module = support::link_container_for_program(
        &workspace,
        std::slice::from_ref(&guest),
        &image,
        "empty",
        Some(top),
    );

    let mut container = runner::Container::instantiate(
        &std::fs::read(&module).expect("read the container"),
        support::m1_mounts(),
    )
    .expect("instantiate");

    let error = container
        .boot()
        .expect_err("a container with no program booted");
    let _ = error;
    let log = container
        .mounts()
        .read(&path(&[b"iso", b"log", b"error"]))
        .expect("the log mount failed")
        .unwrap_or_default();
    let log = String::from_utf8_lossy(&log).into_owned();
    assert!(
        log.contains("no linked program"),
        "the kernel did not say what was missing; it logged: {log}"
    );
}

/// Reaching a function that was never translated says which one.
///
/// The trapping body exists for code a real binary carries and never runs.
/// If something does run it, the container stops — and the whole value of
/// stopping this way rather than trapping is that the message names the
/// function, so the answer is "implement this instruction" rather than "a
/// wasm trap happened somewhere".
#[test]
fn calling_an_untranslated_function_names_it() {
    let workspace = WorkingDirectory::new("boot-untranslated");
    let elf =
        support::link_corpus_executable(&workspace, "calls_untranslatable.s", "_start", "-O1");
    let bytes = std::fs::read(&elf).expect("read the program");
    let object = zaqaru::reader::ObjectFile::parse(&bytes).expect("parse");
    let top = object
        .segments
        .iter()
        .map(|segment| segment.address + segment.memory_size)
        .max()
        .expect("segments");

    let translation = zaqaru::transpile::Transpiler::new(&object)
        .with_untranslatable(zaqaru::transpile::Untranslatable::Trap)
        .translate()
        .expect("translate");
    assert_eq!(
        translation.refused.len(),
        1,
        "the fixture no longer has exactly one untranslatable function"
    );
    let guest = workspace.write("halted.wasm.o", &translation.module);

    let root = workspace.path().join("image");
    std::fs::create_dir_all(&root).expect("create the image tree");
    let mut placed = bytes.clone();
    baker::program::apply(&mut placed, &translation.patches).expect("apply the patches");
    std::fs::write(root.join(PROGRAM), &placed).expect("place the program");
    let image = baker::object::emit(&baker::bake_directory(&root).expect("bake"))
        .expect("emit the image object");

    let module = support::link_container_for_program(
        &workspace,
        std::slice::from_ref(&guest),
        &image,
        "untranslated",
        Some(top),
    );
    let mut container = runner::Container::instantiate(
        &std::fs::read(&module).expect("read the container"),
        support::mounts_seeded(&[0x22; 32]),
    )
    .expect("instantiate");

    container
        .boot()
        .expect_err("a container calling an untranslated function finished");

    // It got as far as the call, so what stopped it is the call and not
    // something before it.
    let output = container
        .mounts()
        .read(&path(&[b"iso", b"console", b"stdout"]))
        .expect("the console mount failed")
        .unwrap_or_default();
    assert_eq!(
        String::from_utf8_lossy(&output),
        "running\n",
        "the program did not reach the call"
    );

    let log = container
        .mounts()
        .read(&path(&[b"iso", b"log", b"error"]))
        .expect("the log mount failed")
        .unwrap_or_default();
    let log = String::from_utf8_lossy(&log).into_owned();
    assert!(
        log.contains("halted"),
        "the kernel did not name the function that was never translated; it \
         logged: {log}"
    );
}

/// The code around an absent weak call still runs.
///
/// A static link resolves a weak symbol nothing defines to address zero and
/// emits the call anyway; the guard on a null pointer is what makes the
/// program correct. The call can never be taken, but everything around it is
/// ordinary code — and glibc's `__libc_start_main` is one such function, so
/// this is the difference between a program that starts and one that does
/// not.
///
/// It is checked here rather than in the differential because the property
/// only exists in a linked program: in a relocatable object an undefined
/// symbol is an import, and something else may yet define it.
#[test]
fn the_code_around_an_absent_weak_call_runs() {
    let workspace = WorkingDirectory::new("boot-weak");
    let elf =
        support::link_corpus_executable(&workspace, "weak_call.s", "guarded_weak_call", "-O1");
    let bytes = std::fs::read(&elf).expect("read");
    let object = zaqaru::reader::ObjectFile::parse(&bytes).expect("parse");
    let top = object
        .segments
        .iter()
        .map(|segment| segment.address + segment.memory_size)
        .max()
        .expect("segments");

    let translation = zaqaru::transpile::Transpiler::new(&object)
        .translate()
        .expect("translate");
    let guest = workspace.write("weak.wasm.o", &translation.module);
    let image = baker::object::empty().expect("an image object");
    let module = support::link_container_for_program(
        &workspace,
        std::slice::from_ref(&guest),
        &image,
        "weak",
        Some(top),
    );
    let mut container = runner::Container::instantiate(
        &std::fs::read(&module).expect("read the container"),
        support::m1_mounts(),
    )
    .expect("instantiate");

    // The guard is false, so the call is skipped and the function returns
    // its argument plus seven — every instruction after the trap included.
    for value in [0i64, 1, -1, 12345] {
        assert_eq!(
            container
                .call_guest("guarded_weak_call", [value, 0, 0, 0, 0, 0])
                .expect("the guest trapped"),
            value + 7,
            "the guarded path did not run"
        );
    }
}
