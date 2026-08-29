//! Booting a real binary: a static glibc `hello`, start to finish.
//!
//! Every other test drives code this repository compiled for the purpose.
//! This one takes a program built the way a distribution builds one — the
//! whole C library statically linked in, a thousand functions, hand-written
//! assembly, ifunc resolvers, hot/cold splitting — translates it, bakes it
//! into an image, and runs it.

mod support;

use support::WorkingDirectory;

fn path(segments: &[&[u8]]) -> Vec<Vec<u8>> {
    segments.iter().map(|segment| segment.to_vec()).collect()
}

#[test]
fn a_static_glibc_hello_runs() {
    let out = run_static_glibc(
        "glibc-hello",
        "#include <stdio.h>\nint main(void){ puts(\"hello\"); return 0; }\n",
    );
    assert_eq!(out, "hello\n");
}

/// A `long double` end to end, through the library code that really uses
/// one.
///
/// `strtold` and `printf("%Lg")` are not a demonstration written to exercise
/// x87 — they are glibc's own extended-precision paths, and between them
/// they run the multiplication, the comparisons, the `fprem`-driven scaling
/// and the control-word manipulation that `__printf_fp` needs. The output is
/// compared against the same program run natively, so the assertion is that
/// every one of those agreed with the hardware to the last digit printed,
/// not merely that nothing trapped.
///
/// 21 significant digits is deliberate: an extended double carries 64 bits
/// of significand, a little over 19 decimal digits, so asking for 21 prints
/// past the point where a double-backed answer could hide.
#[test]
fn a_static_glibc_long_double_prints_what_the_hardware_prints() {
    let program = r#"
#include <stdio.h>
#include <stdlib.h>
int main(void) {
    long double value = strtold("3.14159265358979323846", NULL);
    long double square = value * value;
    long double ratio = square / 2.718281828459045235L;
    printf("%.21Lg\n", value);
    printf("%.21Lg\n", square);
    printf("%.21Lg\n", ratio);
    printf("%d %d\n", value > 3.0L, ratio < value);
    return 0;
}
"#;
    let native = {
        let workspace = WorkingDirectory::new("long-double-native");
        let source = workspace.write("main.c", program);
        let elf = workspace.path().join("native.elf");
        support::run_tool(
            "gcc",
            &[
                "-static",
                "-O2",
                &source.to_string_lossy(),
                "-o",
                &elf.to_string_lossy(),
            ],
        );
        let output = std::process::Command::new(&elf)
            .output()
            .expect("run the program natively");
        assert!(output.status.success(), "the native run failed");
        let text = String::from_utf8(output.stdout).expect("native output is text");
        // Without this the comparison below could be two empty strings
        // agreeing, which is the one way this test could pass while proving
        // nothing.
        assert_eq!(
            text.lines().count(),
            4,
            "the native run printed {text:?}, which is not the four lines \
             the program writes"
        );
        text
    };

    let transpiled = run_static_glibc("glibc-long-double", program);
    assert_eq!(
        transpiled, native,
        "the container printed a different long double than the hardware did"
    );
}

/// Builds a static glibc program, translates it, bakes it into an image,
/// boots it, and returns what it wrote to stdout.
fn run_static_glibc(name: &str, program: &str) -> String {
    let workspace = WorkingDirectory::new(name);

    // Built the way a distribution builds one, which is the point.
    let source = workspace.write("program.c", program);
    let elf = workspace.path().join("program.elf");
    support::run_tool(
        "gcc",
        &[
            "-static",
            "-O2",
            &source.to_string_lossy(),
            "-o",
            &elf.to_string_lossy(),
        ],
    );

    let bytes = std::fs::read(&elf).expect("read the program");
    let object = zaqaru::reader::ObjectFile::parse(&bytes).expect("parse");
    let top = object
        .segments
        .iter()
        .map(|segment| segment.address + segment.memory_size)
        .max()
        .expect("a linked program has segments");

    let translation = zaqaru::transpile::Transpiler::new(&object)
        .with_untranslatable(zaqaru::transpile::Untranslatable::Trap)
        .translate()
        .expect("translate");
    eprintln!(
        "translated {} functions, {} refused",
        object.functions.len(),
        translation.refused.len()
    );
    // The worklist, written where a failing run can be read afterwards: the
    // workspace goes away with the test.
    let worklist: String = translation
        .refused
        .iter()
        .map(|refusal| format!("{}: {}\n", refusal.name, refusal.reason))
        .collect();
    workspace.write("refused.txt", worklist.as_bytes());
    let guest = workspace.write("program.wasm.o", &translation.module);

    let root = workspace.path().join("image");
    std::fs::create_dir_all(&root).expect("create the image tree");
    let mut placed = bytes.clone();
    baker::program::apply(&mut placed, &translation.patches).expect("apply the patches");
    std::fs::write(root.join("init"), &placed).expect("place the program");
    let image = baker::object::emit(&baker::bake_directory(&root).expect("bake"))
        .expect("emit the image object");

    // Kept where a failing run can disassemble it: the workspace goes away
    // with the test.
    let module = support::link_container_for_program(
        &workspace,
        std::slice::from_ref(&guest),
        &image,
        name,
        Some(top),
    );

    if std::env::var_os("ZAQARU_KEEP_CONTAINER").is_some() {
        std::fs::copy(&module, format!("/tmp/{name}.wasm")).expect("keep the container");
    }
    let mut mounts = support::mounts_seeded(&[0x77; 32]);
    mounts.mount(&[b"iso", b"shutdown"], Box::new(runner::store::Sink::new()));
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

    let stream = |container: &mut runner::Container, name: &[u8]| -> String {
        String::from_utf8_lossy(
            &container
                .mounts()
                .read(&path(&[b"iso", b"console", name]))
                .expect("console")
                .unwrap_or_default(),
        )
        .into_owned()
    };
    let out = stream(&mut container, b"stdout");
    let err = stream(&mut container, b"stderr");
    let log = String::from_utf8_lossy(
        &container
            .mounts()
            .read(&path(&[b"iso", b"log", b"error"]))
            .ok()
            .flatten()
            .unwrap_or_default(),
    )
    .into_owned();
    assert_eq!(
        status, 0,
        "the program exited {status}\nstdout: {out:?}\nstderr: {err:?}\n\
         kernel log: {log}"
    );
    out
}
