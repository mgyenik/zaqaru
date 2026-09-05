//! `zaqaru bake`: an image becomes a container module.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;

/// The guest archive this binary was built with.
const GUEST: &[u8] = include_bytes!(env!("ZAQARU_GUEST_ARCHIVE"));

#[derive(Args)]
pub struct Bake {
    /// A `docker save` tarball, or a root directory.
    source: PathBuf,
    /// Where to write the module. Defaults to the source's name with `.wasm`.
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// An environment entry, `NAME=value`, appended to the image's own.
    #[arg(short, long = "env", value_name = "NAME=value")]
    environment: Vec<String>,
    /// A guest archive to link instead of the embedded one: a `libguest.a`
    /// from `cargo build -p zaqaru-guest --target wasm32-unknown-unknown`.
    #[arg(long, value_name = "libguest.a")]
    guest: Option<PathBuf>,
    /// The command to run, replacing the image's own.
    #[arg(last = true)]
    arguments: Vec<String>,
}

impl Bake {
    pub fn execute(self) -> Result<i32> {
        let started = std::time::Instant::now();
        let source = crate::load_source(&self.source, &self.arguments, &self.environment)?;
        let output = match self.output {
            Some(output) => output,
            None => {
                let stem = self
                    .source
                    .file_stem()
                    .context("the source has no name to derive an output from")?;
                PathBuf::from(stem).with_extension("wasm")
            }
        };
        eprintln!(
            "image: {} argv, {} environment entries, {:.1} MB, in {:.2}s",
            source.argv.len(),
            source.environment.len(),
            (source.image.blob.len() + source.image.index.len()) as f64 / 1e6,
            started.elapsed().as_secs_f64()
        );

        // The embedded archive has to be a file for the linker to read; it
        // is written beside the output and removed after the link.
        let embedded = output.with_extension("guest.a");
        let guest = match &self.guest {
            Some(archive) => bake::Guest::from_archive(archive),
            None => {
                std::fs::write(&embedded, GUEST)
                    .with_context(|| format!("writing {}", embedded.display()))?;
                bake::Guest::from_archive(&embedded)
            }
        };
        let linking = std::time::Instant::now();
        let linked = bake::link(&source.image, &guest, &output);
        if self.guest.is_none() {
            let _ = std::fs::remove_file(&embedded);
        }
        linked?;
        eprintln!(
            "module: {} — {:.1} MB, linked in {:.2}s",
            output.display(),
            std::fs::metadata(&output)?.len() as f64 / 1e6,
            linking.elapsed().as_secs_f64()
        );
        Ok(0)
    }
}
