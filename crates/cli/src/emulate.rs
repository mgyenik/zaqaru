//! `zaqaru emulate`: an image, run natively through the interpreter.
//!
//! The tool the breadth grind is driven from. A test tells you a program
//! failed; this tells you *where*, how many instructions it got through, and
//! what the address space looked like when it stopped — with no module in
//! the way, so a native profiler sees the engine's own frames.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::Args;
use cpu::block::BlockCache;
use kernel::abi::{Store, StoreOutcome};
use kernel::image::Image;
use kernel::machine::Interpreted;
use kernel::run::{Exit, Process};
use kernel::syscall::{Enforcement, Kernel};
use kernel::system::System;

#[derive(Args)]
pub struct Emulate {
    /// A `docker save` tarball, or a root directory.
    source: PathBuf,
    /// Publish a guest port on a host port, `HOST:GUEST`. Repeatable.
    #[arg(short, long = "publish", value_name = "HOST:GUEST", value_parser = crate::parse_port_mapping)]
    ports: Vec<(u16, u16)>,
    /// An environment entry, `NAME=value`, appended to the image's own.
    #[arg(short, long = "env", value_name = "NAME=value")]
    environment: Vec<String>,
    /// Print a syscall trace, in `strace`'s format, to standard error.
    #[arg(long)]
    trace: bool,
    /// Run the plain interpreter, without the bytecode accelerator.
    #[arg(long)]
    no_bytecode: bool,
    /// Write the whole address profile here, one `file<TAB>offset<TAB>count`
    /// line per address. Needs a build with `--features zaqaru-cpu/profile`.
    #[arg(long, value_name = "FILE")]
    profile_out: Option<PathBuf>,
    /// The command to run, replacing the image's own.
    #[arg(last = true)]
    arguments: Vec<String>,
}

impl Emulate {
    pub fn execute(self) -> Result<i32> {
        // Ctrl-C becomes the container's own `SIGTERM`, at its first process.
        let _shutdown = host::store::Shutdown::listening();
        let baking = std::time::Instant::now();
        let source = crate::load_source(&self.source, &self.arguments, &self.environment)?;
        let argv: Vec<&[u8]> = source.argv.iter().map(|argument| argument.as_slice()).collect();
        let envp: Vec<&[u8]> = source.environment.iter().map(|held| held.as_slice()).collect();
        let image = Image::parse(&source.image.index, &source.image.blob)
            .map_err(|error| anyhow::anyhow!("the index just baked does not parse: {error:?}"))?;
        eprintln!(
            "baked {} in {:.2}s",
            self.source.display(),
            baking.elapsed().as_secs_f64()
        );

        let kernel = Kernel::with_enforcement(
            Terminal {
                trace: self.trace,
                started: std::time::Instant::now(),
                net: match self.ports.is_empty() {
                    true => None,
                    false => {
                        for (host, guest) in &self.ports {
                            eprintln!("listening on host port {host} for guest port {guest}");
                        }
                        Some(std::rc::Rc::new(std::cell::RefCell::new(
                            host::net::NetStore::new(self.ports.clone()),
                        )))
                    }
                },
            },
            Interpreted::new(),
            image,
            Enforcement::Mapped,
        );
        let cache = match self.no_bytecode {
            true => BlockCache::interpreting(),
            false => BlockCache::new(),
        };
        let process = match Process::boot_with_cache(kernel, argv[0], &argv, &envp, cache) {
            Ok(process) => process,
            Err(error) => {
                let mut message = String::new();
                error.message(&mut message);
                bail!("booting failed: {message}");
            }
        };

        let mut system = System::new(process);
        let running = std::time::Instant::now();
        let exit = system.run();
        let elapsed = running.elapsed().as_secs_f64();
        // Across every process, because a container that forks does most of
        // its work in children and a count that lost them would understate
        // the engine by however much the guest chose to fan out.
        let retired = system.retired();
        let accelerated = system.accelerated();
        let decoded = system.decoded();
        let stall = match exit == Exit::Deadlocked {
            true => system.stall(),
            false => String::new(),
        };
        let process = system.current();
        let share = match retired {
            0 => 0.0,
            _ => accelerated as f64 / retired as f64 * 100.0,
        };
        eprintln!(
            "\n{retired} instructions in {elapsed:.2}s = {:.1} MIPS, {share:.1}% in bytecode, \
             {decoded} blocks decoded",
            retired as f64 / elapsed / 1e6
        );
        // Present only in a build asked for it: `--features zaqaru-cpu/histogram`.
        // What each mnemonic *costs* differs between the native and the wasm
        // build; how often the guest executes one does not, so this is the
        // faster place to take the measurement.
        if let Some(table) = cpu::histogram::report() {
            eprint!("{table}");
        }
        // Likewise `--features zaqaru-cpu/profile`. Addresses are attributed
        // against *this* process's address space, so the report is honest
        // for a container running one process and misleading for a tree of
        // them — which is why the thing to profile is a single `python -c`
        // rather than the whole server.
        match cpu::profile::hot() {
            Some((hot, total)) => eprint!(
                "{}",
                attribute(&hot, total, &process.kernel.render_maps(), self.profile_out.as_deref())
            ),
            None if self.profile_out.is_some() => {
                bail!("--profile-out needs a build with `--features zaqaru-cpu/profile`")
            }
            None => {}
        }
        match exit {
            Exit::Status(status) => Ok(status),
            other => {
                eprintln!("{other:?}");
                eprint!("{stall}");
                // Only the mappings around whatever the guest touched, because
                // a hundred-line map of a Python process buries the two lines
                // that matter.
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
                Ok(1)
            }
        }
    }
}

/// Turns counted addresses into "which file, and how much of the run".
///
/// Two tables. The first is by mapped file, which answers the question a
/// container is actually slow for — a Python interpreter loop being itself
/// is one answer and half a run inside `memcpy` is a different one. The
/// second is the hottest individual addresses with their offset inside that
/// file, which is what `nm` needs to turn them into names.
fn attribute(hot: &[(u64, u64)], total: u64, maps: &str, profile_out: Option<&Path>) -> String {
    use std::fmt::Write;

    // `start-end ... name`, which is what `render_maps` writes.
    let regions: Vec<(u64, u64, u64, String)> = maps
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let (from, to) = fields.next()?.split_once('-')?;
            let start = u64::from_str_radix(from, 16).ok()?;
            let end = u64::from_str_radix(to, 16).ok()?;
            let offset = fields.nth(1).and_then(|held| u64::from_str_radix(held, 16).ok())?;
            let name = fields.last().unwrap_or("").to_string();
            Some((start, end, offset, name))
        })
        .collect();
    let find = |address: u64| {
        regions
            .iter()
            .find(|(start, end, _, _)| address >= *start && address < *end)
    };

    let mut by_file: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for (address, count) in hot {
        let name = match find(*address) {
            Some((_, _, _, name)) if !name.is_empty() => name.clone(),
            _ => String::from("(anonymous)"),
        };
        *by_file.entry(name).or_insert(0) += count;
    }
    let mut files: Vec<(String, u64)> = by_file.into_iter().collect();
    files.sort_by(|left, right| right.1.cmp(&left.1));

    let mut out = format!("\n{total} instructions retired, by mapped file:\n\n");
    for (name, count) in files.iter().take(15) {
        let _ = writeln!(
            out,
            "  {:>6.2}%  {:>14}  {name}",
            *count as f64 / total as f64 * 100.0,
            count
        );
    }
    // The whole profile, machine-readable, when somebody asks for it: an
    // engine cannot name a function and `nm` cannot count instructions, so
    // the join happens outside.
    if let Some(path) = profile_out {
        let mut dump = String::new();
        for (address, count) in hot {
            match find(*address) {
                Some((start, _, offset, name)) if !name.is_empty() => {
                    let _ = writeln!(dump, "{name}\t{}\t{count}", address - start + offset);
                }
                _ => {
                    let _ = writeln!(dump, "(anonymous)\t{address}\t{count}");
                }
            }
        }
        let _ = std::fs::write(path, dump);
        let _ = writeln!(out, "\nfull profile written to {}", path.display());
    }
    let _ = writeln!(out, "\nhottest addresses, with the offset `nm` wants:\n");
    let _ = writeln!(out, "  {:>6} {:>14}  {:<18} {}", "share", "retired", "file+offset", "address");
    for (address, count) in hot.iter().take(30) {
        let (where_, offset) = match find(*address) {
            Some((start, _, offset, name)) if !name.is_empty() => {
                (name.clone(), address - start + offset)
            }
            _ => (String::from("(anonymous)"), *address),
        };
        let _ = writeln!(
            out,
            "  {:>5.2}% {count:>14}  {:#018x} {where_}",
            *count as f64 / total as f64 * 100.0,
            offset
        );
    }
    out
}

/// A store that puts the guest's console on this process's.
///
/// `Clone` because a fork clones the store, and what that means is the
/// store's own decision: this one holds nothing but a flag and two
/// descriptors it does not own, so parent and child sharing the terminal is
/// exactly what copying it produces. A store that *held* the output — the
/// test harness's does — wraps itself in [`kernel::abi::Shared`] instead, and
/// then cloning shares rather than copies.
#[derive(Clone)]
struct Terminal {
    trace: bool,
    /// The `/iso/net` broker, when the caller asked for one with `-p`.
    ///
    /// Shared rather than cloned: a fork copies the store, and two processes
    /// with separate copies of the network would each accept a different
    /// half of the connections.
    net: Option<std::rc::Rc<std::cell::RefCell<host::net::NetStore>>>,
    /// Where this container's monotonic clock starts. A container sees one
    /// that begins near zero at boot, which is all `CLOCK_MONOTONIC`
    /// promises: an origin that does not move during a run, never a
    /// particular origin.
    started: std::time::Instant,
}

impl Store for Terminal {
    fn read(&mut self, path: &[&[u8]], into: &mut Vec<u8>) -> StoreOutcome {
        if self.trace && path == kernel::paths::CONFIG_TRACE {
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
        if path == kernel::paths::RANDOM_SEED {
            into.extend_from_slice(&[0x5a; 32]);
            return StoreOutcome::Present;
        }
        // Ctrl-C, through the same flag the host's `/iso/shutdown` store
        // reads — shared rather than reimplemented, so what this driver does
        // with a signal and what `zaqaru run` does with one cannot drift.
        if path == kernel::paths::SHUTDOWN_REQUESTED {
            return match host::store::Shutdown::asked() {
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
            return match host::store::Store::read(&mut *net.borrow_mut(), &owned) {
                Ok(Some(bytes)) => {
                    into.extend_from_slice(&bytes);
                    StoreOutcome::Present
                }
                Ok(None) => StoreOutcome::Absent,
                Err(_) => StoreOutcome::Failed,
            };
        }
        // The two clocks, in the format `host::store::Clock` answers with:
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
        if path == kernel::paths::TIME_REALTIME || path == kernel::paths::TIME_MONOTONIC {
            let nanoseconds = match path == kernel::paths::TIME_REALTIME {
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
            return match host::store::Store::write(&mut *net.borrow_mut(), &owned, bytes) {
                Ok(_) => StoreOutcome::Present,
                Err(_) => StoreOutcome::Failed,
            };
        }
        let text = String::from_utf8_lossy(bytes);
        if path == kernel::paths::CONSOLE_STDOUT {
            print!("{text}");
            let _ = std::io::stdout().flush();
        } else {
            eprint!("{text}");
        }
        StoreOutcome::Present
    }
}

