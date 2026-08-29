//! Booting a *dynamically linked* program: the executable, `ld.so`, and
//! `libc.so.6`, all translated ahead of time and run as one module.
//!
//! This is the tier the container plan calls the general one. The static
//! tier avoids the loader by putting every byte of code in one file; here
//! there is no such file. The program is a position-independent executable
//! that cannot run at all until something maps its libraries and patches its
//! GOT — and the something is glibc's own `ld.so`, running as ordinary
//! translated guest code with no help from the kernel beyond the two things
//! only the kernel can supply: an auxiliary vector saying where each file
//! was placed, and an `mmap` that answers a request to map a translated ELF
//! with the address its code was translated at.
//!
//! Three properties underlie every case below, and the cases are written so
//! that failing any of them fails loudly rather than quietly:
//!
//! - **The prelink round-trips.** The bake picks a base per file and
//!   translates at it; the loader asks for "anywhere" and must get the same
//!   number back, or every relocation it computes names a byte no exec-map
//!   entry describes.
//! - **The exec map spans modules.** Every cross-library call is an indirect
//!   transfer through an address the loader wrote, so the map has to hold
//!   entries for all three files at once. That is why they are one
//!   translation unit rather than three.
//! - **`ld.so` itself translates and runs.** Self-relocation, symbol
//!   lookup, `DT_NEEDED` walking, TLS setup, and the jump to the program's
//!   entry are all guest code here.

mod support;

use std::sync::OnceLock;

use support::WorkingDirectory;

fn path(segments: &[&[u8]]) -> Vec<Vec<u8>> {
    segments.iter().map(|segment| segment.to_vec()).collect()
}

/// One program, one container, several assertions.
///
/// Every case below needs the same thing built: a dynamic executable, its
/// loader and its libc, all translated, linked into one module and compiled
/// by the engine — fifteen thousand function bodies of which a hello touches
/// a few hundred. Paying for that once per assertion is how a suite becomes
/// something nobody runs. So the cases share a container, and each still
/// gets its own `#[test]` and its own name, because a failure that says
/// which behaviour broke is worth keeping.
///
/// The program prints one labelled line per case, so an assertion reads its
/// own line and says so.
static OUTPUT: OnceLock<String> = OnceLock::new();

fn output() -> &'static str {
    OUTPUT.get_or_init(|| {
        let workspace = WorkingDirectory::new("dynamic-tier");
        run_dynamic(&workspace, "dynamic-tier", EXERCISE)
    })
}

fn line(label: &str) -> &'static str {
    output()
        .lines()
        .find_map(|line| line.strip_prefix(label))
        .unwrap_or_else(|| {
            panic!("no `{label}` line in the program's output:\n{}", output())
        })
}

/// What every case below reads a line of.
///
/// Deliberately one program rather than several: what is being tested is a
/// tier, and a tier's failures are not near the entry point — they are
/// wherever the first unrecovered dispatch or unmapped library happens to
/// be. Breadth in one binary costs one bake.
const EXERCISE: &str = r##"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>

static int by_value(const void *a, const void *b) {
    return *(const int *)a - *(const int *)b;
}

int main(void) {
    puts("puts: hello, dynamic");

    fprintf(stdout, "glob_dat: %d %s\n", 40 + 2, "through the plt");

    int *v = malloc(8 * sizeof(int));
    for (int i = 0; i < 8; i++) v[i] = (i * 37) % 11;
    qsort(v, 8, sizeof(int), by_value);
    printf("qsort: ");
    for (int i = 0; i < 8; i++) printf("%d", v[i]);
    putchar('\n');
    free(v);

    char buf[64];
    snprintf(buf, sizeof buf, "%s/%d/%.3f", "path", 42, sqrt(2.0));
    printf("format: %s len=%zu\n", buf, strlen(buf));

    FILE *f = fopen("/tmp/probe.txt", "w");
    if (!f) { perror("fopen"); return 1; }
    fprintf(f, "written\n");
    fclose(f);
    f = fopen("/tmp/probe.txt", "r");
    if (!f) { perror("reopen"); return 1; }
    char back[32] = {0};
    if (!fgets(back, sizeof back, f)) { perror("fgets"); return 1; }
    fclose(f);
    printf("file: %s", back);
    return 0;
}
"##;

/// The whole tier, in one call: a dynamic hello through `libc`'s `puts`.
///
/// `puts` is reached through the PLT, so the call that prints is itself a
/// cross-module indirect transfer through a GOT slot `ld.so` filled in — the
/// mechanism under test, not merely a use of it.
#[test]
fn a_dynamic_glibc_hello_runs() {
    assert_eq!(line("puts: "), "hello, dynamic");
}

/// The loader's own work, made visible: a value that only exists because
/// relocation happened.
///
/// `stdout` is a pointer in `libc`'s data that the program reads through a
/// `GLOB_DAT` relocation, and `fprintf` reaches `libc` through the PLT. A
/// container that mapped the library at the wrong base, or filled the GOT
/// with addresses nothing was translated at, cannot produce this line — it
/// dies at the first call rather than printing something slightly wrong.
#[test]
fn a_dynamic_program_reaches_libc_data_through_its_relocations() {
    assert_eq!(line("glob_dat: "), "42 through the plt");
}

/// A pointer from the executable, called back by the library.
///
/// The comparison function is `main`'s, handed to `qsort` in `libc` and
/// called through the exec map across the module boundary. Nothing about
/// that works unless both files' functions are in one map at the addresses
/// the loader believes.
#[test]
fn a_callback_crosses_from_the_program_into_the_library() {
    assert_eq!(line("qsort: "), "01245689");
}

/// Formatted output, which is where the dispatch tables live.
///
/// `printf`'s conversion loop is a computed goto over six tables that all
/// measure from one code label, with `gcc -O2` having merged their
/// dispatches — the shape that made an arm space a property of the origin
/// rather than of a table. A wrong arm here prints something plausible, so
/// the value is checked rather than the absence of a crash.
#[test]
fn formatted_output_dispatches_to_the_right_arms() {
    assert_eq!(line("format: "), "path/42/1.414 len=13");
}

/// A file written and read back, through the overlay.
#[test]
fn a_dynamic_program_reaches_the_filesystem() {
    assert_eq!(line("file: "), "written");
}

/// A binary this repository did not compile.
///
/// Everything else here is built by the test out of source it controls, at
/// flags it chose. This one is whatever the distribution shipped: stripped,
/// so `.dynsym` and `.eh_frame` are the only witnesses there are; linked by
/// someone else's toolchain; and carrying whatever startup the packager's
/// `crt` files bring. It is the difference between "the tier works on our
/// programs" and "the tier works".
///
/// `echo` with no arguments prints one newline and exits zero, which is
/// stable across every coreutils there has been. Skipped rather than failed
/// where the system's `echo` is not a dynamic executable — a distribution
/// that ships a static one is not this test's failure.
///
/// This one earns its own bake, because a different input is the point.
#[test]
fn a_binary_we_did_not_build_runs() {
    let Some(program) = distribution_binary("/bin/echo") else {
        eprintln!("skipped: /bin/echo is not a dynamic executable here");
        return;
    };
    let workspace = WorkingDirectory::new("distribution-echo");
    assert_eq!(bake_and_run(&workspace, &program, "distribution-echo"), "\n");
}

/// The path, if it names a dynamic executable this tier can take.
fn distribution_binary(path: &str) -> Option<std::path::PathBuf> {
    let bytes = std::fs::read(path).ok()?;
    // `ET_DYN` at the two bytes ELF puts the type in.
    (bytes.len() > 18 && bytes.starts_with(b"\x7fELF") && bytes[16] == 3)
        .then(|| std::path::PathBuf::from(path))
}

/// Builds a dynamic program the way a distribution builds one, bakes it with
/// everything it loads, and runs it.
fn run_dynamic(workspace: &WorkingDirectory, name: &str, program: &str) -> String {
    let source = workspace.write("program.c", program);
    let elf = workspace.path().join("program.elf");
    // No `-static`, and nothing else either: the default on every
    // distribution this decade is a dynamic position-independent
    // executable, which is the input this tier exists for.
    support::run_tool(
        "gcc",
        &[
            "-O2",
            &source.to_string_lossy(),
            "-o",
            &elf.to_string_lossy(),
            "-lm",
        ],
    );
    bake_and_run(workspace, &elf, name)
}

/// Bakes a linked program with everything it loads, and runs the result.
fn bake_and_run(workspace: &WorkingDirectory, elf: &std::path::Path, name: &str) -> String {
    // A directory to write into. The overlay makes it writable; it has to
    // exist first, exactly as it would in any image.
    let mut tree = baker::tree::Tree::new();
    tree.resolve_or_create(b"/tmp").expect("a /tmp in the image");
    let baked = baker::bake::container(elf, std::path::Path::new("/"), tree)
        .expect("bake the program and what it loads");
    assert!(
        baked.placed.len() >= 3,
        "a dynamic program loads at least itself, its loader and a libc, \
         but the bake found {:?}",
        baked.placed
    );

    let worklist: String = baked
        .refused
        .iter()
        .map(|refusal| format!("{}: {}\n", refusal.name, refusal.reason))
        .collect();
    workspace.write("refused.txt", worklist.as_bytes());

    let guest = workspace.write("program.wasm.o", &baked.module);
    let module = support::link_container_for_program(
        workspace,
        std::slice::from_ref(&guest),
        &baked.image,
        name,
        Some(baked.top),
    );

    if std::env::var_os("ZAQARU_KEEP_CONTAINER").is_some() {
        std::fs::copy(&module, format!("/tmp/{name}.wasm")).expect("keep the container");
    }
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
    assert_eq!(status, 0, "the program did not exit cleanly");

    String::from_utf8_lossy(
        &container
            .mounts()
            .read(&path(&[b"iso", b"console", b"stdout"]))
            .expect("console")
            .unwrap_or_default(),
    )
    .into_owned()
}
