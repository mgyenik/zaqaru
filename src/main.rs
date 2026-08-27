use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;

use zaqaru::{abi, dump, lifter, reader, structurer, thunks, transpile, wasm_reader};

/// Which control-flow translation to use. Both must produce the same
/// results; the dispatcher is also what the structured mode falls back to for
/// a graph it cannot express.
#[derive(Clone, Copy, clap::ValueEnum)]
enum ControlFlow {
    /// Dominator-based `block`/`loop`/`if` nesting, falling back to the
    /// dispatcher where necessary.
    Structured,
    /// A `loop` around a `br_table`, always.
    Dispatcher,
}

impl From<ControlFlow> for structurer::Mode {
    fn from(choice: ControlFlow) -> Self {
        match choice {
            ControlFlow::Structured => structurer::Mode::Structured,
            ControlFlow::Dispatcher => structurer::Mode::Dispatcher,
        }
    }
}

/// Transpile a native object file to a relocatable WebAssembly object.
#[derive(Parser)]
#[command(name = "zaqaru", version, about)]
struct Arguments {
    /// Input x86-64 ELF relocatable objects (`gcc -c`).
    ///
    /// Transpiling takes exactly one. `--thunks` takes the whole set that
    /// will be linked together, because what counts as a foreign function is
    /// a property of the set, not of any one object.
    #[arg(required = true)]
    inputs: Vec<PathBuf>,

    /// Output relocatable wasm object, ready for `wasm-ld`.
    #[arg(short = 'o', long = "output")]
    output: Option<PathBuf>,

    /// Emit the interop thunk object for the given link set instead of
    /// transpiling: one `<name>_guest` for every function the set calls but
    /// none of its objects define, wrapping a typed call to the real one.
    ///
    /// Pass the wasm objects being linked against as well as the native ones.
    /// A wasm object states the type of everything it defines, which is where
    /// a foreign signature is *known* rather than inferred.
    #[arg(long)]
    thunks: bool,

    /// Signature declarations, one per line: `name(i32, f64) -> i32`.
    ///
    /// Exported functions named here get a typed host-entry wrapper instead
    /// of the uniform shim; foreign functions named here can be called
    /// through a thunk.
    #[arg(long, value_name = "FILE")]
    signatures: Option<PathBuf>,

    /// Recover signatures from the machine code and use them, so that
    /// exported functions get typed wrappers without being declared.
    ///
    /// Declarations still win where they are given: inference fills in what
    /// was not declared, and a declared name keeps its declared type.
    #[arg(long)]
    infer: bool,

    /// Print what inference recovered, and on what evidence, then stop.
    #[arg(long)]
    print_signatures: bool,

    /// Keep machine state in the shared globals throughout function bodies
    /// instead of promoting it to locals — slower output, identical
    /// semantics. For pinning a miscompile on the promotion pass.
    #[arg(long)]
    no_promote: bool,

    /// Print what the reader and lifter see, then stop.
    #[arg(long)]
    dump: bool,

    /// Print the translated module as WebAssembly text after transpiling.
    #[arg(long)]
    print: bool,

    /// How to express the control-flow graph in wasm.
    #[arg(long, value_enum, default_value = "structured")]
    control_flow: ControlFlow,
}

fn read_object(path: &std::path::Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("reading {}", path.display()))
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();

    let signatures = match &arguments.signatures {
        Some(path) => abi::SignatureTable::read(path)?,
        None => abi::SignatureTable::new(),
    };

    if arguments.thunks {
        return emit_thunks(&arguments, signatures);
    }

    let [input] = arguments.inputs.as_slice() else {
        bail!(
            "transpiling takes one input object; {} were given. Did you mean \
             `--thunks`?",
            arguments.inputs.len()
        );
    };

    let bytes = read_object(input)?;
    let object = reader::ObjectFile::parse(&bytes)
        .with_context(|| format!("reading {}", input.display()))?;

    if arguments.dump {
        let functions = lifter::lift_object(&object)?;
        print!("{}", dump::dump_object(&object, &functions));
        return Ok(());
    }

    if arguments.print_signatures {
        let inference = abi::infer::infer(&object, &signatures)?;
        print!("{}", inference.report());
        return Ok(());
    }

    // Declarations win: inference fills in what was not declared rather than
    // competing with it, so a name that had to be pinned down by hand stays
    // pinned down.
    let signatures = if arguments.infer {
        let inference = abi::infer::infer(&object, &signatures)?;
        let mut combined = inference.signatures();
        for (name, signature) in signatures.iter() {
            combined.insert(name.clone(), signature.clone());
        }
        combined
    } else {
        signatures
    };

    let translated = transpile::Transpiler::new(&object)
        .with_mode(arguments.control_flow.into())
        .with_signatures(signatures)
        .with_promotion(!arguments.no_promote)
        .transpile()
        .with_context(|| format!("transpiling {}", input.display()))?;

    if arguments.print {
        print!("{}", wasmprinter::print_bytes(&translated)?);
    }

    let output = arguments.output.unwrap_or_else(|| {
        let mut path = input.clone();
        path.set_extension("wasm.o");
        path
    });
    std::fs::write(&output, &translated)
        .with_context(|| format!("writing {}", output.display()))?;
    Ok(())
}

fn emit_thunks(arguments: &Arguments, declared: abi::SignatureTable) -> Result<()> {
    let mut native = Vec::new();
    let mut from_wasm = abi::SignatureTable::new();
    for input in &arguments.inputs {
        let bytes = read_object(input)?;
        if wasm_reader::is_wasm_object(&bytes) {
            let defined = wasm_reader::defined_signatures(&bytes)
                .with_context(|| format!("reading {}", input.display()))?;
            for (name, signature) in defined {
                from_wasm.insert(name, signature);
            }
        } else {
            native.push((input, bytes));
        }
    }

    let mut objects = Vec::new();
    for (input, bytes) in &native {
        objects.push(
            reader::ObjectFile::parse(bytes)
                .with_context(|| format!("reading {}", input.display()))?,
        );
    }

    let foreign = thunks::foreign_functions(&objects)?;
    let signatures = thunks::foreign_signatures(&declared, &from_wasm, &objects)?;
    let translated = thunks::build_thunk_object(&foreign, &signatures)?;

    if arguments.print {
        print!("{}", wasmprinter::print_bytes(&translated)?);
    }

    let Some(output) = arguments.output.clone() else {
        bail!("`--thunks` needs an explicit `-o`: it describes a link set, not one input");
    };
    std::fs::write(&output, &translated)
        .with_context(|| format!("writing {}", output.display()))?;
    Ok(())
}
