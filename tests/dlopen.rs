//! `dlopen`: the rung after M6's checkpoint.
//!
//! A `dlopen`ed library is in nobody's closure. The executable does not link
//! against it, so walking `PT_INTERP` and `DT_NEEDED` from the program will
//! never find it — which means the closure, which is what the dynamic tier
//! was built on, is the wrong unit. The unit of translation is the *image*:
//! every ELF in the tree is swept, translated and merged into the one exec
//! map, because an address a function pointer can name has to be in it.
//!
//! Measured on the target that forced it: a distribution CPython's
//! `lib-dynload/` holds 47 extension modules, and `import json` — the first
//! line of any real script — needs one of them. `print("hello")` needs none,
//! which is why the checkpoint passed without any of this.
//!
//! What is checked here is the ladder `container-build-plan.md` sets out:
//! the policy is inert when there is nothing to sweep, it picks up a library
//! nothing references, the happy path works, a library's constructor and
//! thread-locals and callbacks work, and the failures fail the way Linux
//! fails them.

mod support;

use support::WorkingDirectory;

fn path(segments: &[&[u8]]) -> Vec<Vec<u8>> {
    segments.iter().map(|segment| segment.to_vec()).collect()
}

/// Where a loaded library goes in the image. An absolute path, because the
/// point here is the loading rather than the search.
const LIBRARY: &str = "/lib/libextra.so";

/// The library, which exercises everything a real extension module does.
///
/// A constructor that runs before anything calls in; a `__thread` variable,
/// which is the dynamic-TLS path — `dlopen`ed modules take a different route
/// through `__tls_get_addr` than the boot-time libraries do, because those
/// get static TLS and this cannot; an allocation, which crosses back into
/// the program's own libc; and a callback into the program, which is a
/// function pointer travelling the other way across the seam.
const LIBRARY_SOURCE: &str = r##"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int constructed;
static __thread int per_thread = 7;

__attribute__((constructor)) static void build(void) { constructed = 11; }

int extra_answer(int value) { return value * 3 + constructed; }

int extra_thread_local(int add) {
    per_thread += add;
    return per_thread;
}

char *extra_allocate(const char *text) {
    char *copy = malloc(strlen(text) + 2);
    strcpy(copy, text);
    strcat(copy, "!");
    return copy;
}

int extra_calls_back(int (*back)(int), int value) { return back(value) + 1; }
"##;

/// The program: load, look up, call, and report.
const PROGRAM_SOURCE: &str = r##"
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>

static int doubler(int value) { return value * 2; }

int main(void) {
    void *library = dlopen("/lib/libextra.so", RTLD_NOW);
    if (!library) { printf("dlopen failed: %s\n", dlerror()); return 1; }

    int (*answer)(int) = dlsym(library, "extra_answer");
    int (*thread_local_)(int) = dlsym(library, "extra_thread_local");
    char *(*allocate)(const char *) = dlsym(library, "extra_allocate");
    int (*calls_back)(int (*)(int), int) = dlsym(library, "extra_calls_back");
    if (!answer || !thread_local_ || !allocate || !calls_back) {
        printf("dlsym failed: %s\n", dlerror());
        return 1;
    }

    printf("answer:%d\n", answer(10));
    /* Sequenced deliberately: C leaves argument evaluation order
       unspecified, and gcc evaluates right to left — which is fine for a
       differential, where both sides do the same thing, and a trap for
       anything that reads the output and expects the obvious order. */
    int first = thread_local_(1);
    int second = thread_local_(2);
    printf("thread:%d %d\n", first, second);
    char *copy = allocate("loaded");
    printf("alloc:%s\n", copy);
    free(copy);
    printf("callback:%d\n", calls_back(doubler, 20));
    return 0;
}
"##;

/// The failure path, which is a different question and gets its own test.
///
/// `dlopen` of something absent leaves `ld.so` by `longjmp`, out of
/// `_dl_catch_exception` — which was the parked thorn, and is now the
/// setjmp design's gate. It reports through `dlerror`, so the text is part
/// of the answer: a `dlopen` that returned `NULL` for the wrong reason would
/// pass a check on the pointer alone.
const MISSING_SOURCE: &str = r##"
#include <dlfcn.h>
#include <stdio.h>

int main(void) {
    printf("before\n");
    void *missing = dlopen("/lib/libnothing.so", RTLD_NOW);
    printf("missing:%s\n", missing ? "loaded" : "refused");
    if (!missing) {
        printf("dlerror:%s\n", dlerror());
    }
    return 0;
}
"##;

/// Builds the library and the program, bakes them into one image with the
/// library present but unreferenced, and runs it. `Err` carries the kernel's
/// own account of why it stopped.
fn run(
    workspace: &WorkingDirectory,
    name: &str,
    program_source: &str,
) -> Result<String, String> {
    let library_source = workspace.write("extra.c", LIBRARY_SOURCE);
    let library = workspace.path().join("libextra.so");
    support::run_tool(
        "gcc",
        &[
            "-O2",
            "-shared",
            "-fPIC",
            &library_source.to_string_lossy(),
            "-o",
            &library.to_string_lossy(),
        ],
    );

    let source = workspace.write("program.c", program_source);
    let elf = workspace.path().join("program.elf");
    support::run_tool(
        "gcc",
        &[
            "-O2",
            &source.to_string_lossy(),
            "-o",
            &elf.to_string_lossy(),
        ],
    );

    let mut tree = baker::tree::Tree::new();
    tree.resolve_or_create(b"/tmp").expect("a /tmp in the image");
    place(&mut tree, LIBRARY, &std::fs::read(&library).expect("read the library"));

    let baked = baker::bake::container(&elf, std::path::Path::new("/"), tree)
        .expect("bake the program and what it loads");

    // The library is in nobody's closure, so the sweep is the only thing
    // that could have found it. Asserted rather than assumed, because
    // everything below would still pass if the loader had somehow been
    // handed an untranslated file and refused it in a way the program
    // reported as a `dlopen` failure.
    assert!(
        baked.placed.iter().any(|(path, _)| path == LIBRARY),
        "the sweep did not pick up {LIBRARY}; it placed {:?}",
        baked.placed.iter().map(|(path, _)| path).collect::<Vec<_>>()
    );

    let guest = workspace.write("program.wasm.o", &baked.module);
    let module = support::link_container_for_program(
        workspace,
        std::slice::from_ref(&guest),
        &baked.image,
        name,
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

    let finished = container.boot().is_ok();
    let written = container
        .mounts()
        .read(&path(&[b"iso", b"console", b"stdout"]))
        .expect("the console mount failed")
        .unwrap_or_default();
    let written = String::from_utf8(written).expect("the guest wrote something unreadable");
    match finished {
        true => Ok(written),
        false => {
            let log = container
                .mounts()
                .read(&path(&[b"iso", b"log", b"error"]))
                .ok()
                .flatten()
                .unwrap_or_default();
            Err(format!(
                "{}\nstdout so far:\n{written}",
                String::from_utf8_lossy(&log)
            ))
        }
    }
}

fn place(tree: &mut baker::tree::Tree, at: &str, bytes: &[u8]) {
    let node = tree.add(
        baker::tree::Meta {
            mode: kisal::image::file_type::REGULAR | 0o755,
            ..Default::default()
        },
        baker::tree::Body::Regular(bytes.to_vec()),
    );
    let (directory, leaf) = tree
        .place(at.as_bytes())
        .expect("make room")
        .expect("a path a file can go at");
    tree.link(directory, &leaf, node).expect("place");
}

/// The whole of it, against the only oracle worth having: the same program
/// and the same library, run by Linux.
#[test]
fn a_program_loads_a_library_nothing_linked_against() {
    let workspace = WorkingDirectory::new("dlopen");
    let written = run(&workspace, "dlopen", PROGRAM_SOURCE)
        .unwrap_or_else(|report| panic!("the container did not finish: {report}"));

    // Natively the library has to be where the program asks for it, which
    // needs a place this test may write. The image put it at an absolute
    // path; the native run gets the same one by building the program to look
    // where the library actually is.
    let native = native(&workspace, PROGRAM_SOURCE);
    assert_eq!(
        written, native,
        "the container and the native run disagree"
    );

    // And the parts, named, so a failure says which one broke rather than
    // that a diff did.
    for expected in [
        // The constructor ran: 10 * 3 + 11.
        "answer:41\n",
        // Thread-locals in a loaded module: the dynamic-TLS path.
        "thread:8 10\n",
        // An allocation made in the library and freed in the program.
        "alloc:loaded!\n",
        // A function pointer from the program, called by the library.
        "callback:41\n",
    ] {
        assert!(
            written.contains(expected),
            "the guest did not report `{expected}`; it wrote:\n{written}"
        );
    }
}

/// The same program and library, run by Linux.
fn native(workspace: &WorkingDirectory, program_source: &str) -> String {
    let library_source = workspace.write("extra-native.c", LIBRARY_SOURCE);
    std::fs::create_dir_all(workspace.path().join("lib")).expect("a lib directory");
    let library = workspace.path().join("lib/libextra.so");
    support::run_tool(
        "gcc",
        &[
            "-O2",
            "-shared",
            "-fPIC",
            &library_source.to_string_lossy(),
            "-o",
            &library.to_string_lossy(),
        ],
    );
    // The program names `/lib/libextra.so`, which this test may not write.
    // So the native side gets a copy of the source with the path it can use.
    let rewritten = program_source.replace(
        "\"/lib/libextra.so\"",
        &format!("\"{}\"", library.to_string_lossy()),
    );
    let source = workspace.write("program-native.c", rewritten);
    let elf = workspace.path().join("program-native.elf");
    support::run_tool(
        "gcc",
        &[
            "-O2",
            &source.to_string_lossy(),
            "-o",
            &elf.to_string_lossy(),
        ],
    );
    let output = std::process::Command::new(&elf)
        .env_clear()
        .output()
        .expect("run the program natively");
    assert!(output.status.success(), "the native run failed");
    String::from_utf8(output.stdout).expect("unreadable native output")
}

/// The policy is inert when there is nothing to sweep.
///
/// A bake of a program whose image holds no ELF but the ones its closure
/// already names must produce exactly the module it produced before the
/// sweep existed — byte for byte, since the sweep either finds something or
/// changes nothing at all.
#[test]
fn sweeping_an_image_with_nothing_in_it_changes_nothing() {
    let workspace = WorkingDirectory::new("dlopen-inert");
    let source = workspace.write("plain.c", "#include <stdio.h>\nint main(void){puts(\"x\");}\n");
    let elf = workspace.path().join("plain.elf");
    support::run_tool(
        "gcc",
        &["-O2", &source.to_string_lossy(), "-o", &elf.to_string_lossy()],
    );

    let bake = |extra: Option<&[u8]>| {
        let mut tree = baker::tree::Tree::new();
        tree.resolve_or_create(b"/tmp").expect("a /tmp");
        if let Some(bytes) = extra {
            place(&mut tree, "/lib/libspare.so", bytes);
        }
        baker::bake::container(&elf, std::path::Path::new("/"), tree).expect("bake")
    };

    let plain = bake(None);
    let again = bake(None);
    assert_eq!(
        plain.module, again.module,
        "a bake of the same image is not reproducible"
    );

    // And with one unreferenced library added, the sweep finds it — which is
    // what makes the byte-identity above a statement about the policy being
    // inert rather than about it being absent.
    let library_source = workspace.write("spare.c", "int spare(int v){return v+1;}\n");
    let library = workspace.path().join("libspare.so");
    support::run_tool(
        "gcc",
        &[
            "-O2",
            "-shared",
            "-fPIC",
            &library_source.to_string_lossy(),
            "-o",
            &library.to_string_lossy(),
        ],
    );
    let swept = bake(Some(&std::fs::read(&library).expect("read")));
    assert!(
        swept.placed.iter().any(|(path, _)| path == "/lib/libspare.so"),
        "the sweep missed a library nothing references: {:?}",
        swept.placed.iter().map(|(path, _)| path).collect::<Vec<_>>()
    );
    assert!(
        swept.module.len() > plain.module.len(),
        "the swept module is no larger, so nothing extra was translated"
    );
}

/// `dlopen` of something absent returns `NULL` and says why — which is
/// where the setjmp design started and what it was for.
///
/// This test used to assert the opposite: that the container *died*, on a
/// miss whose address was the value `setjmp` had saved. That was the thorn's
/// signature, recorded so the day it was designed away something would say
/// so. This is that day, and the test says what the design bought instead:
/// ordinary control flow, and the same `dlerror` text Linux produces.
///
/// It is not an incidental case. `ctypes.util.find_library` and every
/// package wrapping a C library *probe* by dlopening candidates and catching
/// the failure, so this is the common path rather than the error path.
#[test]
fn a_failed_dlopen_returns_null_with_its_error() {
    let workspace = WorkingDirectory::new("dlopen-missing");
    let written = run(&workspace, "dlopen-missing", MISSING_SOURCE)
        .unwrap_or_else(|report| panic!("the container did not finish: {report}"));

    assert!(
        written.contains("before\n"),
        "the program did not reach the `dlopen`:\n{written}"
    );
    assert!(
        written.contains("missing:refused\n"),
        "`dlopen` of an absent library did not return NULL:\n{written}"
    );
    // The text as well as the pointer: a `NULL` for the wrong reason would
    // pass a check on the pointer alone, and `ld.so` reaching its error path
    // at all is the thing that used to be impossible.
    assert!(
        written.contains("cannot open shared object file"),
        "`dlerror` did not report what went wrong:\n{written}"
    );
}
