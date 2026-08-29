//! What `.eh_frame` says about where the functions are, and where it stops.
//!
//! Unwind tables are the strongest witness a stripped binary carries: C is
//! compiled with asynchronous unwind tables by default, so there is normally
//! one frame description entry per function, each stating the two things
//! discovery needs. "Normally" is the word this tool exists to check —
//! before building a weaker witness to cover a hole, it is worth knowing
//! whether the hole is in the binary or in the parser.
//!
//! Prints what `src/eh_frame.rs` reads, the fraction of executable bytes it
//! accounts for, and the gaps it leaves, largest first. Compare the frame
//! count against `readelf --debug-dump=frames <elf> | grep -c FDE`.
//!
//! Usage: `cargo run --release --example frames -- <linked-elf>`

use zaqaru::reader::{ObjectFile, SectionRole};

fn main() {
    let path = std::env::args().nth(1).expect("usage: frames <elf>");
    let bytes = std::fs::read(&path).expect("read the program");
    let object = ObjectFile::parse(&bytes).expect("parse the program");

    let mut frames: Vec<(u64, u64)> = Vec::new();
    for section in &object.sections {
        if section.name != ".eh_frame" || section.bytes.is_empty() {
            continue;
        }
        let read = zaqaru::eh_frame::frames(&section.bytes, section.address)
            .expect("read the frame table");
        frames.extend(read.iter().map(|frame| (frame.address, frame.length)));
    }
    frames.sort_unstable();
    println!("frames: {}", frames.len());
    if frames.is_empty() {
        return;
    }

    // Per executable section, because a hole in one says nothing about
    // another and the totals would hide which.
    for section in &object.sections {
        if section.role != SectionRole::Text || section.bytes.is_empty() {
            continue;
        }
        let start = section.address;
        let end = start + section.bytes.len() as u64;
        let inside: Vec<(u64, u64)> = frames
            .iter()
            .copied()
            .filter(|(address, _)| (start..end).contains(address))
            .collect();
        let covered: u64 = inside.iter().map(|(_, length)| length).sum();
        println!(
            "\n{} {start:#x}..{end:#x}  {} bytes",
            section.name,
            end - start
        );
        println!(
            "  {} frames covering {covered} bytes ({:.1}%)",
            inside.len(),
            100.0 * covered as f64 / (end - start) as f64
        );

        let mut gaps: Vec<(u64, u64)> = Vec::new();
        let mut cursor = start;
        for (address, length) in &inside {
            if *address > cursor {
                gaps.push((cursor, address - cursor));
            }
            cursor = cursor.max(address + length);
        }
        if cursor < end {
            gaps.push((cursor, end - cursor));
        }
        gaps.sort_unstable_by_key(|(_, length)| std::cmp::Reverse(*length));
        let total: u64 = gaps.iter().map(|(_, length)| length).sum();
        println!("  {} gaps, {total} bytes uncovered", gaps.len());
        for (address, length) in gaps.iter().take(5) {
            println!(
                "    {address:#x} +{length} ({:.1} KiB)",
                *length as f64 / 1024.0
            );
        }
    }
}
