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

use kisal::abi::{Store, StoreOutcome};
use kisal::image::Image;
use kisal::machine::Interpreted;
use kisal::run::{Exit, Process};
use kisal::syscall::{Enforcement, Kernel};

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
    /// path the program will hand to `dlopen`.
    path: &'static str,
    source: &'static str,
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
        let file = tree.root.join("plugin.c");
        std::fs::write(&file, plugin.source).expect("write the plugin source");
        let destination = tree.root.join(plugin.path.trim_start_matches('/'));
        std::fs::create_dir_all(destination.parent().expect("a parent")).expect("mkdir");
        let outcome = Command::new("gcc")
            .arg(&file)
            .args(["-shared", "-fPIC"])
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
    }

    let baked: &'static baker::Image =
        Box::leak(Box::new(baker::bake_directory(&tree.root).expect("bake")));
    (tree, baked)
}

fn boot(label: &str, image: Image<'static>) -> Process<'static, Console> {
    boot_with(label, image, &[b"/init"])
}

fn boot_with(
    label: &str,
    image: Image<'static>,
    argv: &[&[u8]],
) -> Process<'static, Console> {
    let kernel = Kernel::with_enforcement(
        Console::default(),
        Interpreted::new(),
        image,
        // The interpreter's world: a page is reachable because something
        // mapped it, and an access to anything else is a fault.
        Enforcement::Mapped,
    );
    match Process::boot(kernel, b"/init", argv, &[]) {
        Ok(process) => process,
        Err(error) => {
            let mut message = String::new();
            error.message(&mut message);
            panic!("{label}: booting failed: {message}");
        }
    }
}

/// What a failure needs to be diagnosable: where the guest was, and what its
/// address space looked like when it got there.
fn report(label: &str, exit: &Exit, process: &mut Process<'static, Console>) {
    if matches!(exit, Exit::Status(_)) {
        return;
    }
    eprintln!("--- {label}: {exit:?}");
    eprintln!("rsp {:#x}", process.kernel.machine.thread.stack_pointer());
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
    let (tree, baked) = image_of(label, source, linkage);
    let native = Command::new(tree.root.join("init"))
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

    let image = Image::parse(&baked.index, &baked.blob).expect("parse the image");
    let mut process = boot(label, image);
    let exit = process.run();
    report(label, &exit, &mut process);
    assert_eq!(exit, Exit::Status(0), "{label} did not exit cleanly");
    let out =
        String::from_utf8(process.kernel.store.contents(kisal::paths::CONSOLE_STDOUT))
            .expect("utf-8");
    assert_eq!(out, expected, "{label}: interpreted output differs");
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
    assert_eq!(exit, Exit::Status(0), "{label} did not exit cleanly");
    let out = String::from_utf8(process.kernel.store.contents(kisal::paths::CONSOLE_STDOUT))
        .expect("utf-8");
    assert_eq!(out, expected);
}
