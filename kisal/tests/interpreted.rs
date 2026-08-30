//! A real program, interpreted, start to finish.
//!
//! Every other test in this crate drives the kernel directly: it builds
//! arguments and calls `dispatch`. This one loads a program the host
//! compiler built the way a distribution builds one — glibc statically
//! linked, a thousand functions, hand-written assembly, `ifunc` resolvers —
//! and *runs* it, one instruction at a time, until it exits.
//!
//! Nothing translates it and nothing analysed it. The interpreter decodes at
//! the program counter, which is the whole claim `docs/vm.md` makes: the
//! guest's instructions are data.
//!
//! Native, and deliberately so — the same engine, the same kernel, the same
//! address space, without a wasm toolchain in the loop. What the module
//! build adds is a different `Machine::grow` and nothing else.

#![cfg(target_os = "linux")]
#![cfg(target_arch = "x86_64")]

use std::path::{Path, PathBuf};
use std::process::Command;

use kisal::abi::{Shared, Store, StoreOutcome};
use kisal::image::Image;
use kisal::machine::Interpreted;
use kisal::run::{Exit, Process};
use kisal::syscall::{Enforcement, Kernel};
use kisal::system::System;

/// A directory that removes itself.
struct Tree {
    root: PathBuf,
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// What the guest's console reaches.
#[derive(Default)]
struct Console {
    written: Vec<(Vec<Vec<u8>>, Vec<u8>)>,
}

impl Console {
    fn contents(&self, path: &[&[u8]]) -> Vec<u8> {
        let key: Vec<Vec<u8>> = path.iter().map(|part| part.to_vec()).collect();
        self.written
            .iter()
            .filter(|(written, _)| *written == key)
            .flat_map(|(_, bytes)| bytes.iter().copied())
            .collect()
    }
}

impl Store for Console {
    fn read(&mut self, _path: &[&[u8]], _into: &mut Vec<u8>) -> StoreOutcome {
        StoreOutcome::Absent
    }

    fn write(&mut self, path: &[&[u8]], bytes: &[u8]) -> StoreOutcome {
        self.written.push((
            path.iter().map(|part| part.to_vec()).collect(),
            bytes.to_vec(),
        ));
        StoreOutcome::Present
    }
}

/// Flags every corpus program is built with.
///
/// They switch off features that are explicit non-goals, and link the math
/// library so that its own code is part of what runs.
const COMMON: &[&str] = &[
    "-fcf-protection=none",
    "-fno-stack-protector",
    "-fno-asynchronous-unwind-tables",
    "-fno-math-errno",
    "-lm",
    "-pthread",
];

/// How a program is linked, which decides what has to work to run it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Linkage {
    /// `-static -no-pie`: the whole image at addresses it states itself.
    /// Nothing places it and nothing relocates it.
    Static,
    /// An ordinary dynamic executable, which is what a distribution ships.
    /// `PT_INTERP` names a loader, the loader maps the libraries, applies
    /// their relocations — writing to pages it is about to execute — and
    /// only then jumps to the program.
    Dynamic,
}

impl Linkage {
    fn flags(self) -> &'static [&'static str] {
        match self {
            Linkage::Static => &["-static", "-no-pie"],
            Linkage::Dynamic => &[],
        }
    }
}

/// Whether an ELF names a dynamic loader, read straight out of its program
/// headers — the one fact that distinguishes the two linkages, taken from
/// the file rather than from what the test meant to build.
fn elf_has_interpreter(bytes: &[u8]) -> bool {
    const PT_INTERP: u32 = 3;
    let header_offset = u64::from_le_bytes(bytes[32..40].try_into().expect("e_phoff")) as usize;
    let entry_size = u16::from_le_bytes(bytes[54..56].try_into().expect("e_phentsize")) as usize;
    let count = u16::from_le_bytes(bytes[56..58].try_into().expect("e_phnum")) as usize;
    (0..count).any(|index| {
        let at = header_offset + index * entry_size;
        u32::from_le_bytes(bytes[at..at + 4].try_into().expect("p_type")) == PT_INTERP
    })
}

/// Copies a program's shared libraries into the tree at the paths it will
/// ask for them by.
///
/// `ldd` names them, including the dynamic loader itself, and each is copied
/// to the same absolute path inside the image — because that is the path the
/// `PT_INTERP` string and every `DT_NEEDED` entry holds, and the guest's
/// loader resolves them through the guest's own filesystem.
fn copy_libraries(root: &Path, program: &Path, required: bool) {
    let listed = Command::new("ldd").arg(program).output().expect("run ldd");
    assert!(listed.status.success(), "ldd failed on {}", program.display());
    let text = String::from_utf8_lossy(&listed.stdout).into_owned();
    let mut copied = 0;
    for line in text.lines() {
        // Two shapes: "name => /path (0x...)" and "/path (0x...)" for the
        // loader, which nothing points at by name.
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
    assert!(
        !required || copied > 0,
        "no libraries were copied for {}",
        program.display()
    );
}

/// A shared object built into the image beside the program.
struct Plugin {
    /// Where it lands, as an absolute path inside the image — which is the
    /// path the program will hand to `dlopen`, or `execve`.
    path: &'static str,
    source: &'static str,
    /// What to build it as.
    form: Form,
}

/// A companion file is either something to load into the running program or
/// something to run instead of it, and the two are compiled differently.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    /// A shared object, for `dlopen`.
    Library,
    /// A program, for `execve`.
    Program(Linkage),
    /// Not compiled at all: the source is the file's contents, and it is
    /// marked executable. For the one case that needs a file that *is*
    /// there, *may* be run, and is not a program — which is `ENOEXEC`, and
    /// which nothing else in the tree is.
    Data,
    /// A real program with its executable bits cleared, which is the other
    /// thing `execvp` has to step over while walking `PATH`.
    Unmarked,
}

impl Form {
    fn flags(self) -> &'static [&'static str] {
        match self {
            Form::Library => &["-shared", "-fPIC"],
            Form::Program(linkage) => linkage.flags(),
            Form::Data | Form::Unmarked => &[],
        }
    }
}

/// Compiles `source` and bakes it into an image as `/init`.
fn image_of(label: &str, source: &str, linkage: Linkage) -> (Tree, &'static baker::Image) {
    image_with(label, source, linkage, &[])
}

fn image_with(
    label: &str,
    source: &str,
    linkage: Linkage,
    plugins: &[Plugin],
) -> (Tree, &'static baker::Image) {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("kisal-interpreted-{label}-{unique}"));
    std::fs::create_dir_all(&root).expect("mkdir");
    let tree = Tree { root };

    let file = tree.root.join("program.c");
    std::fs::write(&file, source).expect("write the source");
    let program = tree.root.join("init");
    let outcome = Command::new("gcc")
        .arg(&file)
        .args(linkage.flags())
        .args(COMMON)
        .arg("-o")
        .arg(&program)
        .output()
        .expect("run gcc");
    assert!(
        outcome.status.success(),
        "compiling {label} failed:\n{}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    std::fs::remove_file(&file).expect("the source is not part of the image");
    if linkage == Linkage::Dynamic {
        copy_libraries(&tree.root, &program, true);
    }

    // Leaked because the kernel borrows the image for its whole life, and a
    // test's kernel lives as long as the test.
    for plugin in plugins {
        let destination = tree.root.join(plugin.path.trim_start_matches('/'));
        std::fs::create_dir_all(destination.parent().expect("a parent")).expect("mkdir");
        if plugin.form == Form::Data {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&destination, plugin.source).expect("write the data file");
            std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o755))
                .expect("mark it executable");
            continue;
        }
        let file = tree.root.join("plugin.c");
        std::fs::write(&file, plugin.source).expect("write the plugin source");
        let outcome = Command::new("gcc")
            .arg(&file)
            .args(plugin.form.flags())
            .args(COMMON)
            .arg("-o")
            .arg(&destination)
            .output()
            .expect("run gcc");
        assert!(
            outcome.status.success(),
            "compiling {} failed:\n{}",
            plugin.path,
            String::from_utf8_lossy(&outcome.stderr)
        );
        std::fs::remove_file(&file).expect("the source is not part of the image");
        // A plugin may need nothing the program has not already brought.
        copy_libraries(&tree.root, &destination, false);
        if plugin.form == Form::Unmarked {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o644))
                .expect("clear the executable bits");
        }
    }

    let baked: &'static baker::Image =
        Box::leak(Box::new(baker::bake_directory(&tree.root).expect("bake")));
    (tree, baked)
}

fn boot(label: &str, image: Image<'static>) -> System<'static, Shared<Console>> {
    boot_with(label, image, &[b"/init"])
}

fn boot_with(
    label: &str,
    image: Image<'static>,
    argv: &[&[u8]],
) -> System<'static, Shared<Console>> {
    let kernel = Kernel::with_enforcement(
        Shared::new(Console::default()),
        Interpreted::new(),
        image,
        // The interpreter's world: a page is reachable because something
        // mapped it, and an access to anything else is a fault.
        Enforcement::Mapped,
    );
    match Process::boot(kernel, b"/init", argv, &[]) {
        Ok(process) => System::new(process),
        Err(error) => {
            let mut message = String::new();
            error.message(&mut message);
            panic!("{label}: booting failed: {message}");
        }
    }
}

/// What a failure needs to be diagnosable: where the guest was, and what its
/// address space looked like when it got there.
fn report(label: &str, exit: &Exit, system: &mut System<'static, Shared<Console>>) {
    let process = system.current();
    if matches!(exit, Exit::Status(_)) {
        return;
    }
    eprintln!("--- {label}: {exit:?}");
    eprintln!("rsp {:#x}", process.kernel.machine.thread().stack_pointer());
    for vma in process.kernel.space.vmas() {
        eprintln!(
            "  {:#012x}-{:#012x} prot {:#x}",
            vma.start,
            vma.end(),
            vma.prot
        );
    }
}

/// Runs the program interpreted and natively, and requires the two to agree.
///
/// Against the *native run*, not against a constant written into this file.
/// A constant is a second implementation of the program, written by hand,
/// and it is wrong more often than the engine is — twice in the first six
/// programs here, both times my arithmetic rather than the interpreter's.
/// The native binary is the same bytes the interpreter is executing, so
/// what this compares is exactly what it claims to.
fn agrees_with_native(label: &str, source: &str, linkage: Linkage) {
    agrees_with_native_beside(label, source, linkage, &[]);
}

/// The same, with companion files in the image.
fn agrees_with_native_beside(label: &str, source: &str, linkage: Linkage, plugins: &[Plugin]) {
    let (tree, baked) = image_with(label, source, linkage, plugins);
    let native = Command::new(tree.root.join("init"))
        // The tree *is* the guest's root, and the guest starts in it. Which
        // matters the moment a program names a relative path: `./helper`
        // has to mean the same file on both sides for the comparison to be
        // about the engine.
        .current_dir(&tree.root)
        .output()
        .expect("run the program natively");
    assert!(
        native.status.success(),
        "{label} failed natively, so there is nothing to compare against"
    );
    let expected = String::from_utf8(native.stdout).expect("utf-8");
    // Two comparisons that are both empty agree, and prove nothing.
    assert!(!expected.is_empty(), "{label} printed nothing natively");
    // And a "dynamic" program that the compiler linked statically would
    // pass this test while testing none of what it is named for.
    let elf = std::fs::read(tree.root.join("init")).expect("read the program");
    let interpreted = elf_has_interpreter(&elf);
    assert_eq!(
        interpreted,
        linkage == Linkage::Dynamic,
        "{label}: `PT_INTERP` is {}, which is not what {} asked for",
        if interpreted { "present" } else { "absent" },
        match linkage {
            Linkage::Static => "a static link",
            Linkage::Dynamic => "a dynamic link",
        }
    );

    let out = interpreted_output(label, baked);
    assert_eq!(out, expected, "{label}: interpreted output differs");
}

/// Runs a baked image to completion and answers what it printed, requiring
/// it to have exited cleanly.
///
/// For the handful of properties a native run is *not* the oracle for. An
/// orphan is the example: Linux reparents one to the system's `init`, and a
/// container's `init` is its own first process — so the two worlds disagree
/// by design, and comparing them would be comparing this container against
/// the machine it happens to be running on.
fn interpreted_output(label: &str, baked: &'static baker::Image) -> String {
    let image = Image::parse(&baked.index, &baked.blob).expect("parse the image");
    let mut system = boot(label, image);
    let exit = system.run();
    report(label, &exit, &mut system);
    let complaints = String::from_utf8_lossy(
        &system
            .current()
            .kernel
            .store
            .borrow()
            .contents(kisal::paths::CONSOLE_STDERR),
    )
    .into_owned();
    assert_eq!(
        exit,
        Exit::Status(0),
        "{label} did not exit cleanly; it wrote to stderr:\n{complaints}"
    );
    String::from_utf8(
        system
            .current()
            .kernel
            .store
            .borrow()
            .contents(kisal::paths::CONSOLE_STDOUT),
    )
    .expect("utf-8")
}

/// The floor, demonstrated: a program nobody translated, running.
#[test]
fn a_static_program_writes_and_exits() {
    agrees_with_native(
        "write",
        r#"
#include <unistd.h>
int main(void) { write(1, "hello\n", 6); return 0; }
"#,
        Linkage::Static,
    );
}

/// The first rung that is really about *glibc* rather than about one
/// syscall: `printf` is a page of format parsing, a buffered stream, and a
/// `memcpy` chosen by an `ifunc` resolver at startup.
#[test]
fn formatted_output_goes_through_the_library() {
    agrees_with_native(
        "printf",
        r#"
#include <stdio.h>
int main(void) {
    printf("%d %s %c %05.2f %#x\n", 42, "text", 'z', 3.14159, 255);
    return 0;
}
"#,
        Linkage::Static,
    );
}

/// The allocator, which is where `brk` and `mmap` stop being rows and start
/// being a program's working memory.
#[test]
fn the_allocator_grows_and_shrinks() {
    agrees_with_native(
        "malloc",
        r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
int main(void) {
    long total = 0;
    for (int round = 0; round < 8; round++) {
        size_t size = (size_t)1 << (round + 8);
        char *block = malloc(size);
        memset(block, round + 1, size);
        for (size_t index = 0; index < size; index += 64)
            total += block[index];
        free(block);
    }
    /* Large enough that the allocator reaches for `mmap` instead. */
    char *big = calloc(1, 1 << 20);
    total += big[0] + big[(1 << 20) - 1];
    free(big);
    printf("%ld\n", total);
    return 0;
}
"#,
        Linkage::Static,
    );
}

/// The string routines, which on a static glibc are hand-written assembly
/// selected by an `ifunc` resolver — the exact code the discovery front end
/// has the hardest time with, and which the interpreter never has to find.
#[test]
fn the_string_routines_run() {
    agrees_with_native(
        "strings",
        r#"
#include <stdio.h>
#include <string.h>
int main(void) {
    char buffer[128];
    strcpy(buffer, "the quick brown fox");
    printf("%zu %d %s\n",
           strlen(buffer),
           strcmp(buffer, "the quick brown foy"),
           strchr(buffer, 'b'));
    char sorted[] = "zyxwvu";
    for (int i = 0; i < 3; i++) {
        char t = sorted[i];
        sorted[i] = sorted[5 - i];
        sorted[5 - i] = t;
    }
    printf("%s %d\n", sorted, memcmp("abc", "abd", 3));
    return 0;
}
"#,
        Linkage::Static,
    );
}

/// Double-precision arithmetic, which is SSE, and the library's own
/// formatting of it, which is not.
#[test]
fn floating_point_arithmetic_agrees_with_the_library() {
    agrees_with_native(
        "double",
        r#"
#include <stdio.h>
#include <math.h>
int main(void) {
    double value = 2.0;
    for (int i = 0; i < 10; i++) value = value * 1.5 - 0.25;
    printf("%.10f %.10f %.10f\n", value, sqrt(value), value / 7.0);
    return 0;
}
"#,
        Linkage::Static,
    );
}

/// The x87, reached the only way C reaches it — and through glibc's own
/// extended-precision paths in `printf`, which are `fprem`-driven scaling
/// and control-word manipulation rather than a demonstration.
#[test]
fn extended_precision_goes_through_the_unit() {
    agrees_with_native(
        "long-double",
        r#"
#include <stdio.h>
#include <stdlib.h>
int main(void) {
    long double value = strtold("3.14159265358979323846", NULL);
    printf("%.21Lg\n%.21Lg\n", value, value * value);
    return 0;
}
"#,
        Linkage::Static,
    );
}

/// The rung the ahead-of-time design cannot climb without a bake that
/// assigned every base in advance: an ordinary dynamic executable.
///
/// Nothing here was placed by anybody. `PT_INTERP` names a loader, the
/// loader is a position-independent object the address space finds room
/// for, it maps `libc` for itself, applies relocations — *writing to pages
/// it is about to execute*, which is the case the block cache's
/// invalidation exists for — and only then jumps to the program's entry.
/// The interpreter decodes whatever is at the program counter, so none of
/// it needs to have been understood ahead of time.
#[test]
fn a_dynamic_program_runs_with_its_loader_interpreted() {
    agrees_with_native(
        "dynamic",
        r#"
#include <stdio.h>
int main(void) {
    printf("dynamic %d\n", 7 * 6);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// `dlopen`, which is the shape a real workload actually has.
///
/// A Python extension module, a codec plugin, a wheel installed at run time
/// — all of them are this: a shared object nobody named at link time, opened
/// by path, relocated into a fresh mapping, and called. The ahead-of-time
/// design has to refuse it outright, because a bake can only translate code
/// that existed at bake time and there is no such thing here. The
/// interpreter needs nothing from anyone: the pages get their permission
/// bits and the first fetch decodes them.
#[test]
fn a_dlopened_object_is_loaded_and_called() {
    let label = "dlopen";
    let (tree, baked) = image_with(
        label,
        r#"
#include <dlfcn.h>
#include <stdio.h>
int main(int argc, char **argv) {
    /* The path comes from the command line because the two runs see
       different filesystems: the guest's root is the image, and the native
       run's is the host's. */
    void *handle = argc > 1 ? dlopen(argv[1], RTLD_NOW) : 0;
    if (!handle) { printf("dlopen failed: %s\n", dlerror()); return 1; }
    int (*answer)(int) = dlsym(handle, "answer");
    if (!answer) { printf("dlsym failed\n"); return 1; }
    printf("plugin says %d\n", answer(6));
    dlclose(handle);
    return 0;
}
"#,
        Linkage::Dynamic,
        &[Plugin {
            form: Form::Library,
            path: "/plugin.so",
            source: r#"
int answer(int value) { return value * 7; }
"#,
        }],
    );

    let native = Command::new(tree.root.join("init"))
        .arg(tree.root.join("plugin.so"))
        .output()
        .expect("run natively");
    let expected = String::from_utf8(native.stdout).expect("utf-8");
    assert!(
        expected.starts_with("plugin says 42"),
        "the program does not work natively: {expected:?}"
    );

    let image = Image::parse(&baked.index, &baked.blob).expect("parse the image");
    let mut process = boot_with(label, image, &[b"/init", b"/plugin.so"]);
    let exit = process.run();
    report(label, &exit, &mut process);
    let complaints =
        String::from_utf8_lossy(&process.current().kernel.store.borrow().contents(kisal::paths::CONSOLE_STDERR))
            .into_owned();
    assert_eq!(
        exit,
        Exit::Status(0),
        "{label} did not exit cleanly; it wrote to stderr:\n{complaints}"
    );
    let out = String::from_utf8(process.current().kernel.store.borrow().contents(kisal::paths::CONSOLE_STDOUT))
        .expect("utf-8");
    assert_eq!(out, expected);
}

/// Threads, which the ahead-of-time path defers and the loop gets by
/// scheduling a different control block.
///
/// Four of them, each doing arithmetic and returning a value the joiner
/// collects — so the test fails if a thread never ran, if two threads shared
/// a stack, if thread-local storage was the parent's, or if `pthread_join`
/// returned before the thread did. All four are the same defect from a
/// distance: a context switch that carried the wrong state.
#[test]
fn threads_run_and_are_joined() {
    agrees_with_native(
        "threads",
        r#"
#include <pthread.h>
#include <stdio.h>

/* Thread-local, so a thread that was handed the parent's `%fs` base reads
   somebody else's counter and the total comes out wrong. */
static __thread long mine;

static void *work(void *argument) {
    long index = (long)argument;
    mine = 0;
    for (long step = 1; step <= 1000; step++)
        mine += index * step;
    return (void *)mine;
}

int main(void) {
    pthread_t threads[4];
    long total = 0;
    for (long index = 0; index < 4; index++)
        pthread_create(&threads[index], 0, work, (void *)(index + 1));
    for (int index = 0; index < 4; index++) {
        void *answer;
        pthread_join(threads[index], &answer);
        total += (long)answer;
    }
    printf("total %ld\n", total);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// A mutex, which is where a futex actually blocks.
///
/// Uncontended locking never reaches the kernel — the fast path is a
/// compare-and-swap and nothing else — so this contends deliberately: four
/// threads incrementing one counter a thousand times each. The final count
/// is the assertion, and it is only right if every increment happened under
/// the lock.
#[test]
fn a_contended_mutex_blocks_and_wakes() {
    agrees_with_native(
        "mutex",
        r#"
#include <pthread.h>
#include <stdio.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static long counter;

static void *work(void *unused) {
    (void)unused;
    for (int step = 0; step < 1000; step++) {
        pthread_mutex_lock(&lock);
        counter++;
        pthread_mutex_unlock(&lock);
    }
    return 0;
}

int main(void) {
    pthread_t threads[4];
    for (int index = 0; index < 4; index++)
        pthread_create(&threads[index], 0, work, 0);
    for (int index = 0; index < 4; index++)
        pthread_join(threads[index], 0);
    printf("counter %ld\n", counter);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// Two compute-bound threads making progress against each other.
///
/// The capability the ahead-of-time path defers: it switches only at
/// syscalls, so a thread that spins without making one holds the processor
/// for ever. Here the quantum is denominated in retired instructions and the
/// loop takes the thread off at the count, whatever it was doing.
///
/// Written so that *nothing but preemption* can finish it. Each side spins
/// on a word the other writes, and neither makes a syscall while spinning —
/// so a scheduler that switches only at syscalls hangs, and one that
/// switches on the quantum returns.
#[test]
fn two_spinning_threads_take_turns() {
    agrees_with_native(
        "preemption",
        r#"
#include <pthread.h>
#include <stdio.h>

static volatile int ready;
static volatile int done;

static void *other(void *unused) {
    (void)unused;
    ready = 1;
    while (!done) { }
    return 0;
}

int main(void) {
    pthread_t thread;
    pthread_create(&thread, 0, other, 0);
    /* Only a preemption can let the other thread reach `ready = 1`. */
    while (!ready) { }
    done = 1;
    pthread_join(thread, 0);
    printf("both threads ran\n");
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// The same container, twice, byte for byte.
///
/// **This is what a quantum in retired instructions buys**, and it is not
/// something a real machine can offer: two threads racing on an unlocked
/// counter lose updates, and *how many* they lose is decided by exactly when
/// the scheduler switched. On Linux that is the wall clock and two runs
/// disagree. Here the switch is a pure function of how many instructions
/// have retired, so the same container with the same inputs loses the same
/// updates in the same places.
///
/// Compared against *itself* rather than against a native run, deliberately:
/// the native answer is not stable, and requiring the two to match would be
/// requiring a race to come out the same way twice on hardware.
#[test]
fn a_racing_container_runs_the_same_way_twice() {
    let source = r#"
#include <pthread.h>
#include <stdio.h>

static volatile long shared;

static void *work(void *unused) {
    (void)unused;
    /* Deliberately unlocked: a read-modify-write that loses updates when a
       switch lands inside it, which is what makes the total observable. */
    for (long step = 0; step < 200000; step++)
        shared = shared + 1;
    return 0;
}

int main(void) {
    pthread_t threads[3];
    for (int index = 0; index < 3; index++)
        pthread_create(&threads[index], 0, work, 0);
    for (int index = 0; index < 3; index++)
        pthread_join(threads[index], 0);
    printf("%ld\n", shared);
    return 0;
}
"#;
    let (tree, baked) = image_of("determinism", source, Linkage::Dynamic);
    let _ = &tree;

    let mut answers = Vec::new();
    for _ in 0..2 {
        let image = Image::parse(&baked.index, &baked.blob).expect("parse the image");
        let mut system = boot("determinism", image);
        let exit = system.run();
        assert_eq!(exit, Exit::Status(0), "the container did not exit cleanly");
        answers.push(
            String::from_utf8(
                system
                    .current()
                    .kernel
                    .store
                    .borrow()
                    .contents(kisal::paths::CONSOLE_STDOUT),
            )
            .expect("utf-8"),
        );
    }
    assert_eq!(answers[0], answers[1], "two runs disagreed");
    let total: i64 = answers[0].trim().parse().expect("a number");
    assert!(
        total > 0 && total < 600000,
        "the threads did not actually race: {total}"
    );
}

/// A handler, run and returned from.
///
/// The simplest case and the one everything else rests on: `raise` makes the
/// signal pending, the loop delivers it at the next block boundary, the
/// handler runs as ordinary guest code, and `sigreturn` puts every register
/// back so the interrupted code carries on as if nothing happened.
#[test]
fn a_signal_handler_runs_and_returns() {
    agrees_with_native(
        "handler",
        r#"
#include <signal.h>
#include <stdio.h>

static volatile sig_atomic_t caught;

static void handler(int signal) { caught = signal; }

int main(void) {
    signal(SIGUSR1, handler);
    /* A value the handler must not disturb: `sigreturn` restores every
       register, and a handler that clobbers one is a program whose caller
       silently loses a value. */
    long keep = 0x1234;
    raise(SIGUSR1);
    printf("caught %d keep %ld\n", (int)caught, keep);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **A `SIGSEGV` a handler catches** — the fidelity class the ahead-of-time
/// design documents as impossible.
///
/// There a null dereference reads whatever happens to be at address zero and
/// carries on. Here the address space refused the access, the loop turned
/// the refusal into a signal with a faithful `si_addr`, and the handler runs
/// and can say which address it was. Compared against a native run, so what
/// is asserted is that Linux and this agree about the number *and* the
/// address.
#[test]
fn a_segmentation_fault_reaches_its_handler() {
    agrees_with_native(
        "sigsegv",
        r#"
#define _GNU_SOURCE
#include <signal.h>
#include <setjmp.h>
#include <stdio.h>

static sigjmp_buf recover;
static volatile void *seen;

static void handler(int signal, siginfo_t *info, void *context) {
    (void)signal;
    (void)context;
    seen = info->si_addr;
    /* Out of the handler and back to a known point — the handler cannot
       simply return, because returning would re-run the instruction that
       faulted and fault again. */
    siglongjmp(recover, 1);
}

int main(void) {
    struct sigaction action = {0};
    action.sa_sigaction = handler;
    action.sa_flags = SA_SIGINFO;
    sigaction(SIGSEGV, &action, 0);

    if (sigsetjmp(recover, 1) == 0) {
        volatile int *nowhere = (int *)0x10;
        *nowhere = 1;
        printf("the store did not fault\n");
        return 1;
    }
    printf("si_addr %p\n", (void *)seen);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// A stack overflow, caught on the alternate stack.
///
/// The case `sigaltstack` exists for and the one that could not arise before
/// the address space had page permissions: the ordinary stack is what
/// overflowed, so a frame pushed onto it would fault again. The handler runs
/// on a stack of its own.
#[test]
fn a_stack_overflow_is_caught_on_the_alternate_stack() {
    agrees_with_native(
        "overflow",
        r#"
#define _GNU_SOURCE
#include <signal.h>
#include <setjmp.h>
#include <stdio.h>
#include <stdlib.h>

static sigjmp_buf recover;

static void handler(int signal) {
    (void)signal;
    siglongjmp(recover, 1);
}

/* Not tail-recursive and not foldable: each frame has to be a frame. */
static long deeper(long depth) {
    volatile char padding[4096];
    padding[0] = (char)depth;
    if (depth > 100000000L) return depth;
    return deeper(depth + 1) + padding[0];
}

int main(void) {
    static char alternate[64 * 1024];
    stack_t stack = { .ss_sp = alternate, .ss_size = sizeof alternate, .ss_flags = 0 };
    sigaltstack(&stack, 0);

    struct sigaction action = {0};
    action.sa_handler = handler;
    action.sa_flags = SA_ONSTACK | SA_NODEFER;
    sigaction(SIGSEGV, &action, 0);

    if (sigsetjmp(recover, 1) == 0) {
        deeper(0);
        printf("no overflow\n");
        return 1;
    }
    printf("caught the overflow\n");
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **`fork`.**
///
/// `container-plan.md` specifies this and treats it as critical; an earlier
/// version of this engine refused it by name, which was reading a milestone
/// boundary as a policy. What the interpreter changes is the *price*: on the
/// other path a fork is a snapshot plus a resume-chain walk to rebuild the
/// parent's frames, and the resume bodies that walk needs are the doubled
/// code section. Here the child's machine state is a control block and its
/// address space is bytes — the child returns from `fork` by being
/// interpreted, which is the only thing the loop ever does.
///
/// The parent and child each write, so the test fails if either never ran;
/// they diverge on the return value, so it fails if the child got the
/// parent's; and the child mutates a variable the parent then prints, so it
/// fails if they share an address space when they must not.
#[test]
fn a_forked_child_runs_and_is_waited_for() {
    agrees_with_native(
        "fork",
        r#"
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

static int shared_by_copy = 100;

int main(void) {
    fflush(stdout);
    pid_t child = fork();
    if (child == 0) {
        /* Only the child sees this. */
        shared_by_copy = 7;
        printf("child sees %d\n", shared_by_copy);
        fflush(stdout);
        _exit(3);
    }
    int status = 0;
    pid_t reaped = waitpid(child, &status, 0);
    printf("parent sees %d, reaped %d, exited %d\n",
           shared_by_copy, reaped == child, WEXITSTATUS(status));
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// Several children, each with its own address space, reaped in turn.
///
/// The shape a shell has and a build system has: fan out, wait for each. It
/// fails if two children share memory, if a status is lost, or if `wait`
/// returns the wrong child.
#[test]
fn several_children_are_reaped_with_their_own_statuses() {
    agrees_with_native(
        "fork-many",
        r#"
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    pid_t children[4];
    for (int index = 0; index < 4; index++) {
        fflush(stdout);
        children[index] = fork();
        if (children[index] == 0) {
            /* Each child computes from its own copy of the loop variable. */
            _exit(index + 1);
        }
    }
    int total = 0;
    for (int index = 0; index < 4; index++) {
        int status = 0;
        waitpid(children[index], &status, 0);
        total += WEXITSTATUS(status) * (index + 1);
    }
    printf("total %d\n", total);
    /* And no children left. */
    printf("echild %d\n", wait(0) == -1);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **`fork` and `execve` together**, which is what a subprocess is.
///
/// A `fork` on its own covers a daemon that splits off a copy of itself. The
/// pair is every subprocess anything actually launches — `subprocess.run`, a
/// shell's `$(...)`, a build system's compiler — and it is the pair that
/// `container-plan.md` treats as critical.
///
/// The child replaces its address space with a second program in the image,
/// that program reads the arguments it was given and picks its own status,
/// and the parent — whose memory must be untouched by any of it — reads the
/// status back. So the test fails if the exec did not happen, if the
/// arguments did not survive the address space that held them, if the status
/// was lost, or if the exec disturbed the parent.
#[test]
fn a_child_execs_a_second_program() {
    agrees_with_native_beside(
        "exec",
        r#"
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    static char untouched[] = "the parent's own memory";
    fflush(stdout);
    pid_t child = fork();
    if (child == 0) {
        char *arguments[] = {"helper", "seven", 0};
        execv("./helper", arguments);
        /* Only reached when the exec failed, and then it is the failure. */
        perror("execv");
        _exit(1);
    }
    int status = 0;
    waitpid(child, &status, 0);
    printf("%s, child exited %d\n", untouched, WEXITSTATUS(status));
    return 0;
}
"#,
        Linkage::Dynamic,
        &[Plugin {
            path: "/helper",
            form: Form::Program(Linkage::Dynamic),
            source: r#"
#include <stdio.h>

int main(int count, char **arguments) {
    printf("helper: %d args, second is %s\n", count, arguments[1]);
    return 9;
}
"#,
        }],
    );
}

/// **`execve` that fails returns to the caller.**
///
/// The property the whole ordering in [`kisal::system`] exists for: `execvp`
/// walks `PATH` calling `execve` once per directory and *depends* on getting
/// `ENOENT` back from every miss. A kernel that tore the address space down
/// before it knew the program would load would make the first miss fatal —
/// and the failure would look like "python3 does not exist" on a machine
/// where it does.
///
/// So this fails on the wrong errno, and it segfaults on the wrong order.
#[test]
fn a_failed_exec_leaves_the_caller_running() {
    agrees_with_native_beside(
        "exec-fails",
        r#"
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(void) {
    char *arguments[] = {"nothing", 0};
    /* Absent. */
    printf("absent: %d\n", execv("/no/such/program", arguments) == -1 ? errno : 0);
    /* There, and not a program. */
    printf("directory: %d\n", execv("/", arguments) == -1 ? errno : 0);
    /* There, may be run, and not a program at all. */
    printf("data: %d\n", execv("./data", arguments) == -1 ? errno : 0);
    /* There, a program, and not marked executable. */
    printf("unmarked: %d\n", execv("./unmarked", arguments) == -1 ? errno : 0);
    /* And the caller is still the caller. */
    printf("still running as %d\n", getpid() > 0);
    return 0;
}
"#,
        Linkage::Dynamic,
        &[
            Plugin {
                path: "/data",
                form: Form::Data,
                source: "not an ELF, and nothing here pretends to be\n",
            },
            Plugin {
                path: "/unmarked",
                form: Form::Unmarked,
                source: "int main(void) { return 0; }\n",
            },
        ],
    );
}

/// **`SIGCHLD`**, which is how a program that is not sitting in `wait4`
/// finds out that a child has finished.
///
/// The other half of reaping: a shell installs this handler and reaps from
/// inside it, so a kernel that only wakes a parked `wait4` leaves that shell
/// with zombies it never hears about. The default is to ignore, so this also
/// fails if a program that never installed a handler is disturbed by one.
#[test]
fn a_child_exiting_raises_sigchld() {
    agrees_with_native(
        "sigchld",
        r#"
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t heard = 0;

static void note(int signal) {
    (void)signal;
    heard++;
}

int main(void) {
    struct sigaction action = {0};
    action.sa_handler = note;
    action.sa_flags = SA_RESTART;
    sigaction(SIGCHLD, &action, 0);

    for (int index = 0; index < 3; index++) {
        fflush(stdout);
        if (fork() == 0) {
            _exit(0);
        }
    }
    /* Reap from the loop, not from the handler: what is being tested is
       that the handler ran at all. */
    while (wait(0) > 0) {
    }
    printf("heard %d\n", heard > 0);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **An orphan is reaped by the first process**, which is what `init` is
/// for.
///
/// A child whose parent exits first is reparented — on Linux to the system's
/// `init`, and here to the container's first process, which is the same role
/// played by the same rule. Without it the grandchild is a zombie whose
/// parent does not exist, `reap` never matches it, and the container ends
/// holding a process nothing can collect.
///
/// Checked directly rather than against a native run, and that is the point
/// rather than a shortcut: run natively, this program's orphan goes to the
/// *host's* `init` and the parent never sees it. The container's answer is
/// the correct one for a container, so the host is not the oracle here.
#[test]
fn an_orphan_is_reparented_to_the_first_process() {
    let (_tree, baked) = image_of(
        "orphan",
        r#"
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    fflush(stdout);
    pid_t child = fork();
    if (child == 0) {
        /* A grandchild that outlives its parent. */
        if (fork() == 0) {
            _exit(5);
        }
        _exit(4);
    }
    int first = 0;
    waitpid(child, &first, 0);
    /* The grandchild is this process's now, so this reaps it. */
    int second = 0;
    pid_t orphan = wait(&second);
    printf("child %d, orphan %d exited %d\n",
           WEXITSTATUS(first), orphan > 0, WEXITSTATUS(second));
    printf("and then %d\n", wait(0) == -1);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
    assert_eq!(
        interpreted_output("orphan", baked),
        "child 4, orphan 1 exited 5\nand then 1\n"
    );
}

/// **A pipe inside one process.**
///
/// The buffer, the two ends, and end-of-file when the writer closes — which
/// is the part a program depends on to stop reading, and the part that fails
/// silently by hanging rather than loudly by being wrong.
#[test]
fn a_pipe_carries_bytes_and_ends() {
    agrees_with_native(
        "pipe",
        r#"
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(void) {
    int ends[2];
    if (pipe(ends) != 0) { perror("pipe"); return 1; }
    const char *message = "through the pipe";
    write(ends[1], message, strlen(message));

    char buffer[64] = {0};
    ssize_t got = read(ends[0], buffer, sizeof buffer);
    printf("read %zd: %s\n", got, buffer);

    /* The reader keeps going until the last writer closes. */
    close(ends[1]);
    printf("eof %zd\n", read(ends[0], buffer, sizeof buffer));
    close(ends[0]);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **A pipe across a fork**, which is what a pipe is for.
///
/// Everything a container could open before this was safe to *copy* into a
/// child: an image file is read-only and a console has no position. A pipe
/// is neither, and this is the test that says so — the child writes into a
/// buffer the parent reads, so it fails if the fork copied the ring instead
/// of sharing it, and it fails if the counts are wrong, because then the
/// parent's read either never ends or ends too early.
#[test]
fn a_pipe_carries_bytes_between_processes() {
    agrees_with_native(
        "pipe-fork",
        r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    int ends[2];
    if (pipe(ends) != 0) { perror("pipe"); return 1; }
    fflush(stdout);
    pid_t child = fork();
    if (child == 0) {
        close(ends[0]);
        for (int index = 0; index < 4; index++) {
            char line[32];
            int length = snprintf(line, sizeof line, "line %d\n", index);
            write(ends[1], line, length);
        }
        close(ends[1]);
        _exit(0);
    }
    /* The parent must close its own write end, or the read never ends:
       there would always be a writer, and that writer would be itself. */
    close(ends[1]);
    char buffer[256];
    size_t total = 0;
    ssize_t got;
    while ((got = read(ends[0], buffer + total, sizeof buffer - total)) > 0) {
        total += (size_t)got;
    }
    buffer[total] = 0;
    close(ends[0]);
    waitpid(child, 0, 0);
    printf("got %zu bytes:\n%s", total, buffer);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **A write with nobody reading**, which is `EPIPE` and `SIGPIPE`.
///
/// The other direction of the same accounting: a reader sees end-of-file
/// when the last writer goes, and a writer is told the same fact as an
/// error. A program that ignores `SIGPIPE` — every one that writes to a pipe
/// it might outlive — sees the errno, which is why the signal has to go
/// through the disposition table rather than straight to the exit.
#[test]
fn a_write_to_a_closed_pipe_is_epipe() {
    agrees_with_native(
        "pipe-broken",
        r#"
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(void) {
    signal(SIGPIPE, SIG_IGN);
    int ends[2];
    pipe(ends);
    close(ends[0]);
    ssize_t wrote = write(ends[1], "nobody is listening", 19);
    printf("wrote %zd, errno %d\n", wrote, wrote < 0 ? errno : 0);
    close(ends[1]);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **More than the pipe holds**, which is where a writer parks mid-transfer.
///
/// A pipe is 64 KiB, so a quarter-megabyte write cannot finish in one go: it
/// moves what fits, parks with a count already on it, and finishes across
/// however many turns the reader needs. That is the case the transfer is a
/// *record* for rather than a syscall to re-run — re-running would move the
/// first piece twice, and the caller would be told the last piece was the
/// whole.
///
/// It fails on a lost byte, a duplicated one, or a count that is not the
/// number written.
#[test]
fn a_write_larger_than_the_pipe_finishes_in_pieces() {
    agrees_with_native(
        "pipe-large",
        r#"
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

#define TOTAL (256 * 1024)

int main(void) {
    static unsigned char sent[TOTAL];
    static unsigned char got[TOTAL];
    for (int index = 0; index < TOTAL; index++) {
        sent[index] = (unsigned char)(index * 31 + (index >> 8));
    }
    int ends[2];
    pipe(ends);
    fflush(stdout);
    pid_t child = fork();
    if (child == 0) {
        close(ends[0]);
        ssize_t wrote = write(ends[1], sent, TOTAL);
        close(ends[1]);
        _exit(wrote == TOTAL ? 0 : 1);
    }
    close(ends[1]);
    size_t total = 0;
    ssize_t piece;
    while (total < TOTAL && (piece = read(ends[0], got + total, TOTAL - total)) > 0) {
        total += (size_t)piece;
    }
    close(ends[0]);
    int status = 0;
    waitpid(child, &status, 0);
    int same = total == TOTAL;
    for (size_t index = 0; same && index < total; index++) {
        same = got[index] == sent[index];
    }
    printf("read %zu, identical %d, writer exited %d\n",
           total, same, WEXITSTATUS(status));
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}
