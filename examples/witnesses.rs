//! How many functions each witness accounts for.
//!
//! Usage: `cargo run --release --example witnesses -- <elf>`

fn main() {
    let path = std::env::args().nth(1).expect("usage: witnesses <elf>");
    let bytes = std::fs::read(&path).expect("read the program");
    let object = zaqaru::reader::ObjectFile::parse(&bytes).expect("parse the program");
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for function in &object.functions {
        *counts.entry(format!("{:?}", function.witness)).or_default() += 1;
    }
    println!("{} functions", object.functions.len());
    for (witness, count) in counts {
        println!("  {count:6}  {witness}");
    }
}
