//! The bake: an image and the guest archive become one container module.
//!
//! Three steps, and nothing reads the program in any of them. The image
//! becomes a relocatable wasm object carrying two data segments
//! ([`object`]); the guest archive — the kernel, the interpreter and the FPU,
//! compiled once for wasm32 ([`Guest`]) — is the same in every container;
//! and `wasm-ld` links the two, with the module's own data placed above the
//! region the program will be loaded into ([`layout`]).

pub mod layout;
pub mod object;
pub mod wasm;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

/// The name of the guest archive a wasm32 build of `zaqaru-guest` produces.
pub const GUEST_ARCHIVE: &str = "libguest.a";

/// The exports a container module has, and that the host calls.
pub const RUN_EXPORT: &str = "zaqaru_run";
pub const REALLOC_EXPORT: &str = "cabi_realloc";

/// The guest archive: everything in the module that is not the image.
pub struct Guest {
    archive: PathBuf,
}

impl Guest {
    /// A guest archive already built, at `archive`.
    pub fn from_archive(archive: impl Into<PathBuf>) -> Self {
        Self { archive: archive.into() }
    }

    /// Builds the guest archive from this workspace's sources, released, for
    /// wasm32, and answers where it landed.
    ///
    /// For development: a tool that ships carries its archive rather than a
    /// source tree. The build goes to its own target directory so that it
    /// never contends with a native build in the workspace's own.
    pub fn build() -> Result<Self> {
        let workspace = workspace_root()?;
        let target = workspace.join("target").join("wasm-guest");
        let output = Command::new(env!("CARGO"))
            .current_dir(&workspace)
            .env("CARGO_TARGET_DIR", &target)
            .args([
                "build",
                "-p",
                "zaqaru-guest",
                "--target",
                "wasm32-unknown-unknown",
                "--release",
            ])
            .output()
            .context("running cargo to build the guest for wasm32")?;
        ensure!(
            output.status.success(),
            "building the guest for wasm32 failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(Self::from_archive(
            target
                .join("wasm32-unknown-unknown")
                .join("release")
                .join(GUEST_ARCHIVE),
        ))
    }

    pub fn archive(&self) -> &Path {
        &self.archive
    }
}

/// The workspace this crate was compiled in, as cargo names it.
///
/// Asked of cargo rather than computed from the manifest directory, so that
/// moving the crate does not silently move the target directory with it.
fn workspace_root() -> Result<PathBuf> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let output = Command::new(env!("CARGO"))
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .arg("--manifest-path")
        .arg(&manifest)
        .output()
        .context("running cargo locate-project")?;
    ensure!(
        output.status.success(),
        "cargo could not locate the workspace:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let root = String::from_utf8(output.stdout).context("cargo's answer is not UTF-8")?;
    let root = Path::new(root.trim())
        .parent()
        .context("the workspace manifest has no directory")?;
    Ok(root.to_path_buf())
}

/// Links an image and a guest into a container module at `output`.
///
/// The image object is written beside the output and removed afterwards; a
/// failed link leaves it for inspection.
pub fn link(image: &image::Image, guest: &Guest, output: &Path) -> Result<()> {
    let object = object::emit(image)?;
    let image_object = output.with_extension("image.o");
    std::fs::write(&image_object, &object)
        .with_context(|| format!("writing {}", image_object.display()))?;
    let mut command = Command::new("wasm-ld");
    command
        .arg(&image_object)
        .arg(guest.archive())
        .args([
            "--no-entry",
            "--fatal-warnings",
            "--export-memory",
        ])
        .arg(format!("--export={RUN_EXPORT}"))
        .arg(format!("--export={REALLOC_EXPORT}"))
        // Room below the module's own data for a program that states its
        // own addresses. A position-independent one is placed above it and
        // does not need this, but a static executable's segments are
        // wherever its ELF says.
        .args(layout::link_arguments())
        .arg("-o")
        .arg(output);
    let linked = command.output().context("running wasm-ld")?;
    ensure!(
        linked.status.success(),
        "wasm-ld failed:\n{}",
        String::from_utf8_lossy(&linked.stderr)
    );
    let _ = std::fs::remove_file(&image_object);
    Ok(())
}
