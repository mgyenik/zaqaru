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
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => {
                println!(
                    "usage: zaqaru-run <container.wasm> [-p HOST:GUEST] [--trace <file>]"
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
        table.mount(&[b"iso", b"net"], Box::new(runner::net::NetStore::new(ports)));
    }
    if trace.is_some() {
        let mut config = runner::store::Sink::new();
        // The whole path: a mount table hands its store the path the guest
        // asked for, not the part after the prefix.
        config.place(
            &[b"iso".to_vec(), b"config".to_vec(), b"trace".to_vec()],
            b"1".to_vec(),
        );
        table.mount(&[b"iso", b"config"], Box::new(config));
    }
    let mut container = match runner::Container::instantiate(&bytes, table) {
        Ok(container) => container,
        Err(error) => {
            eprintln!("zaqaru-run: {error:?}");
            return ExitCode::from(2);
        }
    };

    let status = container.boot();

    // The console first, whichever way the boot went: a container that
    // failed part way through has usually said why, and throwing that away
    // to report the failure would be the wrong half.
    print!("{}", console(&mut container, b"stdout"));
    eprint!("{}", console(&mut container, b"stderr"));

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
            let log = read(&mut container, &[b"iso", b"log", b"error"]);
            eprintln!("zaqaru-run: the container did not finish: {error:?}");
            if !log.is_empty() {
                eprintln!("zaqaru-run: kernel log: {log}");
            }
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
fn mounts() -> runner::store::MountTable {
    let mut mounts = runner::store::MountTable::new();
    mounts.mount(&[b"iso", b"console"], Box::new(runner::store::Sink::new()));
    mounts.mount(&[b"iso", b"log"], Box::new(runner::store::Sink::new()));
    mounts.mount(&[b"iso", b"random"], Box::new(runner::store::Sink::new()));
    mounts.mount(&[b"iso", b"shutdown"], Box::new(runner::store::Sink::new()));
    mounts.mount(&[b"iso", b"time"], Box::new(runner::store::Clock::new()));

    // Exactly as many bytes as the seed holds. `/dev/urandom` is a
    // character device and never reports end of file, so a read that stops
    // at EOF does not stop: it allocates until the machine dies.
    let mut seed = [0u8; 32];
    if let Ok(mut source) = std::fs::File::open("/dev/urandom") {
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

fn console(container: &mut runner::Container, stream: &[u8]) -> String {
    read(container, &[b"iso", b"console", stream])
}

fn read(container: &mut runner::Container, path: &[&[u8]]) -> String {
    let path: Vec<Vec<u8>> = path.iter().map(|segment| segment.to_vec()).collect();
    let bytes = container
        .mounts()
        .read(&path)
        .ok()
        .flatten()
        .unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}
