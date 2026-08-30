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
use kisal::syscall::{Enforcement, Kernel};

/// A store that puts the guest's console on this process's.
#[derive(Default)]
struct Terminal {
    trace: bool,
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
        StoreOutcome::Absent
    }

    fn write(&mut self, path: &[&[u8]], bytes: &[u8]) -> StoreOutcome {
        use std::io::Write;
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
    let mut arguments = std::env::args().skip(1);
    let root = PathBuf::from(arguments.next().unwrap_or_else(|| {
        eprintln!("usage: interpret <root> [argv...]");
        std::process::exit(2);
    }));
    let given: Vec<Vec<u8>> = arguments.map(|argument| argument.into_bytes()).collect();
    let owned: Vec<Vec<u8>> = match given.is_empty() {
        true => vec![b"/init".to_vec()],
        false => given,
    };
    let argv: Vec<&[u8]> = owned.iter().map(|argument| argument.as_slice()).collect();

    let baking = std::time::Instant::now();
    let baked = baker::bake_directory(&root).expect("bake the directory");
    let image = Image::parse(&baked.index, &baked.blob).expect("parse the image");
    eprintln!(
        "baked {} in {:.2}s",
        root.display(),
        baking.elapsed().as_secs_f64()
    );

    let kernel = Kernel::with_enforcement(
        Terminal {
            trace: std::env::var_os("KISAL_TRACE").is_some(),
        },
        Interpreted::new(),
        image,
        Enforcement::Mapped,
    );
    let mut process = match Process::boot(kernel, argv[0], &argv, &[]) {
        Ok(process) => process,
        Err(error) => {
            let mut message = String::new();
            error.message(&mut message);
            eprintln!("booting failed: {message}");
            std::process::exit(1);
        }
    };

    let running = std::time::Instant::now();
    let exit = process.run();
    let elapsed = running.elapsed().as_secs_f64();
    let retired = process.kernel.machine.thread.retired;
    eprintln!(
        "\n{retired} instructions in {elapsed:.2}s = {:.1} MIPS",
        retired as f64 / elapsed / 1e6
    );
    eprintln!("blocks decoded: {}", process.cache.decoded);
    match exit {
        Exit::Status(status) => std::process::exit(status),
        other => {
            eprintln!("{other:?}");
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
