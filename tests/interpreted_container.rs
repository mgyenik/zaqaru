//! A container that carries no translated code at all, and runs anyway.
//!
//! Every other container in this suite is a bake's output: the translator
//! turned a program's functions into wasm functions, the linker welded them
//! together, and the module *is* the program. This one carries the program
//! as **data** — the same bytes a distribution shipped, in the image, with
//! nothing having read them — and an interpreter that decodes at the program
//! counter.
//!
//! What that changes about the artifact is the whole point of `docs/vm.md`:
//! the module is the engine, the image is the program, and a bake is
//! assembly plus a link rather than a translation.

mod support;

use std::path::Path;

use support::WorkingDirectory;

/// Builds the engine's staticlib for wasm32.
fn targum_staticlib() -> std::path::PathBuf {
    support::wasm_staticlib("targum", "libtargum.a")
}

/// Links engine + kernel + FPU + image into one module.
///
/// Shorter than the ahead-of-time link by everything that made that one
/// hard: no seam object, because a syscall is a Rust call; no translated
/// guest objects, because there is no translation; no exec map and no
/// resume bodies, because nothing holds guest state on the wasm stack.
fn link_engine(workspace: &WorkingDirectory, image: &[u8], label: &str) -> std::path::PathBuf {
    let image = workspace.write(&format!("image.{label}.wasm.o"), image);
    let objects = vec![
        image,
        support::kisal_staticlib(),
        targum_staticlib(),
        support::x87_staticlib(),
    ];
    let linked = workspace.path().join(format!("engine.{label}.wasm"));
    support::link_wasm(
        &objects,
        &linked,
        &[
            "--fatal-warnings",
            "--export=cabi_realloc",
            "--export=targum_boot",
            // The guest's program is loaded low, so the module's own data
            // has to be above it. Sixty-four megabytes of room, which is
            // what the native side reserves for the same reason.
            "--global-base=67108864",
        ],
    );
    linked
}

/// Copies a program's shared libraries into the tree at the absolute paths
/// it will ask for them by — which is what `PT_INTERP` and every
/// `DT_NEEDED` entry holds, and what the guest's loader resolves through the
/// guest's own filesystem.
fn copy_libraries(root: &Path, program: &Path) {
    let listed = std::process::Command::new("ldd")
        .arg(program)
        .output()
        .expect("run ldd");
    assert!(listed.status.success(), "ldd failed on {}", program.display());
    let text = String::from_utf8_lossy(&listed.stdout).into_owned();
    let mut copied = 0;
    for line in text.lines() {
        let path = match line.split_whitespace().collect::<Vec<_>>()[..] {
            [_, "=>", path, ..] => path,
            [path, ..] if path.starts_with('/') => path,
            _ => continue,
        };
        let source = Path::new(path);
        if !source.is_file() {
            continue;
        }
        let destination = root.join(path.trim_start_matches('/'));
        std::fs::create_dir_all(destination.parent().expect("a parent")).expect("mkdir");
        std::fs::copy(source, &destination).expect("copy a library");
        copied += 1;
    }
    assert!(copied > 0, "no libraries were copied");
}

/// A program, baked with the engine, run under wasmtime.
fn run(label: &str, source: &str) -> (i32, String) {
    run_linked(label, source, &["-static", "-no-pie"])
}

fn run_linked(label: &str, source: &str, linkage: &[&str]) -> (i32, String) {
    let (workspace, module) = module_for(label, source, linkage);
    let outcome = boot(&module, support::mounts_seeded(&[0x33; 32]));
    drop(workspace);
    outcome
}

/// Builds a program into a container module, and hands back both it and the
/// workspace holding it — which the caller has to keep alive, because the
/// module is a file in it.
fn module_for(
    label: &str,
    source: &str,
    linkage: &[&str],
) -> (WorkingDirectory, std::path::PathBuf) {
    let workspace = WorkingDirectory::new(label);
    let root = workspace.path().join("root");
    std::fs::create_dir_all(&root).expect("mkdir");
    let file = root.join("program.c");
    std::fs::write(&file, source).expect("write the source");
    let program = root.join("init");
    let built = std::process::Command::new("gcc")
        .arg(&file)
        .args(linkage)
        .args([
            "-fcf-protection=none",
            "-fno-stack-protector",
            "-fno-asynchronous-unwind-tables",
        ])
        .arg("-o")
        .arg(&program)
        .output()
        .expect("run gcc");
    assert!(
        built.status.success(),
        "compiling {label} failed:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
    std::fs::remove_file(&file).expect("the source is not part of the image");
    if !linkage.contains(&"-static") {
        copy_libraries(&root, &program);
    }

    let baked = baker::bake_directory(&root).expect("bake");
    let object = baker::object::emit(&baked).expect("emit the image object");
    let module = link_engine(&workspace, &object, label);
    (workspace, module)
}

/// Boots a module against a given world and answers what it did.
fn boot(module: &Path, mounts: runner::store::MountTable) -> (i32, String) {
    let mut container = runner::Container::instantiate(
        &std::fs::read(module).expect("read the container"),
        mounts,
    )
    .expect("instantiate the container");

    let status = container
        .call::<(), i32>("targum_boot", ())
        .unwrap_or_else(|error| {
            let log = container
                .mounts()
                .read(&[b"iso".to_vec(), b"log".to_vec(), b"error".to_vec()])
                .ok()
                .flatten()
                .unwrap_or_default();
            panic!(
                "the container did not finish: {error:?}\nkernel log: {}",
                String::from_utf8_lossy(&log)
            )
        });

    let written = container
        .mounts()
        .read(&[b"iso".to_vec(), b"console".to_vec(), b"stdout".to_vec()])
        .ok()
        .flatten()
        .unwrap_or_default();
    // Written out if this world was recording; `None` if it was not.
    if let Some(kept) = container.mounts().keep_tape() {
        kept.expect("write the tape");
    }
    (status, String::from_utf8(written).expect("utf-8"))
}

/// The artifact the design is about: engine plus image, and a program the
/// bake never looked at.
#[test]
fn a_program_the_bake_never_translated_runs_in_a_module() {
    let (status, out) = run(
        "hello",
        r#"
#include <stdio.h>
int main(void) {
    printf("%s %d\n", "interpreted", 6 * 7);
    return 0;
}
"#,
    );
    assert_eq!(status, 0, "the container did not exit cleanly");
    assert_eq!(out, "interpreted 42\n");
}

/// The same, for a program nothing placed and nothing translated: the
/// dynamic loader runs *inside* the module, interpreted, and maps `libc` for
/// itself.
///
/// Two things the ahead-of-time path cannot do are in this one test. There
/// is no prelink — no bases assigned at bake, no modules region, no forced
/// placement — because the address space answers where a shared object goes
/// at load time, the way a kernel does. And the loader writes relocations
/// into pages it is about to execute, which is the case the block cache's
/// invalidation exists for.
#[test]
fn a_dynamic_program_and_its_loader_run_in_a_module() {
    let (status, out) = run_linked(
        "dynamic",
        r#"
#include <stdio.h>
#include <string.h>
int main(void) {
    char buffer[64];
    snprintf(buffer, sizeof buffer, "%s %d", "loaded", 6 * 7);
    printf("%s %zu\n", buffer, strlen(buffer));
    return 0;
}
"#,
        &[],
    );
    assert_eq!(status, 0, "the container did not exit cleanly");
    assert_eq!(out, "loaded 42 9\n");
}

/// **Twelve programs in a row, each seeing a clean address space.**
///
/// A property only the module can lose, so only the module can test it.
/// Natively a process's bytes live in a file of its own and a fresh one is
/// genuinely fresh. Inside the module every process shares the one linear
/// memory, so the range is *reused* — and `kisal::space::Space` states the
/// invariant its fill discipline rests on: above a fresh address space's
/// high-water mark, memory "is freshly grown and therefore zero", so nothing
/// there is zeroed before the guest sees it.
///
/// Two ways that was broken, and both negative controls were run rather
/// than reasoned about:
///
/// - Carving a *fresh* region off the top of memory per process instead of
///   sharing one spends half a gigabyte of the module's four per `execve`.
///   With `Machine::guest_base` put back to `memory_limit()`, this fails
///   with "generation 1 could not reserve" — the ninth program has nowhere
///   to put its address space. Which is how the defect was found in the
///   first place, in a container that ran `python`, a captured subprocess,
///   a shell pipeline, `uname` and `ls | wc`, and then stopped.
/// - Sharing the range without clearing it hands the next program's `brk`,
///   its stack and its anonymous `mmap`s whatever the last one left there.
///   With `Dormant::taken` no longer zeroing, this fails with "generation 8
///   found bytes in a fresh mapping".
///
/// So each generation checks that its `.bss` and a fresh mapping are zero,
/// reserves enough address space that a per-process region would run out,
/// and then fills both with a pattern for the next generation to find if it
/// can.
#[test]
fn a_chain_of_programs_each_get_a_clean_address_space() {
    let (status, out) = run(
        "exec-chain",
        r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

/* A quarter-megabyte, which is enough to reach past whatever the last
   program's own start left behind and small enough that twelve rounds of
   it stay inside a unit test's second. */
#define SPAN (1 << 18)

/* Zero by the C standard — so a non-zero byte here is somebody else's. */
static unsigned char zeroed[SPAN];

/* A word at a time, not a byte: the byte loop is the whole cost of this
   test when it is run twelve times through an interpreter. */
static int all_zero(const void *bytes, size_t length) {
    const unsigned long *words = bytes;
    for (size_t index = 0; index < length / sizeof *words; index++) {
        if (words[index] != 0) {
            return 0;
        }
    }
    return 1;
}

int main(int count, char **arguments) {
    long left = count > 1 ? strtol(arguments[1], 0, 10) : 9;

    /* A reservation big enough that the region a process is given is
       genuinely spent. Unreadable, so nothing is copied when this process
       stops being the current one — the point is the *address space*, which
       is what a scheme that hands every process a fresh region runs out of.
       Without sharing, the eighth program has nowhere to load. */
    if (mmap(0, 200 << 20, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE,
             -1, 0) == MAP_FAILED) {
        printf("generation %ld could not reserve\n", left);
        return 1;
    }
    unsigned char *fresh = mmap(0, SPAN, PROT_READ | PROT_WRITE,
                                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (fresh == MAP_FAILED) {
        printf("generation %ld could not map\n", left);
        return 1;
    }
    if (!all_zero(zeroed, SPAN)) {
        printf("generation %ld found bytes in its bss\n", left);
        return 1;
    }
    if (!all_zero(fresh, SPAN)) {
        printf("generation %ld found bytes in a fresh mapping\n", left);
        return 1;
    }
    /* Leave something for the next one to find, if it can. */
    memset(zeroed, 0x5a, SPAN);
    memset(fresh, 0xa5, SPAN);

    if (left > 0) {
        char next[16];
        snprintf(next, sizeof next, "%ld", left - 1);
        char *forward[] = {arguments[0], next, 0};
        execv("/init", forward);
        printf("generation %ld could not exec\n", left);
        return 1;
    }
    printf("twelve generations, every page clean\n");
    return 0;
}
"#,
    );
    assert_eq!(status, 0, "the container did not exit cleanly: {out}");
    assert_eq!(out, "twelve generations, every page clean\n");
}

/// **A fork and a pipe, inside the module.**
///
/// The other half of the same machinery. Natively a switch between two
/// processes is one `MAP_FIXED` of a file; here it is a copy of the pages
/// the page table describes, and a fork is a copy taken while the parent
/// goes on running. So this fails if the child got a *reference* to the
/// parent's memory rather than a copy, if the copy was taken destructively,
/// or if what a process writes while it is not the current one goes
/// anywhere at all.
#[test]
fn a_fork_and_a_pipe_run_in_a_module() {
    let (status, out) = run(
        "fork-pipe",
        r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static char shared_by_copy[64] = "the parent's";

int main(void) {
    int ends[2];
    if (pipe(ends) != 0) {
        printf("no pipe\n");
        return 1;
    }
    fflush(stdout);
    pid_t child = fork();
    if (child == 0) {
        close(ends[0]);
        /* Only the child sees this, and the parent prints its own after. */
        strcpy(shared_by_copy, "the child's");
        write(ends[1], shared_by_copy, strlen(shared_by_copy));
        close(ends[1]);
        _exit(7);
    }
    close(ends[1]);
    char buffer[64] = {0};
    size_t total = 0;
    ssize_t got;
    while ((got = read(ends[0], buffer + total, sizeof buffer - 1 - total)) > 0) {
        total += (size_t)got;
    }
    close(ends[0]);
    int status = 0;
    waitpid(child, &status, 0);
    printf("child said %s, parent still has %s, exited %d\n",
           buffer, shared_by_copy, WEXITSTATUS(status));
    return 0;
}
"#,
    );
    assert_eq!(status, 0, "the container did not exit cleanly: {out}");
    assert_eq!(
        out,
        "child said the child's, parent still has the parent's, exited 7\n"
    );
}

/// **A run records, and replays byte for byte against a different world.**
///
/// This is the determinism claim, which the demo demonstrates with a served
/// HTTP session and which nothing until now checked. Everything a container
/// does is a function of its own execution and the answers the host gave it,
/// so keeping the answers keeps the run: replayed, the same module reaches
/// the same bytes with no clock and no entropy behind it.
///
/// The world is deliberately *changed* under the replay — a different seed
/// and a fresh clock — because a replay that agrees with the world it was
/// recorded against has not been shown to be reading the tape. The third
/// run is the control: the same change, without a tape, has to disagree.
/// Without it this test would pass on any program whose output never
/// varied, which is most of them.
///
/// The clock is the sharper half. A guest here reads it through the vDSO,
/// where glibc interpolates from a timebase page rather than asking, so
/// what has to be deterministic is the timebase's own refresh and the
/// retired-instruction counter it extrapolates against — a path with no
/// syscall in it at all.
#[test]
fn a_run_records_and_replays_against_a_changed_world() {
    let source = r#"
#include <stdio.h>
#include <sys/random.h>
#include <time.h>

int main(void) {
    unsigned char bytes[8] = {0};
    if (getrandom(bytes, sizeof bytes, 0) != sizeof bytes) {
        printf("no entropy\n");
        return 1;
    }
    for (unsigned i = 0; i < sizeof bytes; i++) printf("%02x", bytes[i]);
    printf("\n");
    struct timespec now;
    clock_gettime(CLOCK_REALTIME, &now);
    printf("%lld.%09ld\n", (long long)now.tv_sec, now.tv_nsec);
    return 0;
}
"#;
    let (workspace, module) = module_for("tape", source, &["-static", "-no-pie"]);
    let tape = workspace.path().join("answers.tape");

    let mut recording = support::mounts_seeded(&[0x33; 32]);
    recording.mount(&[b"iso", b"time"], Box::new(runner::store::Clock::new()));
    recording.record(tape.clone());
    let (status, first) = boot(&module, recording);
    assert_eq!(status, 0, "the recorded run failed: {first}");
    assert!(tape.is_file(), "nothing was recorded");

    // A different world: another seed, and a clock that has moved on.
    let changed = || {
        let mut mounts = support::mounts_seeded(&[0x77; 32]);
        mounts.mount(&[b"iso", b"time"], Box::new(runner::store::Clock::new()));
        mounts
    };

    let mut replaying = changed();
    replaying.replay(&tape).expect("read the tape");
    let (status, replayed) = boot(&module, replaying);
    assert_eq!(status, 0, "the replayed run failed: {replayed}");
    assert_eq!(replayed, first, "the replay diverged from the recording");

    // The control: the same changed world with no tape has to disagree,
    // or the two agreements above were about nothing.
    let (status, without) = boot(&module, changed());
    assert_eq!(status, 0, "the control run failed: {without}");
    assert_ne!(
        without, first,
        "the program's output does not depend on the world, so replaying \
         it proves nothing"
    );
}
