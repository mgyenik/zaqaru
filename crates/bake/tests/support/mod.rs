//! Shared plumbing for the bake's integration tests: scratch directories,
//! host tools (`wasm-ld`, `clang`), the guest archive built once per test
//! process, and the mount tables a container boots against.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// A scratch directory that removes itself when dropped.
pub struct WorkingDirectory {
    path: PathBuf,
}

impl WorkingDirectory {
    pub fn new(label: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is before the epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("zaqaru-{label}-{unique}"));
        std::fs::create_dir_all(&path).expect("create working directory");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, contents).expect("write working file");
        path
    }
}

impl Drop for WorkingDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Runs a host tool and panics, loudly, if it fails or warns.
pub fn run_tool(program: &str, arguments: &[&str]) {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .unwrap_or_else(|error| panic!("failed to run {program}: {error}"));
    if !output.status.success() {
        panic!(
            "{program} {}\nexit: {}\nstdout:\n{}\nstderr:\n{}",
            arguments.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    // wasm-ld warnings are failures for us: every emitted object must link
    // cleanly, not merely successfully.
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("warning:") {
        panic!("{program} produced warnings:\n{stderr}");
    }
}

/// What a host tool did, for the tests that are *about* whether it succeeds.
pub struct ToolOutcome {
    pub succeeded: bool,
    pub stdout: String,
    pub stderr: String,
}

impl ToolOutcome {
    pub fn report(&self) -> String {
        format!(
            "succeeded: {}\nstdout:\n{}\nstderr:\n{}",
            self.succeeded, self.stdout, self.stderr
        )
    }
}

pub fn run_tool_capturing(program: &str, arguments: &[&str]) -> ToolOutcome {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .unwrap_or_else(|error| panic!("failed to run {program}: {error}"));
    ToolOutcome {
        succeeded: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Compiles C with clang's wasm backend into a relocatable wasm object: a
/// probe that links beside the image object and reports where the linker
/// put things.
pub fn compile_foreign_wasm_object(workspace: &WorkingDirectory, name: &str, source_text: &str) -> PathBuf {
    let source = workspace.write(&format!("{name}.c"), source_text);
    let object = workspace.path().join(format!("{name}.foreign.wasm.o"));
    let source_path = source.to_string_lossy().into_owned();
    let object_path = object.to_string_lossy().into_owned();
    run_tool(
        "clang",
        &["--target=wasm32", "-O1", "-c", &source_path, "-o", &object_path],
    );
    object
}

fn link_arguments(objects: &[PathBuf], output: &Path, extra_arguments: &[&str]) -> Vec<String> {
    let mut arguments: Vec<String> = vec!["--no-entry".into()];
    arguments.extend(extra_arguments.iter().map(|argument| argument.to_string()));
    arguments.extend(objects.iter().map(|object| object.to_string_lossy().into_owned()));
    arguments.push("-o".into());
    arguments.push(output.to_string_lossy().into_owned());
    arguments
}

/// The same link, handing back what the linker said instead of panicking.
pub fn try_link_wasm(objects: &[PathBuf], output: &Path, extra_arguments: &[&str]) -> ToolOutcome {
    let arguments = link_arguments(objects, output, extra_arguments);
    let borrowed: Vec<&str> = arguments.iter().map(String::as_str).collect();
    run_tool_capturing("wasm-ld", &borrowed)
}

/// The guest archive, built once per test process.
pub fn guest() -> &'static bake::Guest {
    static GUEST: std::sync::OnceLock<bake::Guest> = std::sync::OnceLock::new();
    GUEST.get_or_init(|| bake::Guest::build().expect("build the guest archive for wasm32"))
}

/// The mounts every container needs: a console and a log.
pub fn console_mounts() -> host::store::MountTable {
    let mut mounts = host::store::MountTable::new();
    mounts.mount(&[b"iso", b"console"], Box::new(host::store::Sink::new()));
    mounts.mount(&[b"iso", b"log"], Box::new(host::store::Sink::new()));
    mounts
}

/// The same, plus the boot entropy a container needs to have any.
///
/// Separate on purpose: a container with no `/iso/random` mount has no
/// randomness, and that is the capability model rather than an oversight. A
/// default that quietly supplied entropy would make the distinction
/// untestable.
pub fn mounts_seeded(seed: &[u8]) -> host::store::MountTable {
    let mut mounts = console_mounts();
    mounts.mount(&[b"iso", b"random"], Box::new(host::store::Sink::new()));
    mounts
        .write(
            &[b"iso".to_vec(), b"random".to_vec(), b"bytes".to_vec(), b"32".to_vec()],
            seed,
        )
        .expect("seed the container");
    mounts
}
