//! A real program, interpreted, start to finish.
//!
//! Every other test in this crate drives the kernel directly: it builds
//! arguments and calls `dispatch`. This one loads a program the host
//! compiler built the way a distribution builds one — glibc statically
//! linked, a thousand functions, hand-written assembly, `ifunc` resolvers —
//! and *runs* it, one instruction at a time, until it exits.
//!
//! Nothing analysed it. The interpreter decodes at the program counter: the
//! guest's instructions are data.
//!
//! Native, and deliberately so — the same engine, the same kernel, the same
//! address space, without a wasm toolchain in the loop. What the module
//! build adds is a different `Machine::grow` and nothing else.

#![cfg(target_os = "linux")]
#![cfg(target_arch = "x86_64")]

use std::path::{Path, PathBuf};
use std::process::Command;

use cpu::block::BlockCache;
use kernel::abi::{Shared, Store, StoreOutcome};
use kernel::image::Image;
use kernel::machine::Interpreted;
use kernel::run::{Exit, Process};
use kernel::syscall::{Enforcement, Kernel};
use kernel::system::{System, Turn};

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
    /// Whether the container should trace its syscalls, which is how a test
    /// counts them.
    tracing: bool,
    /// A batch of Server Protocol Requests for the kernel's next read of
    /// `/iso/server/requests/pending`, as the host would queue them.
    pending: Option<Vec<u8>>,
    /// Whether this container has been asked to stop.
    ///
    /// The host's half of a shutdown, which is a *read* the guest makes
    /// rather than anything pushed at it — the boundary has no way to push.
    /// A test sets it and the container notices at the next point it asks
    /// the host anything, which is the same moment `zaqaru-run` would have
    /// it notice a Ctrl-C.
    stopping: bool,
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
    fn read(&mut self, path: &[&[u8]], into: &mut Vec<u8>) -> StoreOutcome {
        if self.stopping && path == kernel::paths::SHUTDOWN_REQUESTED {
            into.push(b'1');
            return StoreOutcome::Present;
        }
        if self.tracing && path == kernel::paths::CONFIG_TRACE {
            into.push(b'1');
            return StoreOutcome::Present;
        }
        if path == kernel::paths::SERVER_PENDING {
            return match self.pending.take() {
                Some(batch) => {
                    into.extend_from_slice(&batch);
                    StoreOutcome::Present
                }
                None => StoreOutcome::Absent,
            };
        }
        // A *fixed* clock, which is a clock: the container gets a usable
        // timebase — so the vDSO is exercised rather than skipped — and two
        // runs of one container still agree, which
        // `a_racing_container_runs_the_same_way_twice` is about. Time still
        // advances between samples, because the vDSO derives it from the
        // retired-instruction counter and only the *base* comes from here.
        if path == kernel::paths::TIME_REALTIME {
            into.extend_from_slice(b"1700000000000000000");
            return StoreOutcome::Present;
        }
        if path == kernel::paths::TIME_MONOTONIC {
            into.extend_from_slice(b"1000000000");
            return StoreOutcome::Present;
        }
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
fn image_of(label: &str, source: &str, linkage: Linkage) -> (Tree, &'static image::Image) {
    image_with(label, source, linkage, &[])
}

fn image_with(
    label: &str,
    source: &str,
    linkage: Linkage,
    plugins: &[Plugin],
) -> (Tree, &'static image::Image) {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("the kernel-interpreted-{label}-{unique}"));
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

    let baked: &'static image::Image =
        Box::leak(Box::new(image::bake_directory(&tree.root).expect("bake")));
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
    boot_with_cache(label, image, argv, BlockCache::new())
}

fn boot_with_cache(
    label: &str,
    image: Image<'static>,
    argv: &[&[u8]],
    cache: BlockCache,
) -> System<'static, Shared<Console>> {
    let kernel = Kernel::with_enforcement(
        Shared::new(Console::default()),
        Interpreted::new(),
        image,
        // The interpreter's world: a page is reachable because something
        // mapped it, and an access to anything else is a fault.
        Enforcement::Mapped,
    );
    match Process::boot_with_cache(kernel, b"/init", argv, &[], cache) {
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
fn interpreted_output(label: &str, baked: &'static image::Image) -> String {
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
            .contents(kernel::paths::CONSOLE_STDERR),
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
            .contents(kernel::paths::CONSOLE_STDOUT),
    )
    .expect("utf-8")
}

/// The simplest case: a static program, running.
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

/// An ordinary dynamic executable.
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
/// by path, relocated into a fresh mapping, and called. The interpreter
/// needs nothing from anyone: the pages get their permission
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
        String::from_utf8_lossy(&process.current().kernel.store.borrow().contents(kernel::paths::CONSOLE_STDERR))
            .into_owned();
    assert_eq!(
        exit,
        Exit::Status(0),
        "{label} did not exit cleanly; it wrote to stderr:\n{complaints}"
    );
    let out = String::from_utf8(process.current().kernel.store.borrow().contents(kernel::paths::CONSOLE_STDOUT))
        .expect("utf-8");
    assert_eq!(out, expected);
}

/// Threads, which the loop gets by scheduling a different control block.
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
/// A scheduler that switched only at syscalls would let a thread that
/// spins without making one hold the processor for ever. Here the quantum
/// is denominated in retired instructions and the
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
///
/// Interpreted, so that a switch can land inside the read-modify-write: the
/// bytecode fuses the three instructions into one op and preempts only at a
/// back-edge, so under it no update is ever lost — a legal interleaving, and
/// a deterministic one (see the next test), but not a race.
#[test]
fn a_racing_container_runs_the_same_way_twice() {
    let answers = racing_container_twice(BlockCache::interpreting);
    assert_eq!(answers[0], answers[1], "two runs disagreed");
    let total: i64 = answers[0].trim().parse().expect("a number");
    assert!(
        total > 0 && total < 600000,
        "the threads did not actually race: {total}"
    );
}

/// The same container under the bytecode accelerator: the schedule is still
/// a pure function of execution, so two runs agree — whatever the total.
#[test]
fn an_accelerated_racing_container_runs_the_same_way_twice() {
    let answers = racing_container_twice(BlockCache::new);
    assert_eq!(answers[0], answers[1], "two runs disagreed");
    let total: i64 = answers[0].trim().parse().expect("a number");
    assert!(total > 0 && total <= 600000, "an impossible total: {total}");
}

/// Runs the racing program twice with caches from `cache`, and answers what
/// each run printed.
fn racing_container_twice(cache: fn() -> BlockCache) -> Vec<String> {
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
        let mut system = boot_with_cache("determinism", image, &[b"/init"], cache());
        let exit = system.run();
        assert_eq!(exit, Exit::Status(0), "the container did not exit cleanly");
        answers.push(
            String::from_utf8(
                system
                    .current()
                    .kernel
                    .store
                    .borrow()
                    .contents(kernel::paths::CONSOLE_STDOUT),
            )
            .expect("utf-8"),
        );
    }
    answers
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

/// **A `SIGSEGV` a handler catches.**
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
/// An earlier version of this engine refused it by name, which was a
/// sequencing choice mistaken for a policy. The interpreter makes it cheap:
/// a fork is a copy of the control block and the address space, where a
/// machine holding guest state on the wasm stack would need to rebuild the
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
/// shell's `$(...)`, a build system's compiler.
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
/// The property the whole ordering in [`kernel::system`] exists for: `execvp`
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

/// **`poll` on a pipe**, including the wait.
///
/// Three questions in one program: what does an empty pipe answer with no
/// timeout, what does it answer once something is in it, and what does the
/// reader hear when the writer closes. The last is `POLLHUP`, reported
/// whether or not it was asked for — which is `poll(2)`'s rule, and the one
/// a program depends on to notice that a child has finished.
#[test]
fn poll_reports_a_pipe_becoming_readable() {
    agrees_with_native(
        "poll",
        r#"
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(void) {
    int ends[2];
    pipe(ends);
    struct pollfd watch = { .fd = ends[0], .events = POLLIN };

    /* Nothing there, and asked not to wait. */
    printf("empty %d %d\n", poll(&watch, 1, 0), watch.revents);

    write(ends[1], "x", 1);
    watch.revents = 0;
    printf("ready %d in=%d\n", poll(&watch, 1, 0), (watch.revents & POLLIN) != 0);

    char byte;
    read(ends[0], &byte, 1);
    close(ends[1]);
    watch.revents = 0;
    /* The writer is gone, so this must return rather than wait forever. */
    printf("hup %d hup=%d\n", poll(&watch, 1, -1), (watch.revents & POLLHUP) != 0);
    close(ends[0]);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **`poll` that actually parks**, and is woken by another process.
///
/// The empty-pipe wait with no timeout, which is the whole reason `poll`
/// blocks: nothing here can run, the container gives the child a turn, and
/// the parent's `poll` answers when the child writes. It fails by hanging if
/// readiness is not what wakes the process, and by returning zero early if
/// the wait is not really a wait.
#[test]
fn a_parked_poll_is_woken_by_another_process() {
    agrees_with_native(
        "poll-fork",
        r#"
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    int ends[2];
    pipe(ends);
    fflush(stdout);
    pid_t child = fork();
    if (child == 0) {
        close(ends[0]);
        write(ends[1], "from the child", 14);
        close(ends[1]);
        _exit(0);
    }
    close(ends[1]);
    struct pollfd watch = { .fd = ends[0], .events = POLLIN };
    int ready = poll(&watch, 1, -1);
    char buffer[32] = {0};
    ssize_t got = read(ends[0], buffer, sizeof buffer - 1);
    close(ends[0]);
    waitpid(child, 0, 0);
    printf("ready %d, read %zd: %s\n", ready, got, buffer);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **`epoll`**, which is the same question with the set kept in the kernel.
///
/// Registering, waiting, the caller's own word coming back untouched, and
/// `EEXIST` for a second `ADD` of one descriptor — the errors `epoll_ctl`
/// has instead of silently replacing a registration.
#[test]
fn epoll_reports_registered_descriptors() {
    agrees_with_native(
        "epoll",
        r#"
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <unistd.h>

int main(void) {
    int ends[2];
    pipe(ends);
    int set = epoll_create1(EPOLL_CLOEXEC);
    if (set < 0) { perror("epoll_create1"); return 1; }

    struct epoll_event watch = { .events = EPOLLIN, .data = { .u64 = 0x1234abcd } };
    printf("add %d\n", epoll_ctl(set, EPOLL_CTL_ADD, ends[0], &watch));
    /* Twice is `EEXIST`, which is how a program learns it already had it.
       The call and the errno are separate statements on purpose: reading
       `errno` in the same `printf` as the call that sets it is unsequenced,
       and the two sides would be comparing evaluation orders. */
    errno = 0;
    int twice = epoll_ctl(set, EPOLL_CTL_ADD, ends[0], &watch);
    printf("again %d %d\n", twice, errno);

    struct epoll_event fired[4];
    printf("quiet %d\n", epoll_wait(set, fired, 4, 0));

    write(ends[1], "wake up", 7);
    int count = epoll_wait(set, fired, 4, -1);
    printf("count %d in=%d data %llx\n", count,
           (fired[0].events & EPOLLIN) != 0,
           (unsigned long long)fired[0].data.u64);

    printf("del %d\n", epoll_ctl(set, EPOLL_CTL_DEL, ends[0], 0));
    printf("gone %d\n", epoll_wait(set, fired, 4, 0));
    close(set);
    close(ends[0]);
    close(ends[1]);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **`close_range`**, and what a process ending does with what it held.
///
/// Two halves of one fact. A child that `close_range`s everything above
/// standard error and then writes is the shape `fork` plus `exec` takes in
/// every subprocess library; and a child that *exits* must let go of its
/// descriptors too, because a zombie is a status and a process id and not an
/// open file. Without the second, the parent's read on a pipe the child held
/// waits for an end-of-file that already happened, which is a hang and not
/// an error.
#[test]
fn an_ending_process_lets_go_of_its_descriptors() {
    agrees_with_native(
        "close-range",
        r#"
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    int ends[2];
    pipe(ends);
    int spare[2];
    pipe(spare);
    fflush(stdout);
    pid_t child = fork();
    if (child == 0) {
        /* Everything but the standard streams and this one pipe. */
        dup2(ends[1], 3);
        close_range(4, ~0U, 0);
        write(3, "from a tidy child", 17);
        /* And then exit *without* closing it: the kernel has to. */
        _exit(0);
    }
    close(ends[1]);
    close(spare[0]);
    close(spare[1]);
    char buffer[64] = {0};
    size_t total = 0;
    ssize_t got;
    while ((got = read(ends[0], buffer + total, sizeof buffer - 1 - total)) > 0) {
        total += (size_t)got;
    }
    close(ends[0]);
    waitpid(child, 0, 0);
    printf("read %zu: %s\n", total, buffer);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **A pipeline**, which is the shape a shell makes: two children, one pipe
/// between them, and standard output and input `dup2`ed onto its ends.
///
/// Every piece of the fd work at once — a fork that shares the ring, a
/// `dup2` that moves a count, a `close` in each child of the end it does not
/// use, and the writer's exit being what ends the reader's loop. Get any of
/// them wrong and this hangs.
#[test]
fn two_children_form_a_pipeline() {
    agrees_with_native(
        "pipeline",
        r#"
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    int middle[2];
    int out[2];
    pipe(middle);
    pipe(out);
    fflush(stdout);

    if (fork() == 0) {
        /* The producer: its standard output is the pipe. */
        dup2(middle[1], 1);
        close(middle[0]);
        close(middle[1]);
        close(out[0]);
        close(out[1]);
        for (int index = 0; index < 3; index++) {
            printf("row %d\n", index);
        }
        fflush(stdout);
        _exit(0);
    }
    if (fork() == 0) {
        /* The consumer: its input is that pipe and its output is the next. */
        dup2(middle[0], 0);
        dup2(out[1], 1);
        close(middle[0]);
        close(middle[1]);
        close(out[0]);
        close(out[1]);
        int rows = 0;
        int character;
        while ((character = getchar()) != EOF) {
            rows += character == '\n';
        }
        printf("counted %d\n", rows);
        fflush(stdout);
        _exit(0);
    }
    close(middle[0]);
    close(middle[1]);
    close(out[1]);
    char buffer[64] = {0};
    size_t total = 0;
    ssize_t got;
    while ((got = read(out[0], buffer + total, sizeof buffer - 1 - total)) > 0) {
        total += (size_t)got;
    }
    close(out[0]);
    while (wait(0) > 0) {
    }
    printf("pipeline said: %s", buffer);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **Process identity**, now that there is more than one process.
///
/// `getpid` was a constant for as long as a container had one process — the
/// entry process is the first in its own namespace, so the answer was fixed.
/// `fork` is what made that false, and the failure is quiet: a child that
/// answers 1 collides with its parent in every temporary filename and every
/// lock a program keys by process, and cannot be signalled by the parent
/// that just created it.
#[test]
fn every_process_knows_which_one_it_is() {
    agrees_with_native(
        "identity",
        r#"
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    pid_t mine = getpid();
    fflush(stdout);
    int ends[2];
    pipe(ends);
    pid_t child = fork();
    if (child == 0) {
        close(ends[0]);
        pid_t reported[2] = {getpid(), getppid()};
        write(ends[1], reported, sizeof reported);
        close(ends[1]);
        _exit(0);
    }
    close(ends[1]);
    pid_t reported[2] = {0, 0};
    read(ends[0], reported, sizeof reported);
    close(ends[0]);
    waitpid(child, 0, 0);
    printf("child is itself %d, its parent is me %d, and not me %d\n",
           reported[0] == child, reported[1] == mine, reported[0] != mine);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **`kill` reaching another process**, which is the other half of having
/// identities at all.
///
/// The disposition is consulted in the *target's* kernel, because that is
/// where it lives: what a signal means is a property of the process
/// receiving it. So this fails if the signal was decided by the sender, if
/// the target could not be found, or if a signal to a process that has
/// already exited is an error rather than a no-op.
#[test]
fn a_signal_reaches_another_process() {
    agrees_with_native(
        "kill",
        r#"
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    int ready[2];
    pipe(ready);
    fflush(stdout);
    pid_t child = fork();
    if (child == 0) {
        close(ready[0]);
        /* Nothing catches it, so the default action ends the child. */
        close(ready[1]);
        for (;;) {
            pause();
        }
    }
    close(ready[1]);
    /* The child closes its end and then waits forever; the read returning
       zero is how this knows the child has got that far. */
    char byte;
    read(ready[0], &byte, 1);
    close(ready[0]);

    printf("alive %d\n", kill(child, 0) == 0);
    printf("sent %d\n", kill(child, SIGTERM) == 0);
    int status = 0;
    waitpid(child, &status, 0);
    printf("signalled %d by %d\n", WIFSIGNALED(status), WTERMSIG(status));
    /* Reaped, so gone. The call and the errno are separate statements
       because reading `errno` in the same `printf` as the call that sets it
       is unsequenced, and the two sides would be comparing evaluation
       orders rather than kernels. */
    errno = 0;
    int missing = kill(child, 0);
    printf("gone %d %d\n", missing, errno == ESRCH);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **The `epoll` dup footgun, asserted rather than avoided.**
///
/// Interest is registered on the open file *description*, not on the
/// descriptor — which is Linux's rule and the reason a registered
/// descriptor closed while a `dup` of it survives goes on firing. It is
/// famous as a bug source, and real software depends on it. So it is built,
/// and the test
/// says so both ways: the registration survives the close that leaves a
/// dup, and it is gone after the close that leaves none.
///
/// The second half is not decoration. A description's slot is reused, so a
/// registration outliving its file would start reporting whichever file
/// opened next, under the old caller's data word.
#[test]
fn epoll_interest_follows_the_description_not_the_descriptor() {
    agrees_with_native(
        "epoll-dup",
        r#"
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <unistd.h>

int main(void) {
    int ends[2];
    pipe(ends);
    int set = epoll_create1(0);

    struct epoll_event watch = { .events = EPOLLIN, .data = { .u64 = 0xfeed } };
    epoll_ctl(set, EPOLL_CTL_ADD, ends[0], &watch);
    int copy = dup(ends[0]);
    write(ends[1], "x", 1);

    struct epoll_event fired[4];
    printf("registered %d\n", epoll_wait(set, fired, 4, 0));

    /* The registered descriptor goes, and the dup keeps the description —
       so the registration is still there and still fires. */
    close(ends[0]);
    printf("after close %d data %llx\n",
           epoll_wait(set, fired, 4, 0), (unsigned long long)fired[0].data.u64);

    /* And now the last one goes, which is what actually ends it. */
    close(copy);
    printf("after both %d\n", epoll_wait(set, fired, 4, 0));

    /* A file opened now takes the freed slot. Nothing may report it. */
    /* Relative, so it is the same file on both sides: the tree is the
       guest's root and the guest starts in it. */
    int reused = open("./init", 0);
    printf("reused %d, still quiet %d\n", reused >= 0, epoll_wait(set, fired, 4, 0));
    close(reused);
    close(set);
    close(ends[1]);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **A `socketpair` across a fork**, which is the simplest connection there
/// is and the one gunicorn's master uses to tell its worker to shut down.
///
/// Two rings crossed: what one endpoint writes the other reads, in both
/// directions at once. So this fails if the pair is not bidirectional, if
/// the fork copied the rings instead of sharing them, or if the reference
/// census does not count what a socket holds — a socket descriptor is a
/// reference on the socket *and* one on each ring it can still move bytes
/// through, and a child that gets the socket but not the rings leaves the
/// parent reading an end-of-file that has not happened.
#[test]
fn a_socketpair_carries_both_ways_across_a_fork() {
    agrees_with_native(
        "socketpair",
        r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    int ends[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, ends) != 0) {
        perror("socketpair");
        return 1;
    }
    fflush(stdout);
    pid_t child = fork();
    if (child == 0) {
        close(ends[0]);
        char question[64] = {0};
        ssize_t got = read(ends[1], question, sizeof question - 1);
        /* And answer back down the same pair, which a pipe could not do. */
        char answer[96];
        int length = snprintf(answer, sizeof answer, "heard %zd bytes: %s", got, question);
        write(ends[1], answer, length);
        close(ends[1]);
        _exit(0);
    }
    close(ends[1]);
    write(ends[0], "quit", 4);
    char reply[128] = {0};
    size_t total = 0;
    ssize_t piece;
    while ((piece = read(ends[0], reply + total, sizeof reply - 1 - total)) > 0) {
        total += (size_t)piece;
    }
    close(ends[0]);
    waitpid(child, 0, 0);
    printf("%s\n", reply);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **The half-close matrix, on a socketpair.**
///
/// `shutdown` is what `close` is only the both-directions case of, and each
/// direction is a reference on a ring — so giving one up is exactly what the
/// last writer of a pipe closing already does. `SHUT_WR` makes the peer
/// drain and then read zero; a write to a peer that has `SHUT_RD` is
/// `EPIPE`; and the endpoint that shut a direction sees its own side of that
/// too. Each cell is a different rule getting the same answer, which is the
/// claim the ring split was made for.
#[test]
fn shutting_one_direction_leaves_the_other() {
    agrees_with_native(
        "half-close",
        r#"
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void) {
    signal(SIGPIPE, SIG_IGN);
    int ends[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, ends);

    /* Bytes already sent survive the shutdown that follows them. */
    write(ends[0], "queued", 6);
    shutdown(ends[0], SHUT_WR);

    char buffer[32] = {0};
    printf("drained %zd: %s\n", read(ends[1], buffer, sizeof buffer - 1), buffer);
    printf("then eof %zd\n", read(ends[1], buffer, sizeof buffer - 1));

    /* The direction the other way is untouched. */
    write(ends[1], "still open", 10);
    memset(buffer, 0, sizeof buffer);
    printf("other way %zd: %s\n", read(ends[0], buffer, sizeof buffer - 1), buffer);

    /* Writing into a direction this endpoint gave up. */
    errno = 0;
    ssize_t refused = write(ends[0], "no", 2);
    printf("own shut %zd errno %d\n", refused, errno);

    /* And a write to a peer that has stopped reading. */
    shutdown(ends[0], SHUT_RD);
    errno = 0;
    ssize_t broken = write(ends[1], "nobody home", 11);
    printf("peer shut %zd errno %d\n", broken, errno);

    close(ends[0]);
    close(ends[1]);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **A loopback server and client in one process tree**, which is the shape
/// the demo stack is made of: nginx binds a port, gunicorn connects to it.
///
/// Every piece of loopback networking at once — `socket`, `bind`, `listen`, `connect`,
/// `accept4`, `getsockname`, and a parked `accept` woken by a connection
/// that arrives from another *process*. No host is involved: both ends are
/// in the guest, so a connection is two rings in an arena the process tree
/// shares, and `connect` is a queue push.
#[test]
fn a_loopback_server_answers_a_forked_client() {
    agrees_with_native(
        "loopback",
        r#"
#include <arpa/inet.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    int listener = socket(AF_INET, SOCK_STREAM, 0);
    if (listener < 0) { perror("socket"); return 1; }
    struct sockaddr_in bound = {0};
    bound.sin_family = AF_INET;
    bound.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    /* Port zero: the kernel picks, and getsockname says which. */
    bound.sin_port = 0;
    if (bind(listener, (struct sockaddr *)&bound, sizeof bound) != 0) {
        perror("bind"); return 1;
    }
    if (listen(listener, 8) != 0) { perror("listen"); return 1; }

    struct sockaddr_in mine = {0};
    socklen_t length = sizeof mine;
    getsockname(listener, (struct sockaddr *)&mine, &length);
    printf("bound to loopback %d, a real port %d\n",
           mine.sin_addr.s_addr == htonl(INADDR_LOOPBACK), ntohs(mine.sin_port) != 0);

    fflush(stdout);
    pid_t child = fork();
    if (child == 0) {
        int client = socket(AF_INET, SOCK_STREAM, 0);
        if (connect(client, (struct sockaddr *)&mine, sizeof mine) != 0) {
            perror("connect"); _exit(1);
        }
        write(client, "GET / HTTP/1.0", 14);
        shutdown(client, SHUT_WR);
        char reply[64] = {0};
        size_t total = 0;
        ssize_t piece;
        while ((piece = read(client, reply + total, sizeof reply - 1 - total)) > 0) {
            total += (size_t)piece;
        }
        printf("client read: %s\n", reply);
        fflush(stdout);
        close(client);
        _exit(0);
    }

    /* The parent parks here until the child's connect queues one. */
    struct sockaddr_in peer = {0};
    socklen_t peerlen = sizeof peer;
    int served = accept(listener, (struct sockaddr *)&peer, &peerlen);
    if (served < 0) { perror("accept"); return 1; }
    char request[64] = {0};
    size_t total = 0;
    ssize_t piece;
    while ((piece = read(served, request + total, sizeof request - 1 - total)) > 0) {
        total += (size_t)piece;
    }
    printf("server read %zu: %s\n", total, request);
    write(served, "HTTP/1.0 200 OK", 15);
    close(served);
    close(listener);
    waitpid(child, 0, 0);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **Connecting to a port nothing is listening on** is `ECONNREFUSED`, and
/// an `AF_UNIX` path that is not there is `ENOENT`.
///
/// The distinction is the one glibc's NSS depends on: it probes
/// `/var/run/nscd/socket` before it will read `/etc/passwd`, four times in
/// the traced baseline, and the `ENOENT` is what sends it to the file it was
/// going to read anyway.
#[test]
fn connecting_to_nothing_says_which_nothing() {
    agrees_with_native(
        "refused",
        r#"
#include <errno.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

int main(void) {
    int client = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in nowhere = {0};
    nowhere.sin_family = AF_INET;
    nowhere.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    nowhere.sin_port = htons(9);
    errno = 0;
    int refused = connect(client, (struct sockaddr *)&nowhere, sizeof nowhere);
    printf("inet %d errno %d\n", refused, errno == ECONNREFUSED);
    close(client);

    int unix_client = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un absent = {0};
    absent.sun_family = AF_UNIX;
    strcpy(absent.sun_path, "/var/run/nscd/socket");
    errno = 0;
    int missing = connect(unix_client, (struct sockaddr *)&absent, sizeof absent);
    printf("unix %d errno %d\n", missing, errno == ENOENT);
    close(unix_client);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **Connecting off `lo` says there is no network**, not that something
/// refused.
///
/// This container is a namespace with only loopback in it — the guest is
/// told of no interface at all — so `127.0.0.0/8` is the whole of the world
/// it can route to and everything else is `ENETUNREACH`. The distinction is
/// the one a client acts on: "refused" means the service is down, so retry;
/// "unreachable" means there is no network, so stop. Answering
/// `ECONNREFUSED` for an address with no route tells a program to keep
/// trying something that cannot ever work.
///
/// And `-p` does not change this. It publishes a guest port *inward* — a
/// listener the host can reach — which is not a route the guest can take
/// out. Egress is not built; this is what not having it should look like
/// from inside, rather than a hang or a lie.
///
/// Checked directly rather than against a native run, for the same reason
/// the orphan test is: the machine running these tests has a route to the
/// internet and this container does not, so the host is not the oracle. A
/// native run of this program would sit in a TEST-NET connect until it
/// timed out.
#[test]
fn there_is_no_route_off_loopback() {
    let (_tree, baked) = image_of(
        "unreachable",
        r#"
#include <errno.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <stdio.h>
#include <sys/socket.h>
#include <unistd.h>

static int reach(const char *dotted, int port) {
    int client = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in where = {0};
    where.sin_family = AF_INET;
    where.sin_addr.s_addr = inet_addr(dotted);
    where.sin_port = htons(port);
    errno = 0;
    int outcome = connect(client, (struct sockaddr *)&where, sizeof where);
    int kept = errno;
    close(client);
    return outcome == 0 ? 0 : kept;
}

int main(void) {
    /* TEST-NET-3, and a private address a container would really have. */
    printf("far %d\n", reach("203.0.113.1", 80) == ENETUNREACH);
    printf("private %d\n", reach("172.17.0.2", 80) == ENETUNREACH);
    /* Loopback still has a route, and nothing is listening on it: the
       answer there is refused, which is the fact this must not flatten. */
    printf("loopback %d\n", reach("127.0.0.1", 9) == ECONNREFUSED);
    /* All of 127/8 is loopback, not just the one address. */
    printf("wider %d\n", reach("127.0.0.53", 53) == ECONNREFUSED);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
    assert_eq!(
        interpreted_output("unreachable", baked),
        "far 1\nprivate 1\nloopback 1\nwider 1\n"
    );
}

/// **`send`, `recv` and the options**, which is the rest of what the traced
/// stack asks a socket for.
///
/// `MSG_PEEK` reads without consuming — a separate operation and not a flag
/// threaded through the taking loop, because that is how one of the two ends
/// up consuming when it should not. `MSG_NOSIGNAL` suppresses the signal and
/// never the errno, which is what every server that writes to a socket it
/// might outlive depends on. And `SO_ERROR` is read once and cleared, which
/// nginx does after every connect whether or not one was needed.
#[test]
fn the_socket_options_and_message_flags_answer() {
    agrees_with_native(
        "sockopt",
        r#"
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void) {
    int ends[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, ends);

    /* What a socket says about itself. */
    int kind = 0, held = sizeof kind;
    getsockopt(ends[0], SOL_SOCKET, SO_TYPE, &kind, (socklen_t *)&held);
    printf("type stream %d, listening %d\n", kind == SOCK_STREAM, ({
        int accepting = 1; socklen_t n = sizeof accepting;
        getsockopt(ends[0], SOL_SOCKET, SO_ACCEPTCONN, &accepting, &n);
        accepting; }));

    /* Recorded and read back. */
    int on = 1;
    setsockopt(ends[0], SOL_SOCKET, SO_KEEPALIVE, &on, sizeof on);
    int back = 0; socklen_t backlen = sizeof back;
    getsockopt(ends[0], SOL_SOCKET, SO_KEEPALIVE, &back, &backlen);
    printf("keepalive %d\n", back != 0);

    /* No error has happened, and reading clears it. */
    int problem = -1; socklen_t problemlen = sizeof problem;
    getsockopt(ends[0], SOL_SOCKET, SO_ERROR, &problem, &problemlen);
    printf("so_error %d\n", problem);

    /* Peek, then read the same bytes again. */
    send(ends[0], "peek at me", 10, 0);
    char first[32] = {0}, second[32] = {0};
    ssize_t peeked = recv(ends[1], first, sizeof first - 1, MSG_PEEK);
    ssize_t taken = recv(ends[1], second, sizeof second - 1, 0);
    printf("peeked %zd '%s', took %zd '%s'\n", peeked, first, taken, second);
    printf("and then nothing %zd\n", recv(ends[1], first, sizeof first - 1, MSG_DONTWAIT));

    /* A write with nobody reading, and no signal about it. */
    signal(SIGPIPE, SIG_IGN);
    close(ends[1]);
    errno = 0;
    ssize_t refused = send(ends[0], "gone", 4, MSG_NOSIGNAL);
    printf("nosignal %zd errno %d\n", refused, errno == EPIPE);
    close(ends[0]);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **`select` waking on a connection**, which is how gunicorn's worker
/// learns it has one.
///
/// The traced stack waits in `pselect6` on its listener, so a `select` that
/// only ever returns when its *timeout* expires is a server that answers
/// every request fifteen seconds late — which looks like slowness and is
/// actually a readiness path that never fires.
#[test]
fn select_wakes_on_a_connection() {
    agrees_with_native(
        "select",
        r#"
#include <arpa/inet.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    int listener = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in bound = {0};
    bound.sin_family = AF_INET;
    bound.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    bind(listener, (struct sockaddr *)&bound, sizeof bound);
    listen(listener, 8);
    socklen_t length = sizeof bound;
    getsockname(listener, (struct sockaddr *)&bound, &length);

    fflush(stdout);
    if (fork() == 0) {
        int client = socket(AF_INET, SOCK_STREAM, 0);
        connect(client, (struct sockaddr *)&bound, sizeof bound);
        write(client, "hello", 5);
        close(client);
        _exit(0);
    }

    /* A generous timeout that must NOT be what ends the wait: if the
       connection is what wakes it, this returns almost at once, and the
       test would still pass on a slow machine — what it cannot survive is
       readiness never firing, because then `ready` is zero. */
    fd_set readable;
    FD_ZERO(&readable);
    FD_SET(listener, &readable);
    struct timeval patience = { .tv_sec = 30, .tv_usec = 0 };
    int ready = select(listener + 1, &readable, 0, 0, &patience);
    printf("select said %d, listener in set %d\n", ready, FD_ISSET(listener, &readable) != 0);

    int served = accept(listener, 0, 0);
    char buffer[16] = {0};
    printf("read %zd: %s\n", read(served, buffer, sizeof buffer - 1), buffer);
    close(served);
    close(listener);
    wait(0);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **`poll(POLLIN)` on a half-closed socket whose peer has gone**, with the
/// peer's next connection landing in the slot the old rings left.
///
/// What gunicorn does after every response: `shutdown(SHUT_WR)`, then a
/// `recv` under a two-second socket timeout, waiting for nginx to close its
/// side. Two things have to be true for that to return at once. End of file
/// has to count as readable — Linux reports a `FIN` as `POLLIN | POLLRDHUP`,
/// and a caller that asked only for `POLLIN` must not sleep through it. And
/// the server's own transmit ring, which has no ends left once it gave up
/// its writer and the client closed its reader, must not be handed to the
/// client's *next* `connect` while the server still names it — or the
/// server reads that new connection's reader as its old peer come back,
/// loses `POLLHUP`, and waits out the whole timeout. Four clients at once
/// measured two seconds a request from exactly this, and one at a time
/// measured nothing, because one at a time nobody reconnects in the gap.
#[test]
fn poll_sees_end_of_file_when_the_peer_has_reconnected() {
    agrees_with_native(
        "poll-eof-reconnect",
        r#"
#include <arpa/inet.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void) {
    int listener = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in bound = {0};
    bound.sin_family = AF_INET;
    bound.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    bind(listener, (struct sockaddr *)&bound, sizeof bound);
    listen(listener, 8);
    socklen_t length = sizeof bound;
    getsockname(listener, (struct sockaddr *)&bound, &length);

    /* One request and its answer, the way a proxy and a worker do it. */
    int client = socket(AF_INET, SOCK_STREAM, 0);
    connect(client, (struct sockaddr *)&bound, sizeof bound);
    int served = accept(listener, 0, 0);
    char buffer[16] = {0};
    send(client, "hi", 2, 0);
    recv(served, buffer, sizeof buffer, 0);
    send(served, "ok", 2, 0);
    /* The worker is done sending and waits for the proxy to hang up. */
    shutdown(served, SHUT_WR);
    recv(client, buffer, sizeof buffer, 0);
    ssize_t eof = recv(client, buffer, sizeof buffer, 0);
    close(client);

    /* The proxy's next request arrives before the worker has looked. */
    int next = socket(AF_INET, SOCK_STREAM, 0);
    connect(next, (struct sockaddr *)&bound, sizeof bound);

    /* Asked without waiting, on purpose. gunicorn waits two seconds here,
       and what went wrong was the *answer*, which is computed the same way
       whether or not the caller waits — so a regression should fail this
       at once rather than wait on a timeout, which under this harness's
       fixed clock never arrives. Both
       defects are visible separately: `in` is end of file being readable,
       and `hup` is the server still seeing its own transmit ring rather
       than the one the reconnect was handed. */
    struct pollfd watch = { .fd = served, .events = POLLIN };
    int ready = poll(&watch, 1, 0);
    ssize_t after = recv(served, buffer, sizeof buffer, 0);
    printf("client eof %zd, poll %d in=%d hup=%d, server eof %zd\n",
           eof, ready, (watch.revents & POLLIN) != 0,
           (watch.revents & POLLHUP) != 0, after);

    close(served);
    close(next);
    close(listener);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **`epoll` on a connected socket**, which is how nginx reads a response
/// from an upstream it proxied to.
///
/// The listener case is the one every socket test reaches for, and it is not
/// the one a proxy spends its life in: nginx connects to its upstream,
/// registers *that* descriptor, and waits for the response to arrive. If a
/// connected socket's readiness never reaches `epoll`, the proxy waits for
/// its `proxy_read_timeout` and answers 504 — which is what the demo stack
/// did, sixty seconds at a time, while the upstream had already replied.
#[test]
fn epoll_wakes_on_a_reply_from_an_upstream() {
    agrees_with_native(
        "epoll-upstream",
        r#"
#include <arpa/inet.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    int listener = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in bound = {0};
    bound.sin_family = AF_INET;
    bound.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    bind(listener, (struct sockaddr *)&bound, sizeof bound);
    listen(listener, 8);
    socklen_t length = sizeof bound;
    getsockname(listener, (struct sockaddr *)&bound, &length);

    fflush(stdout);
    if (fork() == 0) {
        /* The upstream: accept, read the request, answer it. */
        int served = accept(listener, 0, 0);
        char request[64] = {0};
        read(served, request, sizeof request - 1);
        write(served, "HTTP/1.0 200 OK", 15);
        close(served);
        close(listener);
        _exit(0);
    }
    close(listener);

    int upstream = socket(AF_INET, SOCK_STREAM, 0);
    connect(upstream, (struct sockaddr *)&bound, sizeof bound);
    write(upstream, "GET / HTTP/1.0", 14);

    int set = epoll_create1(0);
    struct epoll_event watch = { .events = EPOLLIN, .data = { .fd = upstream } };
    epoll_ctl(set, EPOLL_CTL_ADD, upstream, &watch);
    struct epoll_event fired[4];
    /* Generous, and it must not be what ends the wait. */
    int ready = epoll_wait(set, fired, 4, 30000);
    printf("epoll said %d, on the upstream %d\n", ready, ready > 0 && fired[0].data.fd == upstream);

    char reply[64] = {0};
    printf("read %zd: %s\n", read(upstream, reply, sizeof reply - 1), reply);
    close(upstream);
    close(set);
    wait(0);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
}

/// **A shutdown request becomes the container's own `SIGTERM`**, at its
/// first process.
///
/// How a server ends. The host has no way to push anything at a container —
/// the boundary answers questions — so a stop is a path the guest reads at
/// the points it is already asking the host things, and what it becomes is
/// a signal at pid 1. That is what makes an init script's `trap` run and
/// its children get told, rather than the host reaching past the
/// container to kill processes it did not start.
///
/// Checked directly rather than against a native run: nothing sends this
/// program a signal when it runs on the host, so the host is not the oracle.
#[test]
fn a_shutdown_request_becomes_a_signal_at_the_first_process() {
    let (_tree, baked) = image_of(
        "shutdown",
        r#"
#include <signal.h>
#include <stdio.h>
#include <unistd.h>

static volatile sig_atomic_t asked = 0;

static void stopping(int signal) {
    asked = signal;
}

int main(void) {
    struct sigaction action = {0};
    action.sa_handler = stopping;
    sigaction(SIGTERM, &action, 0);
    printf("running\n");
    fflush(stdout);
    /* Waits for a signal and nothing else, which is what an init that has
       started its children does. */
    while (!asked) {
        pause();
    }
    printf("asked to stop by %d\n", asked);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
    let image = Image::parse(&baked.index, &baked.blob).expect("parse the image");
    let mut system = boot("shutdown", image);
    system.current().kernel.store.borrow_mut().stopping = true;
    let exit = system.run();
    report("shutdown", &exit, &mut system);
    assert_eq!(exit, Exit::Status(0), "the container did not exit cleanly");
    let out = String::from_utf8(
        system
            .current()
            .kernel
            .store
            .borrow()
            .contents(kernel::paths::CONSOLE_STDOUT),
    )
    .expect("utf-8");
    assert_eq!(out, "running\nasked to stop by 15\n");
}

/// **The vDSO, and the only way to tell it is working.**
///
/// A vDSO glibc rejects is a vDSO glibc *silently* ignores — it falls back
/// to the syscall, which answers correctly, and everything passes. So the
/// assertion cannot be that the clock works. It has to be that **no clock
/// syscall was issued at all**, which is what a native `strace` of the same
/// program shows and what `demo/hello-django/baseline/n5-diff.txt` named as
/// the one structural divergence left between the two.
///
/// Two thousand reads, so a single stray syscall is visible against zero,
/// and a spread over the three calls glibc routes through it.
#[test]
fn the_clock_is_read_without_a_syscall() {
    let (_tree, baked) = image_of(
        "vdso",
        r#"
#include <stdio.h>
#include <sys/time.h>
#include <time.h>

int main(void) {
    struct timespec first, last;
    clock_gettime(CLOCK_MONOTONIC, &first);
    for (int index = 0; index < 2000; index++) {
        struct timespec now;
        clock_gettime(CLOCK_MONOTONIC, &now);
    }
    clock_gettime(CLOCK_MONOTONIC, &last);
    struct timeval wall;
    gettimeofday(&wall, 0);

    long long elapsed = (last.tv_sec - first.tv_sec) * 1000000000LL
        + (last.tv_nsec - first.tv_nsec);
    /* Monotonic, and it *moved*: a clock frozen between kernel samples
       would read the same value twice and report zero, which is the failure
       the interpolation exists to avoid. */
    printf("forward %d\n", elapsed > 0);
    printf("wall is a real time %d\n", wall.tv_sec > 1600000000);
    struct timespec grain;
    printf("resolution %d\n", clock_getres(CLOCK_MONOTONIC, &grain) == 0);
    return 0;
}
"#,
        Linkage::Dynamic,
    );
    let image = Image::parse(&baked.index, &baked.blob).expect("parse the image");
    let mut system = boot("vdso", image);
    system.current().kernel.store.borrow_mut().tracing = true;
    let exit = system.run();
    report("vdso", &exit, &mut system);
    assert_eq!(exit, Exit::Status(0), "the container did not exit cleanly");

    let store = system.current().kernel.store.borrow();
    let out = String::from_utf8(store.contents(kernel::paths::CONSOLE_STDOUT)).expect("utf-8");
    assert_eq!(out, "forward 1\nwall is a real time 1\nresolution 1\n");

    let trace = String::from_utf8_lossy(&store.contents(kernel::paths::LOG_DEBUG)).into_owned();
    let clock_calls: Vec<&str> = trace
        .lines()
        .filter(|line| {
            ["clock_gettime(", "gettimeofday(", "time(", "clock_getres("]
                .iter()
                .any(|name| line.contains(name))
        })
        .collect();
    assert!(
        !trace.is_empty(),
        "nothing was traced, so this proves nothing"
    );
    assert_eq!(
        clock_calls.len(),
        0,
        "the clock was read by syscall {} times, so glibc rejected the vDSO \
         and fell back — which answers correctly and is why this test counts \
         syscalls rather than checking the time:\n{}",
        clock_calls.len(),
        clock_calls.join("\n")
    );
}

/// **The container is a store.** The isotope Server Protocol from the
/// kernel's side: a batch of Requests read from `/iso/server/requests/pending`,
/// each answered by a Response written to its `respond_to` path — the
/// processes, a thread's registers, the run's cost — and a path the store
/// does not serve answered as `not_found`.
#[test]
fn the_container_serves_its_own_store() {
    let (_tree, baked) = image_of(
        "served",
        r#"
#include <unistd.h>
int main(void) {
    volatile long sum = 0;
    for (long i = 0; i < 2000000; i++) sum += i;
    write(1, "done\n", 5);
    return 0;
}
"#,
        Linkage::Static,
    );
    let image = Image::parse(&baked.index, &baked.blob).expect("parse the image");
    let mut system = boot("served", image);
    for _ in 0..3 {
        assert_ne!(system.turn(), Turn::Finished(Exit::Status(0)), "finished before it was asked anything");
    }
    system.current().kernel.store.borrow_mut().pending = Some(
        br#"[{"op":"read","path":"statistics","data":null,"respond_to":"/iso/server/responses/1"},
            {"op":"read","path":"processes","data":null,"respond_to":"/iso/server/responses/2"},
            {"op":"read","path":"processes/1/threads/1/registers","data":null,"respond_to":"/iso/server/responses/3"},
            {"op":"read","path":"processes/1/maps","data":null,"respond_to":"/iso/server/responses/4"},
            {"op":"read","path":"processes/1/descriptors","data":null,"respond_to":"/iso/server/responses/5"},
            {"op":"write","path":"processes","data":{"x":1},"respond_to":"/iso/server/responses/6"},
            {"op":"read","path":"nothing","data":null,"respond_to":"/iso/server/responses/7"}]"#
            .to_vec(),
    );
    system.serve();
    let mut response = |id: &str| -> String {
        let path = vec![b"iso".to_vec(), b"server".to_vec(), b"responses".to_vec(), id.as_bytes().to_vec()];
        let store = system.current().kernel.store.clone();
        let store = store.borrow();
        let (_, bytes) = store.written.iter().find(|(at, _)| *at == path).unwrap_or_else(|| panic!("no response {id}"));
        String::from_utf8(bytes.clone()).expect("utf-8")
    };
    let statistics = response("1");
    assert!(statistics.starts_with(r#"{"result":"ok","value":{"retired":"#), "{statistics}");
    assert!(statistics.contains(r#""current":1"#), "{statistics}");
    let processes = response("2");
    assert!(processes.contains(r#""pid":1,"parent":0,"state":"live""#), "{processes}");
    assert!(processes.contains(r#""tid":1,"rip":"0x"#), "{processes}");
    let registers = response("3");
    assert!(registers.contains(r#""rsp":"0x"#) && registers.contains(r#""flags_stale":null"#), "{registers}");
    let maps = response("4");
    assert!(maps.contains("r-xp") || maps.contains("rw-p"), "{maps}");
    let descriptors = response("5");
    assert!(descriptors.contains(r#""fd":1"#), "{descriptors}");
    assert!(response("6").contains(r#""type":"not_writable""#));
    assert!(response("7").contains(r#""type":"not_found""#));
    // And the guest saw none of it: the run finishes as it always does.
    assert_eq!(system.run(), Exit::Status(0));
}
