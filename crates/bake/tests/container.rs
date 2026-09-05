//! A container, end to end: a program baked into an image, linked beside
//! the interpreter and the kernel, and run under wasmtime.
//!
//! The container carries the program as **data** — the same bytes a
//! distribution shipped, in the image, with nothing having read them — and
//! an interpreter that decodes at the program counter. The module is the
//! engine, the image is the program, and a bake is assembly plus a link.

mod support;

use std::path::Path;

use support::WorkingDirectory;

/// Links the image and the guest into one module.
fn link_container(workspace: &WorkingDirectory, baked: &image::Image, label: &str) -> std::path::PathBuf {
    let linked = workspace.path().join(format!("container.{label}.wasm"));
    bake::link(baked, support::guest(), &linked).expect("link the container");
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

    let baked = image::bake_directory(&root).expect("bake");
    let module = link_container(&workspace, &baked, label);
    (workspace, module)
}

/// Boots a module against a given world and answers what it did.
fn boot(module: &Path, mounts: host::store::MountTable) -> (i32, String) {
    let mut container = host::Container::instantiate(
        &std::fs::read(module).expect("read the container"),
        mounts,
    )
    .expect("instantiate the container");

    let status = container
        .boot()
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

/// Everything a run wrote that a host can read back, for comparing two runs.
fn readback(container: &mut host::Container, path: &[&[u8]]) -> String {
    let path: Vec<Vec<u8>> = path.iter().map(|segment| segment.to_vec()).collect();
    let bytes = container.mounts().read(&path).ok().flatten().unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// **The host can hold the machine between turns.** The same module, run
/// to completion in one call and in steps of one quantum, with the console
/// output, the exit status and the retired count compared — and the
/// stepped run has to have taken many steps, or it proved nothing.
///
/// A child that computes, a pipe and a `wait4`, so that the turns being
/// stepped through include a process switch and a parked thread, which is
/// where a scheduler resumed from outside could go wrong.
#[test]
fn a_container_runs_the_same_in_one_call_and_in_steps() {
    let (workspace, module) = module_for(
        "stepped",
        r#"
#include <stdio.h>
#include <unistd.h>
#include <sys/wait.h>
int main(void) {
    int fd[2];
    pipe(fd);
    pid_t child = fork();
    if (child == 0) {
        long sum = 0;
        for (long i = 0; i < 3000000; i++) sum += i * i;
        char text[32];
        int length = snprintf(text, sizeof text, "%ld", sum);
        write(fd[1], text, length);
        _exit(3);
    }
    close(fd[1]);
    char text[64];
    int length = read(fd[0], text, sizeof text - 1);
    text[length] = 0;
    int status;
    waitpid(child, &status, 0);
    printf("child said %s and exited %d\n", text, WEXITSTATUS(status));
    return 0;
}
"#,
        &["-static", "-no-pie"],
    );
    let bytes = std::fs::read(&module).expect("read the container");

    let mut whole = host::Container::instantiate(&bytes, support::mounts_seeded(&[0x33; 32]))
        .expect("instantiate");
    let status = whole.boot().expect("run in one call");
    let (out, stats) = (
        readback(&mut whole, &[b"iso", b"console", b"stdout"]),
        readback(&mut whole, &[b"iso", b"log", b"statistics"]),
    );
    assert_eq!(status, 0);
    assert_eq!(out, "child said 8999995500000500000 and exited 3\n");

    let mut stepped = host::Container::instantiate(&bytes, support::mounts_seeded(&[0x33; 32]))
        .expect("instantiate");
    let mut steps = 0u64;
    let mut asked_while_running = false;
    let stepped_status = loop {
        steps += 1;
        match stepped.step((steps * 100_000) as i64).expect("step") {
            host::Turn::Finished(status) => break status,
            host::Turn::Running | host::Turn::Idle => {}
            host::Turn::Stopped => unreachable!("only stop_at stops"),
        }
        // The container's own store, read while the machine is stopped
        // between turns: the isotope Server Protocol from the outside.
        if steps == 10 {
            let processes = stepped.ask("processes").expect("ask");
            assert!(processes.contains(r#""result":"ok""#), "{processes}");
            assert!(processes.contains(r#""pid":1"#), "{processes}");
            let statistics = stepped.ask("statistics").expect("ask");
            assert!(statistics.contains(r#""retired":"#), "{statistics}");
            let registers = stepped.ask("processes/1/threads/1/registers").expect("ask");
            assert!(registers.contains(r#""rip":"0x"#), "{registers}");
            let missing = stepped.ask("nothing/here").expect("ask");
            assert!(missing.contains(r#""type":"not_found""#), "{missing}");
            asked_while_running = true;
        }
    };
    assert!(steps > 20, "the run took only {steps} steps, so stepping was not exercised");
    assert!(asked_while_running);
    assert_eq!(stepped_status, status);
    assert_eq!(readback(&mut stepped, &[b"iso", b"console", b"stdout"]), out);
    assert_eq!(readback(&mut stepped, &[b"iso", b"log", b"statistics"]), stats);
    // Asking changed nothing the guest could see: the interface the
    // container declared at boot is there too.
    let interface = readback(&mut stepped, &[b"iso", b"self", b"interface"]);
    assert!(interface.contains(r#""name":"zaqaru-container""#), "{interface}");
    drop(workspace);
}

/// **The machine stops on an instruction.** For several targets, a fresh
/// container run to exactly that count reports exactly that count, says
/// whether its flags are the last instruction's, and — run twice — stands
/// in the same place with the same registers.
#[test]
fn a_container_stops_on_the_instruction_asked_for() {
    let (workspace, module) = module_for(
        "stopped",
        r#"
#include <stdio.h>
int main(void) {
    volatile long sum = 0;
    for (long i = 0; i < 400000; i++) sum += i ^ (i >> 3);
    printf("%ld\n", sum);
    return 0;
}
"#,
        &["-static", "-no-pie"],
    );
    let bytes = std::fs::read(&module).expect("read the container");
    let field = |json: &str, name: &str| -> Option<String> {
        let key = format!("\"{name}\":");
        let at = json.find(&key)? + key.len();
        let rest = &json[at..];
        let end = rest.find([',', '}']).unwrap_or(rest.len());
        Some(rest[..end].trim_matches('"').to_string())
    };
    for target in [1i64, 12_345, 137_531, 1_000_003] {
        let mut runs = Vec::new();
        for _ in 0..2 {
            let mut container =
                host::Container::instantiate(&bytes, support::mounts_seeded(&[0x33; 32])).expect("instantiate");
            assert_eq!(container.stop_at(target).expect("stop"), host::Turn::Stopped, "at {target}");
            let statistics = container.ask("statistics").expect("ask");
            assert_eq!(field(&statistics, "retired").as_deref(), Some(target.to_string().as_str()), "{statistics}");
            let registers = container.ask("processes/1/threads/1/registers").expect("ask");
            let stale = field(&registers, "flags_stale").expect("flags_stale is reported");
            assert!(stale == "true" || stale == "false" || stale == "null", "{registers}");
            runs.push(registers);
        }
        assert_eq!(runs[0], runs[1], "two runs to {target} stand in different places");
    }
    // A stop at the end is a finished container.
    let mut container =
        host::Container::instantiate(&bytes, support::mounts_seeded(&[0x33; 32])).expect("instantiate");
    assert!(matches!(container.stop_at(i64::MAX).expect("stop"), host::Turn::Finished(0)));
    drop(workspace);
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

/// The same, for a dynamic program: the loader runs *inside* the module,
/// interpreted, and maps `libc` for itself.
///
/// Two things are in this one test. The address space answers where a
/// shared object goes at load time, the way a kernel does, with nothing
/// decided at bake. And the loader writes relocations into pages it is
/// about to execute, which is the case the block cache's invalidation
/// exists for.
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
/// memory, so the range is *reused* — and `kernel::space::Space` states the
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
    recording.mount(&[b"iso", b"time"], Box::new(host::store::Clock::new()));
    recording.record(tape.clone(), true);
    let (status, first) = boot(&module, recording);
    assert_eq!(status, 0, "the recorded run failed: {first}");
    assert!(tape.is_file(), "nothing was recorded");

    // A different world: another seed, and a clock that has moved on.
    let changed = || {
        let mut mounts = support::mounts_seeded(&[0x77; 32]);
        mounts.mount(&[b"iso", b"time"], Box::new(host::store::Clock::new()));
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

/// **The edge, end to end**: a guest binds a port, the host reaches it, and
/// bytes cross in both directions.
///
/// Everything else about sockets here happens inside one module — two
/// processes in a shared arena, where a connection is two rings and nothing
/// leaves. This is the other network, the one the plan calls host-terminated:
/// a real `TcpListener` on the host, a real client connecting to it, and the
/// guest's `accept` waking because something outside the module arrived.
///
/// It is the piece the demo proves and nothing checked. `-p HOST:GUEST` is
/// also the capability model's firewall — a guest port with no mapping is
/// loopback-only and cannot be reached — so a test that never crosses the
/// boundary cannot tell a working edge from a mapping that silently did
/// nothing, which is what an unopened host listener looks like from inside.
///
/// The host port is taken from the kernel rather than picked: binding zero
/// and reading back what was assigned is what keeps this from failing on a
/// machine where somebody happens to be using the number a test guessed.
#[test]
fn the_host_reaches_a_port_the_guest_published() {
    use std::io::{Read, Write};

    const GUEST_PORT: u16 = 8080;
    // Bound and dropped, purely to be told a free number.
    let host_port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("ask for a free port")
        .local_addr()
        .expect("its address")
        .port();

    let (workspace, module) = module_for(
        "edge",
        r#"
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void) {
    int server = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(server, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in mine = {0};
    mine.sin_family = AF_INET;
    mine.sin_addr.s_addr = htonl(INADDR_ANY);
    mine.sin_port = htons(8080);
    if (bind(server, (struct sockaddr *)&mine, sizeof mine) != 0) {
        perror("bind"); return 1;
    }
    if (listen(server, 8) != 0) { perror("listen"); return 1; }
    /* Bounded, so that an edge which never opens is a failing test with a
       diagnosis rather than a run that hangs: the guest is the only thing
       that can end the container, so it has to be the thing that gives up. */
    struct pollfd waiting = { .fd = server, .events = POLLIN };
    if (poll(&waiting, 1, 30000) <= 0) { printf("nobody came\n"); return 0; }
    int client = accept(server, 0, 0);
    if (client < 0) { perror("accept"); return 1; }
    char asked[64] = {0};
    ssize_t got = read(client, asked, sizeof asked - 1);
    printf("read %zd: %s", got, asked);
    write(client, "pong\n", 5);
    close(client);
    close(server);
    return 0;
}
"#,
        &["-static", "-no-pie"],
    );

    // The client runs while the container does, because the container does
    // not return until the guest has served it — which is the whole shape
    // of the thing being tested.
    let client = std::thread::spawn(move || -> Result<String, String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if std::time::Instant::now() > deadline {
                return Err("the guest never answered on the host port".into());
            }
            // Refused until the guest has listened and the store has opened
            // the host listener behind it, which is not instant: the module
            // is interpreting a dynamic loader and a libc start-up first.
            let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", host_port)) else {
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            };
            // Bounded, for the same reason the guest's `poll` is: the host
            // listener is opened by the store the moment the guest
            // registers its port, so a connection can be accepted *here*
            // and never reach the guest at all — and a read waiting for an
            // end of file that is not coming is a hung test with nothing
            // to say.
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(30)))
                .map_err(|e| e.to_string())?;
            stream.write_all(b"ping\n").map_err(|e| e.to_string())?;
            let mut back = String::new();
            stream.read_to_string(&mut back).map_err(|e| e.to_string())?;
            return Ok(back);
        }
    });

    let mut mounts = support::mounts_seeded(&[0x33; 32]);
    // A clock, because the guest's `poll` has a timeout and a deadline with
    // nothing to measure it against is refused rather than ignored.
    mounts.mount(&[b"iso", b"time"], Box::new(host::store::Clock::new()));
    mounts.mount(
        &[b"iso", b"net"],
        Box::new(host::net::NetStore::new(vec![(host_port, GUEST_PORT)])),
    );
    let (status, printed) = boot(&module, mounts);
    let heard = client.join().expect("the client thread");

    // The guest's own account first. It is the side that can say whether
    // the connection ever crossed, so a client error reported ahead of it
    // describes a symptom where the guest has the diagnosis.
    assert_eq!(status, 0, "the guest failed: {printed}");
    assert_eq!(
        printed, "read 5: ping\n",
        "the guest did not read the request; the host end said {heard:?}"
    );
    assert_eq!(heard.as_deref(), Ok("pong\n"), "the host did not read the reply");
    drop(workspace);
}

/// **A half-close reaches the host too.**
///
/// `shutdown(SHUT_WR)` is how a server says "that is the whole response"
/// while still reading — the peer sees end of file on its side and the
/// connection stays open on the other. Inside the module that is one ring
/// reference going away, which the arena has always handled. Across the
/// edge it is a FIN that only the host can send, and the guest has to ask.
///
/// Separate from the full close rather than folded into it, because a
/// half-close that quietly did nothing looks exactly like one that worked
/// right up until the guest closes for other reasons and the peer is
/// finally released — which is to say, it looks fine in any test whose
/// guest exits.
#[test]
fn a_half_close_reaches_the_host() {
    use std::io::{Read, Write};

    let host_port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("ask for a free port")
        .local_addr()
        .expect("its address")
        .port();

    let (workspace, module) = module_for(
        "halfclose",
        r#"
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void) {
    int server = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in mine = {0};
    mine.sin_family = AF_INET;
    mine.sin_addr.s_addr = htonl(INADDR_ANY);
    mine.sin_port = htons(8080);
    if (bind(server, (struct sockaddr *)&mine, sizeof mine) != 0) {
        perror("bind"); return 1;
    }
    if (listen(server, 8) != 0) { perror("listen"); return 1; }
    struct pollfd waiting = { .fd = server, .events = POLLIN };
    if (poll(&waiting, 1, 30000) <= 0) { printf("nobody came\n"); return 0; }
    int client = accept(server, 0, 0);
    if (client < 0) { perror("accept"); return 1; }
    write(client, "half\n", 5);
    /* The whole of the response, said without closing: the peer reads end
       of file and this end can still read. */
    if (shutdown(client, SHUT_WR) != 0) { perror("shutdown"); return 1; }
    /* And it can: the client sends this only after it has seen the end of
       file, so a reply here proves the connection outlived the FIN. */
    char last[16] = {0};
    /* Bounded like the accept, so a half-close that stopped reaching the
       host fails this test with a diagnosis instead of parking here while
       the client waits for an end of file that is not coming. */
    struct pollfd reply = { .fd = client, .events = POLLIN };
    if (poll(&reply, 1, 30000) <= 0) { printf("no reply\n"); return 0; }
    ssize_t got = read(client, last, sizeof last - 1);
    printf("after %zd: %s", got, last);
    close(client);
    close(server);
    return 0;
}
"#,
        &["-static", "-no-pie"],
    );

    let client = std::thread::spawn(move || -> Result<String, String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if std::time::Instant::now() > deadline {
                return Err("the guest never answered on the host port".into());
            }
            let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", host_port)) else {
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            };
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(30)))
                .map_err(|e| e.to_string())?;
            // Reads to end of file. If the half-close never reached the
            // host this blocks until the timeout, because the guest is
            // still sitting in its own read and will not close.
            let mut back = String::new();
            stream.read_to_string(&mut back).map_err(|e| e.to_string())?;
            stream.write_all(b"bye\n").map_err(|e| e.to_string())?;
            return Ok(back);
        }
    });

    let mut mounts = support::mounts_seeded(&[0x33; 32]);
    mounts.mount(&[b"iso", b"time"], Box::new(host::store::Clock::new()));
    mounts.mount(
        &[b"iso", b"net"],
        Box::new(host::net::NetStore::new(vec![(host_port, 8080)])),
    );
    let (status, printed) = boot(&module, mounts);
    let heard = client.join().expect("the client thread");

    assert_eq!(status, 0, "the guest failed: {printed}");
    assert_eq!(
        heard.as_deref(),
        Ok("half\n"),
        "the host did not see the response end"
    );
    assert_eq!(
        printed, "after 4: bye\n",
        "the guest could not read after half-closing"
    );
    drop(workspace);
}
