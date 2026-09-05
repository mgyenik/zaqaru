//! `zaqaru-run`: boots a container module and reports what it did.
//!
//! The host side of the boundary is small on purpose. A container asks for
//! nothing but a store: paths under `/iso` it reads and writes, which is how
//! it reaches a console, a source of entropy, and the switch it throws to
//! shut down. Everything else — files, memory, the program itself — is
//! inside the module.
//!
//! So this is the whole host: mount a console, a kernel log, some entropy
//! and a shutdown sink, boot, then copy the console to this process's own
//! streams and exit with the status the guest asked for.

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut path = None;
    let mut trace = None;
    let mut ports: Vec<(u16, u16)> = Vec::new();
    let mut record = None;
    let mut replay = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => {
                println!(
                    "usage: zaqaru-run <container.wasm> [-p HOST:GUEST] \
                     [--trace <file>] [--record <tape> | --replay <tape>]"
                );
                return ExitCode::SUCCESS;
            }
            // A syscall trace, in the shape `strace` prints. The guest is
            // told to produce one by the presence of a mount, which is the
            // only channel there is: the host interface is the store, so a
            // question the host wants answered is a path the host mounts.
            "--trace" => match arguments.next() {
                Some(where_to) => trace = Some(where_to),
                None => {
                    eprintln!("zaqaru-run: `--trace` needs a file to write to");
                    return ExitCode::from(2);
                }
            },
            // `-p HOST:GUEST`, repeatable, the docker convention — and the
            // capability model's firewall, living host-side as
            // configuration. A guest port with no mapping is loopback-only,
            // which is not an error and which the guest cannot tell.
            "-p" | "--publish" => {
                let Some(mapping) = arguments.next() else {
                    eprintln!("zaqaru-run: `-p` needs HOST:GUEST");
                    return ExitCode::from(2);
                };
                let Some((host, guest)) = mapping.split_once(':') else {
                    eprintln!("zaqaru-run: `-p {mapping}` is not HOST:GUEST");
                    return ExitCode::from(2);
                };
                match (host.parse::<u16>(), guest.parse::<u16>()) {
                    (Ok(host), Ok(guest)) => ports.push((host, guest)),
                    _ => {
                        eprintln!("zaqaru-run: `-p {mapping}` is not two port numbers");
                        return ExitCode::from(2);
                    }
                }
            }
            // Every answer the host gives, kept — which is the whole of a
            // run's nondeterminism, because everything else is a function
            // of the guest's own execution.
            "--record" => match arguments.next() {
                Some(where_to) => record = Some(std::path::PathBuf::from(where_to)),
                None => {
                    eprintln!("zaqaru-run: `--record` needs a file to write to");
                    return ExitCode::from(2);
                }
            },
            // And the same run again, from the tape rather than the world.
            "--replay" => match arguments.next() {
                Some(from) => replay = Some(std::path::PathBuf::from(from)),
                None => {
                    eprintln!("zaqaru-run: `--replay` needs a tape to read");
                    return ExitCode::from(2);
                }
            },
            other if other.starts_with('-') => {
                eprintln!("zaqaru-run: unknown option `{other}`");
                return ExitCode::from(2);
            }
            other => path = Some(other.to_string()),
        }
    }
    let Some(path) = path else {
        eprintln!("usage: zaqaru-run <container.wasm> [-p HOST:GUEST] [--trace <file>]");
        return ExitCode::from(2);
    };

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("zaqaru-run: reading {path}: {error}");
            return ExitCode::from(2);
        }
    };

    let mut table = mounts();
    // No mount, no edge. A container started without `-p` still binds and
    // listens happily; nothing outside can reach it, which is exactly a
    // network namespace with only `lo` in it.
    if !ports.is_empty() {
        for (host, guest) in &ports {
            eprintln!("listening on host port {host} for guest port {guest}");
        }
        table.mount(&[b"iso", b"net"], Box::new(host::net::NetStore::new(ports)));
    }
    if trace.is_some() {
        let mut config = host::store::Sink::new();
        // The whole path: a mount table hands its store the path the guest
        // asked for, not the part after the prefix.
        config.place(
            &[b"iso".to_vec(), b"config".to_vec(), b"trace".to_vec()],
            b"1".to_vec(),
        );
        table.mount(&[b"iso", b"config"], Box::new(config));
    }
    if record.is_some() && replay.is_some() {
        eprintln!("zaqaru-run: `--record` and `--replay` are opposite ends of the same tape");
        return ExitCode::from(2);
    }
    if let Some(to) = &record {
        table.record(to.clone());
    }
    if let Some(from) = &replay {
        if let Err(error) = table.replay(from) {
            eprintln!("zaqaru-run: {error}");
            return ExitCode::from(2);
        }
    }
    // Compiling is not running, and a container's cost is both. wasmtime
    // turns the whole module into machine code before the guest's first
    // instruction — a fixed price paid per process, on a module whose size
    // is the engine plus the image — and a report that folded it into the
    // run would make a container look slower the bigger its filesystem is.
    let compiling = std::time::Instant::now();
    let mut container = match host::Container::instantiate(&bytes, table) {
        Ok(container) => container,
        Err(error) => {
            eprintln!("zaqaru-run: {error:?}");
            return ExitCode::from(2);
        }
    };
    eprintln!(
        "zaqaru-run: compiled {:.1} MB of module in {:.2}s",
        bytes.len() as f64 / (1 << 20) as f64,
        compiling.elapsed().as_secs_f64()
    );

    let started = std::time::Instant::now();
    let status = container.boot();
    let elapsed = started.elapsed().as_secs_f64();

    // Already echoed as it arrived, so nothing is printed again here — a
    // container's output must not appear twice because the host kept a
    // copy of it.
    let _ = &mut container;

    // What the run cost. The module counts the work and cannot time itself;
    // this is the only side holding a clock, so the rate is computed here or
    // nowhere.
    report_cost(&mut container, elapsed);

    match container.mounts().keep_tape() {
        Some(Ok(answers)) => eprintln!("zaqaru-run: recorded {answers} host answers"),
        Some(Err(error)) => eprintln!("zaqaru-run: {error}"),
        None => {}
    }

    if let Some(where_to) = &trace {
        let lines = read(&mut container, &[b"iso", b"log", b"debug"]);
        match std::fs::write(where_to, lines.as_bytes()) {
            Ok(()) => eprintln!(
                "zaqaru-run: wrote {} syscalls to {where_to}",
                lines.lines().count()
            ),
            Err(error) => eprintln!("zaqaru-run: writing {where_to}: {error}"),
        }
    }

    match status {
        Ok(status) => ExitCode::from(status.clamp(0, 255) as u8),
        Err(error) => {
            // The kernel log was teed too, so this is the failure itself
            // rather than a replay of what led to it.
            eprintln!("zaqaru-run: the container did not finish: {error:?}");
            ExitCode::from(1)
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
fn mounts() -> host::store::MountTable {
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
    // Not teed: the kernel log is echoed as it arrives because a server's
    // diagnostics are worth nothing after it stops, but the cost summary is
    // written once on the way out and is reported in a line of this
    // program's own.
    mounts.mount(
        &[b"iso", b"log", b"statistics"],
        Box::new(host::store::Sink::new()),
    );
    mounts.mount(&[b"iso", b"random"], Box::new(host::store::Sink::new()));
    // Listening, so Ctrl-C becomes the container's own `SIGTERM` at its
    // first process — which is how a demo ends the way `docker stop` ends,
    // with the tree shutting itself down rather than being torn out.
    mounts.mount(
        &[b"iso", b"shutdown"],
        Box::new(host::store::Shutdown::listening()),
    );
    mounts.mount(&[b"iso", b"time"], Box::new(host::store::Clock::new()));

    // Exactly as many bytes as the seed holds. `/dev/urandom` is a
    // character device and never reports end of file, so a read that stops
    // at EOF does not stop: it allocates until the machine dies.
    let mut seed = [0u8; 32];
    // `ZAQARU_SEED=<byte>` fills the seed with one repeated byte, so a run
    // can be made to match a test's fixed seed for reproduction.
    if let Some(byte) = std::env::var("ZAQARU_SEED").ok().and_then(|value| value.parse::<u8>().ok()) {
        seed = [byte; 32];
    } else if let Ok(mut source) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = source.read_exact(&mut seed);
    }
    // The count is the last segment of the path, not the length of what is
    // written: the store is addressed rather than streamed. Getting this
    // wrong is not quiet, but it is indirect — the guest's `getrandom` finds
    // nothing, and glibc responds by seeding itself from `clock_gettime`
    // instead. So the failure is reported here rather than ignored, because
    // the place it surfaces otherwise is a syscall three layers away.
    mounts
        .write(
            &[
                b"iso".to_vec(),
                b"random".to_vec(),
                b"bytes".to_vec(),
                b"32".to_vec(),
            ],
            &seed,
        )
        .expect("seed the container");
    mounts
}

/// Prints how much work the container did, and how fast.
///
/// Both numbers, not one. Instructions retired says what the guest asked
/// for; seconds says what it cost; and the rate between them is the engine's
/// speed, which is the only one of the three that can be compared across
/// runs of different programs. Blocks decoded is the fourth and says how
/// much of the run was *new* code rather than a loop — a program whose block
/// count keeps climbing is one the decoder never gets ahead of.
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
    let share = if retired > 0 {
        accelerated as f64 / retired as f64 * 100.0
    } else {
        0.0
    };
    eprintln!(
        "zaqaru-run: {retired} instructions in {elapsed:.2}s = {:.1} MIPS, \
         {share:.1}% in bytecode, {decoded} blocks decoded",
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
