//! `zaqaru`: x86-64 OCI containers, run in WebAssembly.
//!
//! One binary, four verbs. `bake` turns an image into a container module;
//! `run` runs a module under wasmtime; `emulate` runs an image natively
//! through the same kernel and interpreter, for diagnosis and profiling;
//! `image inspect` says what an image would boot as.

mod bake;
mod emulate;
mod inspect;
mod run;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "zaqaru", version, about, long_about = None)]
struct Arguments {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Bake an OCI image or a root directory into a container module.
    Bake(bake::Bake),
    /// Run a container module under wasmtime.
    Run(run::Run),
    /// Run an image natively through the interpreter, without a module.
    Emulate(emulate::Emulate),
    /// Look at an image without baking or running it.
    #[command(subcommand)]
    Image(inspect::Image),
}

fn main() -> std::process::ExitCode {
    let arguments = Arguments::parse();
    let outcome = match arguments.command {
        Command::Bake(command) => command.execute(),
        Command::Run(command) => command.execute(),
        Command::Emulate(command) => command.execute(),
        Command::Image(command) => command.execute(),
    };
    match outcome {
        Ok(status) => std::process::ExitCode::from(status.clamp(0, 255) as u8),
        Err(error) => {
            eprintln!("zaqaru: {error:#}");
            std::process::ExitCode::from(2)
        }
    }
}

/// A `HOST:GUEST` port mapping, the docker convention.
fn parse_port_mapping(text: &str) -> Result<(u16, u16), String> {
    let Some((host, guest)) = text.split_once(':') else {
        return Err(format!("`{text}` is not HOST:GUEST"));
    };
    match (host.parse::<u16>(), guest.parse::<u16>()) {
        (Ok(host), Ok(guest)) => Ok((host, guest)),
        _ => Err(format!("`{text}` is not two port numbers")),
    }
}

/// What an image or a directory boots as, with the caller's overrides.
///
/// A `docker save` tarball carries its own entrypoint, command, environment
/// and working directory; a directory has none of them and runs `/init`.
/// Arguments given on the command line replace the image's command, which
/// is what `docker run image cmd...` means; `--env` entries are appended,
/// so they take precedence over the image's own.
struct Source {
    image: image::Image,
    argv: Vec<Vec<u8>>,
    environment: Vec<Vec<u8>>,
}

fn load_source(path: &std::path::Path, arguments: &[String], environment: &[String]) -> anyhow::Result<Source> {
    use anyhow::Context;
    let given: Vec<Vec<u8>> = arguments.iter().map(|a| a.clone().into_bytes()).collect();
    let extra: Vec<Vec<u8>> = environment.iter().map(|e| e.clone().into_bytes()).collect();
    if path.is_dir() {
        let tree = image::tree::Tree::from_directory(path)
            .with_context(|| format!("reading the directory {}", path.display()))?;
        let argv = match given.is_empty() {
            true => vec![b"/init".to_vec()],
            false => given,
        };
        let baked = image::bake_tree_with_invocation(&tree, &argv, &extra)
            .context("baking the directory")?;
        return Ok(Source {
            image: baked,
            argv,
            environment: extra,
        });
    }
    let archive = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let (baked, invocation) = image::bake_archive_as_configured(&archive, &given)
        .with_context(|| format!("baking the image {}", path.display()))?;
    anyhow::ensure!(
        !invocation.argv.is_empty(),
        "the image has no entrypoint and no command, and none was given on \
         the command line, so there is nothing to run"
    );
    let mut environment = invocation.environment.clone();
    environment.extend(extra);
    let baked = match environment.len() == invocation.environment.len() {
        true => baked,
        // Re-baked with the extended environment: the environment is a fact
        // recorded in the image, so it has to be in the image.
        false => {
            let tree = image::layers::tree_from_archive(&archive).context("reading the layers")?;
            image::bake_tree_as_configured(
                &tree,
                &invocation.argv,
                &environment,
                &invocation.working_directory,
            )
            .context("baking the image with the extra environment")?
        }
    };
    Ok(Source {
        image: baked,
        argv: invocation.argv,
        environment,
    })
}
