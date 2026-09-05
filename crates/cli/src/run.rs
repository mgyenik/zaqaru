//! `zaqaru run`: a container module, under wasmtime.
//!
//! The host side of the boundary is small on purpose. A container asks for
//! nothing but a store: paths under `/iso` it reads and writes, which is how
//! it reaches a console, a source of entropy, a clock, and the switch it
//! throws to shut down. Everything else — files, memory, the program itself
//! — is inside the module.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Args;

#[derive(Args)]
pub struct Run {
    /// The container module.
    module: PathBuf,
    /// Publish a guest port on a host port, `HOST:GUEST`. Repeatable. A
    /// guest port with no mapping is loopback-only.
    #[arg(short, long = "publish", value_name = "HOST:GUEST", value_parser = crate::parse_port_mapping)]
    ports: Vec<(u16, u16)>,
    /// Write a syscall trace, in `strace`'s format, to this file.
    #[arg(long, value_name = "FILE")]
    trace: Option<PathBuf>,
    /// Record every answer the host gives — the whole of a run's
    /// nondeterminism — to this tape.
    #[arg(long, value_name = "TAPE", conflicts_with = "replay")]
    record: Option<PathBuf>,
    /// Run again from a tape rather than from the world.
    #[arg(long, value_name = "TAPE")]
    replay: Option<PathBuf>,
    /// Run the plain interpreter, without the bytecode accelerator.
    #[arg(long)]
    no_bytecode: bool,
    /// Fill the boot entropy with one repeated byte, for a reproducible run.
    #[arg(long, value_name = "BYTE")]
    seed: Option<u8>,
    /// Have wasmtime write a perf map, so a host profiler can name the
    /// engine's frames.
    #[arg(long)]
    perfmap: bool,
}

impl Run {
    pub fn execute(self) -> Result<i32> {
        let bytes = std::fs::read(&self.module)
            .with_context(|| format!("reading {}", self.module.display()))?;

        let mut table = mounts(self.seed);
        // No mount, no edge. A container started without `-p` still binds
        // and listens happily; nothing outside can reach it, which is
        // exactly a network namespace with only `lo` in it.
        if !self.ports.is_empty() {
            for (host, guest) in &self.ports {
                eprintln!("listening on host port {host} for guest port {guest}");
            }
            table.mount(
                &[b"iso", b"net"],
                Box::new(host::net::NetStore::new(self.ports.clone())),
            );
        }
        // A replayed run has to use the engine the recorded one did — the
        // schedule depends on where quanta end, and the two engines end
        // them differently — so the tape decides, and says so when the
        // flag disagreed.
        let mut no_bytecode = self.no_bytecode;
        if let Some(from) = &self.replay {
            let recorded_bytecode = table.replay(from).map_err(anyhow::Error::msg)?;
            if recorded_bytecode == self.no_bytecode {
                eprintln!(
                    "the tape was recorded {} the bytecode accelerator; replaying the same way",
                    if recorded_bytecode { "with" } else { "without" }
                );
            }
            no_bytecode = !recorded_bytecode;
        }
        // What the host asks of the guest is a path the host mounts: the
        // store is the only channel there is.
        if self.trace.is_some() || no_bytecode {
            let mut config = host::store::Sink::new();
            if self.trace.is_some() {
                config.place(
                    &[b"iso".to_vec(), b"config".to_vec(), b"trace".to_vec()],
                    b"1".to_vec(),
                );
            }
            if no_bytecode {
                config.place(
                    &[b"iso".to_vec(), b"config".to_vec(), b"bytecode".to_vec()],
                    b"0".to_vec(),
                );
            }
            table.mount(&[b"iso", b"config"], Box::new(config));
        }
        if let Some(to) = &self.record {
            table.record(to.clone(), !no_bytecode);
        }

        // Compiling is not running, and a container's cost is both. wasmtime
        // turns the whole module into machine code before the guest's first
        // instruction — a fixed price paid per process, on a module whose
        // size is the engine plus the image — and a report that folded it
        // into the run would make a container look slower the bigger its
        // filesystem is.
        let compiling = std::time::Instant::now();
        let options = host::Options { perfmap: self.perfmap };
        let mut container = host::Container::instantiate_with(&bytes, table, options)
            .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        eprintln!(
            "compiled {:.1} MB of module in {:.2}s",
            bytes.len() as f64 / (1 << 20) as f64,
            compiling.elapsed().as_secs_f64()
        );

        let started = std::time::Instant::now();
        let status = container.boot();
        let elapsed = started.elapsed().as_secs_f64();

        // The console was echoed as it arrived, so nothing is printed again
        // here. What the run cost: the module counts the work and cannot
        // time itself, so the rate is computed on this side or nowhere.
        report_cost(&mut container, elapsed);

        match container.mounts().keep_tape() {
            Some(Ok(answers)) => eprintln!("recorded {answers} host answers"),
            Some(Err(error)) => eprintln!("zaqaru: {error}"),
            None => {}
        }
        if let Some(where_to) = &self.trace {
            let lines = read(&mut container, &[b"iso", b"log", b"debug"]);
            std::fs::write(where_to, lines.as_bytes())
                .with_context(|| format!("writing {}", where_to.display()))?;
            eprintln!(
                "wrote {} syscalls to {}",
                lines.lines().count(),
                where_to.display()
            );
        }
        match status {
            Ok(status) => Ok(status),
            // The kernel log was echoed too, so this is the failure itself
            // rather than a replay of what led to it.
            Err(error) => bail!("the container did not finish: {error:?}"),
        }
    }
}

/// What a container boots with: a console, a kernel log, entropy, a clock,
/// and somewhere to record that it finished.
///
/// The entropy is not a default that happens to be here — a container with
/// no `/iso/random` mount has none, and that is the capability model rather
/// than an oversight. This host supplies it because it is running a program
/// on the user's behalf; one that would rather not, does not mount it.
fn mounts(seed: Option<u8>) -> host::store::MountTable {
    let mut mounts = host::store::MountTable::new();
    // Teed, so a server's output arrives while it is running rather than
    // when it stops — and a server does not stop.
    mounts.mount(
        &[b"iso", b"console"],
        Box::new(host::store::Sink::new().teed(host::store::Tee::ByStream)),
    );
    mounts.mount(
        &[b"iso", b"log"],
        Box::new(host::store::Sink::new().teed(host::store::Tee::Diagnostics)),
    );
    // Not teed: the cost summary is written once on the way out and is
    // reported in a line of this program's own.
    mounts.mount(
        &[b"iso", b"log", b"statistics"],
        Box::new(host::store::Sink::new()),
    );
    mounts.mount(&[b"iso", b"random"], Box::new(host::store::Sink::new()));
    // Listening, so Ctrl-C becomes the container's own `SIGTERM` at its
    // first process — which is how a run ends the way `docker stop` ends,
    // with the tree shutting itself down rather than being torn out.
    mounts.mount(
        &[b"iso", b"shutdown"],
        Box::new(host::store::Shutdown::listening()),
    );
    mounts.mount(&[b"iso", b"time"], Box::new(host::store::Clock::new()));
    // The container's own store, so anything holding this host can ask it
    // questions between turns; and `/iso/self`, where it declares what that
    // store serves.
    mounts.serve();
    mounts.mount(&[b"iso", b"self"], Box::new(host::store::Sink::new()));

    // Exactly as many bytes as the seed holds. `/dev/urandom` is a
    // character device and never reports end of file, so a read that stops
    // at EOF does not stop: it allocates until the machine dies.
    let mut bytes = [0u8; 32];
    match seed {
        Some(byte) => bytes = [byte; 32],
        None => {
            if let Ok(mut source) = std::fs::File::open("/dev/urandom") {
                use std::io::Read;
                let _ = source.read_exact(&mut bytes);
            }
        }
    }
    // The count is the last segment of the path, not the length of what is
    // written: the store is addressed rather than streamed.
    mounts
        .write(
            &[
                b"iso".to_vec(),
                b"random".to_vec(),
                b"bytes".to_vec(),
                b"32".to_vec(),
            ],
            &bytes,
        )
        .expect("seed the container");
    mounts
}

/// Prints how much work the container did, and how fast.
///
/// Both numbers, not one. Instructions retired says what the guest asked
/// for; seconds says what it cost; and the rate between them is the engine's
/// speed, which is the only one of the three that can be compared across
/// runs of different programs. Blocks decoded says how much of the run was
/// *new* code rather than a loop, and the bytecode share how much of it the
/// accelerator covered.
fn report_cost(container: &mut host::Container, elapsed: f64) {
    let text = read(container, &[b"iso", b"log", b"statistics"]);
    let field = |name: &str| -> Option<u64> {
        text.lines()
            .find_map(|line| line.strip_prefix(name)?.trim().parse().ok())
    };
    let (Some(retired), Some(decoded)) = (field("retired"), field("decoded")) else {
        // A container that stopped before it could say. Silence rather than
        // zeros, which would read as a run that did nothing.
        return;
    };
    let accelerated = field("accelerated").unwrap_or(0);
    let share = match retired {
        0 => 0.0,
        _ => accelerated as f64 / retired as f64 * 100.0,
    };
    eprintln!(
        "{retired} instructions in {elapsed:.2}s = {:.1} MIPS, {share:.1}% in bytecode, \
         {decoded} blocks decoded",
        retired as f64 / elapsed / 1e6
    );
}

fn read(container: &mut host::Container, path: &[&[u8]]) -> String {
    let path: Vec<Vec<u8>> = path.iter().map(|segment| segment.to_vec()).collect();
    let bytes = container
        .mounts()
        .read(&path)
        .ok()
        .flatten()
        .unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}
