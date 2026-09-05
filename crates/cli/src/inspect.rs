//! `zaqaru image inspect`: what an image would boot as.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

#[derive(Subcommand)]
pub enum Image {
    /// Print what the image would boot as: its command, environment, working
    /// directory, and the size of the baked filesystem.
    Inspect(Inspect),
}

#[derive(Args)]
pub struct Inspect {
    /// A `docker save` tarball, or a root directory.
    source: PathBuf,
    /// List every path in the baked filesystem.
    #[arg(long)]
    list: bool,
}

impl Image {
    pub fn execute(self) -> Result<i32> {
        match self {
            Image::Inspect(inspect) => inspect.execute(),
        }
    }
}

impl Inspect {
    fn execute(self) -> Result<i32> {
        let source = crate::load_source(&self.source, &[], &[])?;
        let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
        println!(
            "command:     {}",
            source.argv.iter().map(|a| text(a)).collect::<Vec<_>>().join(" ")
        );
        for entry in &source.environment {
            println!("environment: {}", text(entry));
        }
        let parsed = kernel::image::Image::parse(&source.image.index, &source.image.blob)
            .map_err(|error| anyhow::anyhow!("the index just baked does not parse: {error:?}"))?;
        let directory = parsed.working_directory();
        if !directory.is_empty() {
            println!("directory:   {}", text(directory));
        }
        let paths = image::describe(&source.image).context("walking the index")?;
        println!(
            "filesystem:  {} entries, {:.1} MB of contents, {:.1} MB of index",
            paths.len(),
            source.image.blob.len() as f64 / 1e6,
            source.image.index.len() as f64 / 1e6
        );
        if self.list {
            for path in paths {
                println!("{path}");
            }
        }
        Ok(0)
    }
}
