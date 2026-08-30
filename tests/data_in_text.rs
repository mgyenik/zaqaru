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

/// A table nothing but a `lea` points at is not a function, and does not
/// become one.
///
/// The operand harvest mints a candidate there — an instruction really does
/// take the address, because it is a lookup table — and the decode then says
/// what the bytes are: three bytes with nothing in them that ends execution.
/// Every real function has something that does, because control has to get
/// out.
///
/// Truncating it to a stub instead would not be free. A stub is *in the exec
/// map*, so an indirect transfer to a data address would call three bytes of
/// nonsense instead of missing by name — which is the one failure the whole
/// discovery design exists to avoid, arrived at from the other direction.
#[test]
fn a_table_something_takes_the_address_of_is_not_a_function() {
    let workspace = WorkingDirectory::new("data-in-text-guessed");
    let elf = support::link_corpus_executable(&workspace, "data_in_text.s", "_start", "-O1");
    let bytes = std::fs::read(&elf).expect("read");
    let object = ObjectFile::parse(&bytes).expect("parse");

    // The fixture has to be the shape it claims to be: `lookup` really does
    // take the table's address, so something had to decide about it.
    let lookup = object
        .functions
        .iter()
        .find(|function| function.name == "lookup")
        .expect("the fixture defines `lookup`");
    let text = &object.sections[lookup.section];
    let table = lookup.offset + lookup.size;

    assert!(
        !object
            .functions
            .iter()
            .any(|function| function.section == lookup.section && function.offset >= table
                && function.offset < table + 0x1c),
        "a function was left inside the table at {:#x}: {:?}",
        text.address + table,
        object
            .functions
            .iter()
            .map(|function| (&function.name, function.offset))
            .collect::<Vec<_>>()
    );
    assert!(
        !object
            .functions
            .iter()
            .any(|function| function.witness == zaqaru::discover::Witness::AddressTaken),
        "an address-taken candidate survived, and the only one here is data"
    );

    // And nothing else was lost: the function that takes the address keeps
    // its own size.
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
