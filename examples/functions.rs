//! Every function discovery found, and what found it.
//!
//! Usage: `cargo run --release --example functions -- <elf> [base]`
//!
//! The base is what a bake would assign a position-independent file. It is
//! not decoration: read at zero, a shared object's text sits at the
//! addresses ordinary integer constants occupy, and the operand harvest
//! cannot tell one from the other.

fn main() {
    let path = std::env::args().nth(1).expect("usage: functions <elf> [base]");
    let base = std::env::args()
        .nth(2)
        .map(|text| {
            let text = text.trim_start_matches("0x");
            u64::from_str_radix(text, 16).expect("the base is hexadecimal")
        })
        .unwrap_or(0);
    let bytes = std::fs::read(&path).expect("read the program");
    let object = zaqaru::reader::ObjectFile::parse_at(&bytes, base).expect("parse the program");
    for function in &object.functions {
        let section = &object.sections[function.section];
        println!(
            "{:#x} +{:#x} {} {} [{:?}]",
            section.address + function.offset,
            function.size,
            section.name,
            function.name,
            function.witness
        );
    }
}
