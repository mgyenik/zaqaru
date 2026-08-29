//! Discovery, run across every ELF in a directory, as a distribution.
//!
//! **This is not a test and must never become one.** It is a
//! characterization tool, for the handful of moments when a question is
//! genuinely about the *population* of binaries rather than about a change:
//! how common a stripped static executable is, whether a `.eh_frame` hole
//! the size of busybox's is normal or freakish, what fraction of real
//! programs the reader cannot open at all. Those questions come up a few
//! times in a project's life, get answered, and are recorded in the worklog.
//!
//! It reads a couple of thousand binaries and takes minutes. Running it
//! after a change would be the wrong instrument for the wrong question at
//! several hundred times the cost of the right one: the suite pins
//! behaviour, `examples/functions` and `examples/witnesses` answer "what did
//! discovery do to *this* binary", and `examples/frames` answers "is the
//! unwind table telling the truth".
//!
//! Expect to archive this once the population questions are settled.
//!
//! Usage: `cargo run --release --example survey -- /usr/bin`
use zaqaru::reader::{Layout, ObjectFile, SectionRole, SymbolRole};

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    for root in std::env::args().skip(1) {
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                if entry.file_type().map(|k| k.is_file()).unwrap_or(false) {
                    paths.push(entry.path());
                }
            }
        }
    }
    paths.sort();
    println!("path\tkind\tpie\tstripped\tehframehdr\ttext\tfdecover\tfuncs\twitnesses\tstatus");
    let total = paths.len();
    for (index, path) in paths.into_iter().enumerate() {
        // Named on stderr *before* the read, so that a binary this cannot
        // finish is identified by the run that hangs on it rather than by
        // the absence of a line afterwards.
        eprintln!("[{}/{total}] {}", index + 1, path.display());
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if !bytes.starts_with(b"\x7fELF") {
            continue;
        }

        // e_type at offset 16: 2 = EXEC, 3 = DYN.
        let etype = u16::from_le_bytes([bytes[16], bytes[17]]);
        let pie = etype == 3;
        let has_hdr = bytes.windows(13).any(|w| w == b".eh_frame_hdr");
        // A cap, because one binary must not decide whether the population
        // question gets answered. It is not a judgement that large binaries
        // do not matter — `arm-none-eabi-lto-dump` is 27 MB and discovery
        // did not finish it in three minutes, which is a finding of its own
        // and is reported as a skip rather than hidden as a hang.
        const CAP: usize = 64 << 20;
        if bytes.len() > CAP {
            println!(
                "{}\t-\t{pie}\t-\t{has_hdr}\t0\t0.0\t0\t\tskipped: {} bytes",
                path.display(),
                bytes.len()
            );
            use std::io::Write;
            let _ = std::io::stdout().flush();
            continue;
        }
        let parsed = std::panic::catch_unwind(|| ObjectFile::parse(&bytes));
        let row = match parsed {
            Ok(Ok(object)) => {
                let kind = if object.layout == Layout::Linked {
                    "linked"
                } else {
                    "reloc"
                };
                let stripped = !object
                    .symbols
                    .iter()
                    .any(|s| s.role == SymbolRole::Function && s.defined);
                let text: u64 = object
                    .sections
                    .iter()
                    .filter(|s| s.role == SectionRole::Text)
                    .map(|s| s.bytes.len() as u64)
                    .sum();
                let mut frames = 0u64;
                for section in &object.sections {
                    if section.name == ".eh_frame"
                        && !section.bytes.is_empty()
                        && let Ok(read) = zaqaru::eh_frame::frames(&section.bytes, section.address)
                    {
                        frames += read.iter().map(|f| f.length).sum::<u64>();
                    }
                }
                let cover = if text == 0 {
                    0.0
                } else {
                    100.0 * frames as f64 / text as f64
                };
                let mut kinds: std::collections::BTreeMap<String, usize> = Default::default();
                for function in &object.functions {
                    *kinds.entry(format!("{:?}", function.witness)).or_default() += 1;
                }
                let witnesses: Vec<String> =
                    kinds.iter().map(|(k, v)| format!("{k}={v}")).collect();
                format!(
                    "{kind}\t{pie}\t{stripped}\t{has_hdr}\t{text}\t{cover:.1}\t{}\t{}\tok",
                    object.functions.len(),
                    witnesses.join(",")
                )
            }
            Ok(Err(error)) => format!(
                "-\t{pie}\t-\t{has_hdr}\t0\t0.0\t0\t\tparse: {}",
                format!("{error:#}").replace(['\t', '\n'], " ")
            ),
            Err(_) => format!("-\t{pie}\t-\t{has_hdr}\t0\t0.0\t0\t\tpanic"),
        };
        println!("{}\t{row}", path.display());
        // Flushed per row: a run this long is watched while it runs, and
        // block-buffered output shows nothing until it is over.
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}
