//! Runs a directory as a container, interpreted, and says what happened.
//!
//! The tool the breadth grind is driven from. A test tells you a program
//! failed; this tells you *where*, how many instructions it got through, and
//! what the address space looked like when it stopped — and it takes a real
//! directory, so pointing it at a rootfs built from the host's own files is
//! one command rather than a new test.
//!
//! ```text
//! cargo run --example interpret -- <root> [argv...]
//! ```

use std::path::PathBuf;

use kisal::abi::{Store, StoreOutcome};
use kisal::image::Image;
use kisal::machine::Interpreted;
use kisal::run::{Exit, Process};
use kisal::system::System;
use kisal::syscall::{Enforcement, Kernel};

/// A store that puts the guest's console on this process's.
///
/// `Clone` because a fork clones the store, and what that means is the
/// store's own decision: this one holds nothing but a flag and two
/// descriptors it does not own, so parent and child sharing the terminal is
/// exactly what copying it produces. A store that *held* the output — the
/// test harness's does — wraps itself in [`kisal::abi::Shared`] instead, and
/// then cloning shares rather than copies.
#[derive(Clone)]
struct Terminal {
    trace: bool,
    /// The `/iso/net` broker, when the caller asked for one with `-p`.
    ///
    /// Shared rather than cloned: a fork copies the store, and two processes
    /// with separate copies of the network would each accept a different
    /// half of the connections.
    net: Option<std::rc::Rc<std::cell::RefCell<runner::net::NetStore>>>,
    /// Where this container's monotonic clock starts. A container sees one
    /// that begins near zero at boot, which is all `CLOCK_MONOTONIC`
    /// promises: an origin that does not move during a run, never a
    /// particular origin.
    started: std::time::Instant,
}

impl Store for Terminal {
    fn read(&mut self, path: &[&[u8]], into: &mut Vec<u8>) -> StoreOutcome {
        if self.trace && path == kisal::paths::CONFIG_TRACE {
            into.extend_from_slice(b"1");
            return StoreOutcome::Present;
        }
        // The boot seed, which the host chooses before the guest runs an
        // instruction. A *fixed* one here: this tool exists to make runs
        // comparable, and the design's whole claim about replay is that the
        // same container with the same inputs produces the same run. A
        // random seed would be a different input every time.
        //
        // Without it, CPython stops during pre-initialisation — hash
        // randomisation needs entropy and the design refuses to invent any,
        // which is the same capability decision as the clock.
        if path == kisal::paths::RANDOM_SEED {
            into.extend_from_slice(&[0x5a; 32]);
            return StoreOutcome::Present;
        }
        // Ctrl-C, through the same flag the runner's `/iso/shutdown` store
        // reads — shared rather than reimplemented, so what this driver does
        // with a signal and what `zaqaru-run` does with one cannot drift.
        if path == kisal::paths::SHUTDOWN_REQUESTED {
            return match runner::store::Shutdown::asked() {
                true => {
                    into.push(b'1');
                    StoreOutcome::Present
                }
                false => StoreOutcome::Absent,
            };
        }
        // The edge, if this container was given one.
        if path.first().map(|held| *held) == Some(&b"iso"[..])
            && path.get(1).map(|held| *held) == Some(&b"net"[..])
        {
            let Some(net) = self.net.as_ref() else {
                return StoreOutcome::Absent;
            };
            let owned: Vec<Vec<u8>> = path.iter().map(|held| held.to_vec()).collect();
            return match runner::store::Store::read(&mut *net.borrow_mut(), &owned) {
                Ok(Some(bytes)) => {
                    into.extend_from_slice(&bytes);
                    StoreOutcome::Present
                }
                Ok(None) => StoreOutcome::Absent,
                Err(_) => StoreOutcome::Failed,
            };
        }
        // The two clocks, in the format `runner::store::Clock` answers with:
        // nanoseconds as decimal text. Without them `clock_gettime` refuses
        // by name — which is correct, a clock is a capability the host
        // grants — and a container that cannot read one does not merely lose
        // timestamps. CPython's `time.time()` raises `OSError` before
        // `logging` has finished importing, so gunicorn does not start; and
        // nginx keeps its last cached time, which is uninitialised, and
        // stamps its log 1973.
        //
        // A real clock rather than a fixed one, and the reproducibility this
        // tool is written for survives it: scheduling here is a function of
        // *retired instructions*, so two runs interleave identically whatever
        // the clock says. What varies is only what the guest is told the time
        // is.
        if path == kisal::paths::TIME_REALTIME || path == kisal::paths::TIME_MONOTONIC {
            let nanoseconds = match path == kisal::paths::TIME_REALTIME {
                true => std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|since| since.as_nanos() as i128)
                    .unwrap_or(0),
                false => self.started.elapsed().as_nanos() as i128,
            };
            into.extend_from_slice(nanoseconds.to_string().as_bytes());
            return StoreOutcome::Present;
        }
        StoreOutcome::Absent
    }

    fn write(&mut self, path: &[&[u8]], bytes: &[u8]) -> StoreOutcome {
        use std::io::Write;
        if path.first().map(|held| *held) == Some(&b"iso"[..])
            && path.get(1).map(|held| *held) == Some(&b"net"[..])
        {
            let Some(net) = self.net.as_ref() else {
                return StoreOutcome::Absent;
            };
            let owned: Vec<Vec<u8>> = path.iter().map(|held| held.to_vec()).collect();
            return match runner::store::Store::write(&mut *net.borrow_mut(), &owned, bytes) {
                Ok(_) => StoreOutcome::Present,
                Err(_) => StoreOutcome::Failed,
            };
        }
        let text = String::from_utf8_lossy(bytes);
        if path == kisal::paths::CONSOLE_STDOUT {
            print!("{text}");
            let _ = std::io::stdout().flush();
        } else {
            eprint!("{text}");
        }
        StoreOutcome::Present
    }
}

fn main() {
    let mut ports: Vec<(u16, u16)> = Vec::new();
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut rest: Vec<String> = Vec::new();
    let mut index = 0;
    while index < raw.len() {
        match raw[index].as_str() {
            // The same `-p HOST:GUEST` `zaqaru-run` takes, so a stack can be
            // served from the native driver while it is being built and from
            // the module once it is.
            "-p" | "--publish" if index + 1 < raw.len() => {
                if let Some((host, guest)) = raw[index + 1].split_once(':')
                    && let (Ok(host), Ok(guest)) = (host.parse(), guest.parse())
                {
                    ports.push((host, guest));
                }
                index += 2;
            }
            _ => {
                rest.push(raw[index].clone());
                index += 1;
            }
        }
    }
    // Ctrl-C becomes the container's own `SIGTERM`, at its first process.
    let _shutdown = runner::store::Shutdown::listening();
    let mut arguments = rest.into_iter();
    let root = PathBuf::from(arguments.next().unwrap_or_else(|| {
        eprintln!("usage: interpret <root> [argv...]");
        std::process::exit(2);
    }));
    let given: Vec<Vec<u8>> = arguments.map(|argument| argument.into_bytes()).collect();

    let baking = std::time::Instant::now();
    // A directory or an OCI archive, because the thing worth running is
    // usually an image somebody built rather than a tree somebody curated —
    // and the archive carries its own entrypoint and environment.
    let (baked, invocation) = match root.extension().is_some_and(|kind| kind == "tar") {
        true => {
            let archive = std::fs::read(&root).expect("read the archive");
            let (image, invocation) =
                baker::bake_archive_as_configured(&archive, &given).expect("bake the archive");
            (image, Some(invocation))
        }
        false => (
            baker::bake_directory(&root).expect("bake the directory"),
            None,
        ),
    };
    // The image's own entrypoint and environment when it has them, which is
    // the whole difference between running an archive and running a tree.
    let (owned, environment): (Vec<Vec<u8>>, Vec<Vec<u8>>) = match invocation {
        // An archive's own entrypoint, unless the caller named one — which
        // is `docker run image cmd`, and is how a stack gets poked at
        // without rebuilding the image it lives in.
        Some(invocation) => (
            match given.is_empty() {
                true => invocation.argv,
                false => given,
            },
            invocation.environment,
        ),
        None => (
            match given.is_empty() {
                true => vec![b"/init".to_vec()],
                false => given,
            },
            Vec::new(),
        ),
    };
    let argv: Vec<&[u8]> = owned.iter().map(|argument| argument.as_slice()).collect();
    let envp: Vec<&[u8]> = environment.iter().map(|held| held.as_slice()).collect();

    let image = Image::parse(&baked.index, &baked.blob).expect("parse the image");
    eprintln!(
        "baked {} in {:.2}s",
        root.display(),
        baking.elapsed().as_secs_f64()
    );

    let kernel = Kernel::with_enforcement(
        Terminal {
            trace: std::env::var_os("KISAL_TRACE").is_some(),
            started: std::time::Instant::now(),
            net: match ports.is_empty() {
                true => None,
                false => {
                    for (host, guest) in &ports {
                        eprintln!("listening on host port {host} for guest port {guest}");
                    }
                    Some(std::rc::Rc::new(std::cell::RefCell::new(
                        runner::net::NetStore::new(ports),
                    )))
                }
            },
        },
        Interpreted::new(),
        image,
        Enforcement::Mapped,
    );
    let process = match Process::boot(kernel, argv[0], &argv, &envp) {
        Ok(process) => process,
        Err(error) => {
            let mut message = String::new();
            error.message(&mut message);
            eprintln!("booting failed: {message}");
            std::process::exit(1);
        }
    };

    let mut system = System::new(process);
    let running = std::time::Instant::now();
    let exit = system.run();
    let elapsed = running.elapsed().as_secs_f64();
    // Across every process, because a container that forks does most of its
    // work in children and a count that lost them would understate the
    // engine by however much the guest chose to fan out.
    let retired = system.retired();
    let decoded = system.decoded();
    let stall = match exit == Exit::Deadlocked {
        true => system.stall(),
        false => String::new(),
    };
    let process = system.current();
    eprintln!(
        "\n{retired} instructions in {elapsed:.2}s = {:.1} MIPS",
        retired as f64 / elapsed / 1e6
    );
    eprintln!("blocks decoded: {decoded}");
    // Present only in a build asked for it: `--features targum/histogram`.
    // What each mnemonic *costs* differs between the native and the wasm
    // build; how often the guest executes one does not, so this is the
    // faster place to take the measurement.
    if let Some(table) = targum::histogram::report() {
        eprint!("{table}");
    }
    match exit {
        Exit::Status(status) => std::process::exit(status),
        other => {
            eprintln!("{other:?}");
            eprint!("{stall}");
            // Only the mappings around whatever the guest touched, because a
            // hundred-line map of a Python process buries the two lines that
            // matter.
            if let Exit::Signalled { address, rip, .. } = other {
                for (label, at) in [("address", address), ("rip", rip)] {
                    let holder = process
                        .kernel
                        .space
                        .vmas()
                        .iter()
                        .find(|vma| at >= vma.start && at < vma.end());
                    match holder {
                        Some(vma) => eprintln!(
                            "  {label} {at:#x} is inside {:#x}..{:#x} prot {:#x}",
                            vma.start,
                            vma.end(),
                            vma.prot
                        ),
                        None => eprintln!("  {label} {at:#x} is in no mapping"),
                    }
                }
            }
            std::process::exit(1);
        }
    }
}
