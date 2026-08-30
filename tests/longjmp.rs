//! `setjmp`/`longjmp`: the saved PC is already a continuation.
//!
//! Under `--resume` every call site stores a resume ID in its return-address
//! slot, and `setjmp` saves the word at `(%rsp)` — so a `jmp_buf`'s "program
//! counter" is a materialized, enterable continuation, stored by code that
//! has no idea that is what it is doing. `longjmp` jumps to it, the exec map
//! misses, and the miss path tells a tagged ID from an address, hands the
//! kernel somewhere to go, and throws away the wasm frames in between — the
//! blocking-syscall leave path, used verbatim.
//!
//! Nothing here is a shim and nothing matches on a name, which matters
//! because a stripped binary has no names to match. See
//! `container-plan.md`'s setjmp section.

mod support;

use support::WorkingDirectory;

/// Every rung of the ladder, in one program, against the same program run by
/// Linux.
///
/// One container rather than five because each one costs a bake and a link
/// of the whole of glibc, and the cases are independent of each other in the
/// guest — what they share is a `jmp_buf` and the order they run in, both of
/// which the comparison covers.
#[test]
fn setjmp_and_longjmp_agree_with_native() {
    let workspace = WorkingDirectory::new("longjmp");
    let written = support::dynamic_program_agrees_with_native(
        &workspace,
        &support::corpus_source("long_jump.c"),
        "longjmp",
    );

    // Named, so a failure says which rung broke rather than that a diff did.
    for expected in [
        // Same frame, nothing in between.
        "same_frame\n",
        // Across six frames, with callee-saved values live across the jump —
        // a restore that got them wrong prints different numbers rather than
        // crashing, which is the failure worth catching.
        "across_frames:22 1111 2222 3333\n",
        // The canonical idiom, and the case that tells this design from
        // reading the continuation back out of the stack: the slot `setjmp`
        // saved from has been overwritten by the call this frame made next,
        // so entering *it* would print "work returned" instead.
        "reused_slot:jumped\n",
        // Twenty thousand jumps in constant stack. The frames between a jump
        // and its target are discarded by a throw; a design that called the
        // continuation instead would leak a chain per iteration and exhaust
        // the stack long before this number.
        "many:20000\n",
        // And a jump whose target frame was itself entered by a resume body,
        // which is where "the frames re-materialize lazily" does the most
        // work.
        "nested\n",
        "done\n",
    ] {
        assert!(
            written.contains(expected),
            "the guest did not report `{expected}`; it wrote:\n{written}"
        );
    }
}

/// And a value that is *not* a continuation still misses by name.
///
/// The negative control for the discrimination. The loud miss is the default
/// arm now, with a test on the tag in front of it, and an arm nothing
/// exercises is an arm that can be wired backwards without anything saying
/// so — which is exactly what a mutation of this check found.
///
/// A resume ID is a table slot with an entry index above it; an address is
/// neither, so it takes the other arm and reports itself, as it always did.
#[test]
fn an_address_that_is_not_a_continuation_still_misses_by_name() {
    let workspace = WorkingDirectory::new("longjmp-miss");
    let source = workspace.write(
        "wild.c",
        "#include <stdio.h>\n\
         int main(void) {\n\
             printf(\"before\\n\");\n\
             fflush(stdout);\n\
             void (*wild)(void) = (void (*)(void))0x424242;\n\
             wild();\n\
             return 0;\n\
         }\n",
    );
    let elf = workspace.path().join("wild.elf");
    support::run_tool(
        "gcc",
        &["-O2", &source.to_string_lossy(), "-o", &elf.to_string_lossy()],
    );

    let mut tree = baker::tree::Tree::new();
    tree.resolve_or_create(b"/tmp").expect("a /tmp in the image");
    let baked = baker::bake::container(&elf, std::path::Path::new("/"), tree).expect("bake");
    let guest = workspace.write("wild.wasm.o", &baked.module);
    let module = support::link_container_for_program(
        &workspace,
        std::slice::from_ref(&guest),
        &baked.image,
        "longjmp-miss",
        Some(baked.top),
    );
    let mut container = runner::Container::instantiate(
        &std::fs::read(&module).expect("read the container"),
        support::mounts_seeded(&[0x77; 32]),
    )
    .expect("instantiate");

    container
        .boot()
        .expect_err("a call through a wild pointer completed");
    let log = container
        .mounts()
        .read(&[b"iso".to_vec(), b"log".to_vec(), b"error".to_vec()])
        .expect("the log mount failed")
        .unwrap_or_default();
    let log = String::from_utf8_lossy(&log).into_owned();
    assert!(
        log.contains("0x424242") && log.contains("not the address of any translated function"),
        "the kernel did not name the address it could not find: {log}"
    );
}
