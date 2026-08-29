//! Every function discovery found, and what found it.
//!
//! Usage: `cargo run --release --example functions -- <elf>`

fn main() {
    let path = std::env::args().nth(1).expect("usage: functions <elf>");
    let bytes = std::fs::read(&path).expect("read the program");
    let object = zaqaru::reader::ObjectFile::parse(&bytes).expect("parse the program");
    for function in &object.functions {
        let section = &object.sections[function.section];
        println!(
            "{:#x} +{:#x} {} {}",
            section.address + function.offset,
            function.size,
            section.name,
            function.name
        );
    }
}
