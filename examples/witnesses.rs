//! How many functions each witness accounts for.
//!
//! Usage: `cargo run --release --example witnesses -- <elf>`

fn main() {
    let path = std::env::args().nth(1).expect("usage: witnesses <elf> [base]");
    let base = std::env::args()
        .nth(2)
        .map(|text| {
            let text = text.trim_start_matches("0x");
            u64::from_str_radix(text, 16).expect("the base is hexadecimal")
        })
        .unwrap_or(0);
    let bytes = std::fs::read(&path).expect("read the program");
    let object = zaqaru::reader::ObjectFile::parse_at(&bytes, base).expect("parse the program");
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for function in &object.functions {
        *counts.entry(format!("{:?}", function.witness)).or_default() += 1;
    }
    println!("{} functions", object.functions.len());
    for (witness, count) in counts {
        println!("  {count:6}  {witness}");
    }
}
