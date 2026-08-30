//! Data inside `.text`, and where a function's code actually ends.
//!
//! An extent says where a function's *bytes* end; the decode says where its
//! *code* ends. A compiler keeps the two the same, and hand-written assembly
//! does not — it puts constant pools inside `.text` as a matter of course,
//! sometimes inside a symbol's own stated size. Both shapes here are taken
//! from `libcrypto.so.3`, which is what found them, and both used to refuse
//! the whole bake with "undecodable bytes".
//!
//! See `tests/corpus/data_in_text.s` for what each one is.

mod support;

use support::WorkingDirectory;
use zaqaru::reader::ObjectFile;

/// A table nothing but a `lea` points at is not a function, and stops being
/// treated as one after three bytes.
#[test]
fn a_guessed_extent_ends_where_its_code_stops_decoding() {
    let workspace = WorkingDirectory::new("data-in-text-guessed");
    let elf = support::link_corpus_executable(&workspace, "data_in_text.s", "_start", "-O1");
    let bytes = std::fs::read(&elf).expect("read");
    let object = ObjectFile::parse(&bytes).expect("parse");

    // The fixture has to be the shape it claims to be: something took the
    // table's address, so discovery minted a function there.
    let table = object
        .functions
        .iter()
        .find(|function| function.witness == zaqaru::discover::Witness::AddressTaken)
        .expect("the table was not address-taken, so this proves nothing");
    assert_eq!(
        table.extent,
        zaqaru::discover::Extent::Guessed,
        "the table's extent is stated, so it is not the case this describes"
    );

    // Three bytes: `xor %rax,%rax`, and then the byte that is not an
    // instruction. Not "less than the table", which a bounding change could
    // satisfy by accident — the exact place the decode stops.
    assert_eq!(
        table.size, 3,
        "the extent was not cut back to where the code stops"
    );

    // And nothing else was lost: the function before it keeps its own size.
    let lookup = object
        .functions
        .iter()
        .find(|function| function.name == "lookup")
        .expect("the fixture defines `lookup`");
    assert_eq!(lookup.size, 11, "a neighbour was truncated too");
}

/// A pool inside a symbol's own `.size` is data, and the symbol saying
/// otherwise does not make it code.
///
/// This is the half that used to be a build-time refusal on the argument
/// that a stated extent is the file calling these bytes code. `RC4_options`
/// says otherwise: perlasm writes `.size name,.-name` *after* the constant
/// pool, so 64 of its 208 bytes are the strings it returns. Refusing there
/// refuses every library built that way.
#[test]
fn a_stated_extent_ends_where_its_code_stops_decoding() {
    let workspace = WorkingDirectory::new("data-in-text-stated");
    let elf = support::link_corpus_executable(&workspace, "data_in_text.s", "_start", "-O1");
    let bytes = std::fs::read(&elf).expect("read");
    let object = ObjectFile::parse(&bytes).expect("parse");

    let options = object
        .functions
        .iter()
        .find(|function| function.name == "options")
        .expect("the fixture defines `options`");
    assert_eq!(
        options.extent,
        zaqaru::discover::Extent::Stated,
        "`options` no longer states its own size"
    );
    let stated = object.symbols[options.symbol.expect("a symbol")].size;
    assert!(
        options.size < stated,
        "the stated extent was not cut back: the symbol says {stated} bytes \
         and the function is {}",
        options.size
    );
    // The code itself is intact — `lea` and `ret` — which is what makes the
    // truncation a statement about the pool rather than about the function.
    assert!(
        options.size >= 8,
        "the code was cut back too far: {} bytes is not `lea` and `ret`",
        options.size
    );
}

/// And it runs, which is the only claim that matters.
#[test]
fn a_program_with_data_in_its_text_runs() {
    let workspace = WorkingDirectory::new("data-in-text-run");
    let status = support::program_agrees_with_native(&workspace, "data_in_text.s", "data-in-text");
    assert_eq!(
        status, 36,
        "the program no longer reads the table it was written to read"
    );
}
