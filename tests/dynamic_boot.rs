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
//! Three properties are being asserted at once, and the test is written so
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

use support::WorkingDirectory;

fn path(segments: &[&[u8]]) -> Vec<Vec<u8>> {
    segments.iter().map(|segment| segment.to_vec()).collect()
}

/// The whole tier, in one program: a dynamic hello through `libc`'s `puts`.
///
/// `puts` is reached through the PLT, so the call that prints is itself a
/// cross-module indirect transfer through a GOT slot `ld.so` filled in — the
/// mechanism under test, not merely a use of it.
#[test]
fn a_dynamic_glibc_hello_runs() {
    let out = run_dynamic(
        "dynamic-hello",
        "#include <stdio.h>\nint main(void){ puts(\"hello, dynamic\"); return 0; }\n",
    );
    assert_eq!(out, "hello, dynamic\n");
}

/// The loader's own work, made visible: a value that only exists because
/// relocation happened.
///
/// `stdout` is a pointer in `libc`'s data that the program reads through a
/// `GLOB_DAT` relocation, and `fprintf` reaches `libc` through the PLT. A
/// container that mapped the library at the wrong base, or filled the GOT
/// with addresses nothing was translated at, cannot produce this output — it
/// dies at the first call rather than printing something slightly wrong.
#[test]
fn a_dynamic_program_reaches_libc_data_through_its_relocations() {
    let out = run_dynamic(
        "dynamic-fprintf",
        "#include <stdio.h>\n\
         int main(void){ fprintf(stdout, \"%d %s\\n\", 40 + 2, \"through the plt\"); return 0; }\n",
    );
    assert_eq!(out, "42 through the plt\n");
}

/// A program that uses the library rather than merely calling into it.
///
/// `qsort` through a callback is the piece that matters most: the comparison
/// function is a pointer from the *executable* handed to a function in
/// `libc`, called back across the module boundary through the exec map, and
/// nothing about that works unless both files' functions are in one map at
/// the addresses the loader believes.
///
/// The rest is breadth on purpose — an allocator, formatted output through
/// `__printf_chk`'s dispatch tables, a libm call, and a file written and
/// read back through the overlay — because the failures this tier produces
/// are not near the entry point. They are wherever the first unrecovered
/// dispatch or unmapped library happens to be.
#[test]
fn a_dynamic_program_uses_the_library_it_linked_against() {
    let out = run_dynamic(
        "dynamic-exercise",
        r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>

static int by_value(const void *a, const void *b) {
    return *(const int *)a - *(const int *)b;
}

int main(void) {
    int *v = malloc(8 * sizeof(int));
    for (int i = 0; i < 8; i++) v[i] = (i * 37) % 11;
    qsort(v, 8, sizeof(int), by_value);
    for (int i = 0; i < 8; i++) printf("%d", v[i]);
    putchar('\n');
    free(v);

    char buf[64];
    snprintf(buf, sizeof buf, "%s/%d/%.3f", "path", 42, sqrt(2.0));
    printf("%s len=%zu\n", buf, strlen(buf));

    FILE *f = fopen("/tmp/probe.txt", "w");
    if (!f) { perror("fopen"); return 1; }
    fprintf(f, "written\n");
    fclose(f);
    f = fopen("/tmp/probe.txt", "r");
    if (!f) { perror("reopen"); return 1; }
    char back[32] = {0};
    if (!fgets(back, sizeof back, f)) { perror("fgets"); return 1; }
    fclose(f);
    printf("read back: %s", back);
    return 0;
}
"#,
    );
    assert_eq!(out, "01245689
path/42/1.414 len=13
read back: written
");
}

/// Builds a dynamic program the way a distribution builds one, bakes it with
/// everything it loads, and runs it.
fn run_dynamic(name: &str, program: &str) -> String {
    let workspace = WorkingDirectory::new(name);
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

    // The same call the tool makes. The image starts empty: everything in it
    // is put there by the bake, which is the point — the loader will find
    // its libraries at the paths it looks in because the bake placed them
    // at those paths.
    // A directory to write into. The overlay makes it writable; it has to
    // exist first, exactly as it would in any image.
    let mut tree = baker::tree::Tree::new();
    tree.resolve_or_create(b"/tmp").expect("a /tmp in the image");
    let baked = baker::bake::container(&elf, std::path::Path::new("/"), tree)
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
        &workspace,
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
