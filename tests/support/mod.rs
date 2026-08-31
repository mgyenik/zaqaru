//! Shared plumbing for the integration tests: temporary working directories,
//! host-tool invocation (`wasm-ld`, `gcc`), and running linked modules under
//! wasmtime.

#![allow(dead_code)]

pub mod linking_format;

use std::path::{Path, PathBuf};

use std::process::Command;
use zaqaru::abi::SignatureTable;

/// A scratch directory that removes itself when dropped.
pub struct WorkingDirectory {
    path: PathBuf,
}

impl WorkingDirectory {
    pub fn new(label: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is before the epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("zaqaru-{label}-{unique}"));
        std::fs::create_dir_all(&path).expect("create working directory");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, contents).expect("write working file");
        path
    }
}

impl Drop for WorkingDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Flags the corpus is compiled with. They switch off features that are
/// explicit non-goals: control-flow protection, stack protectors, unwind
/// tables, and — for the floating-point corpus — the `errno` setting that
/// turns `sqrt` from one instruction into a call into libm, which the
/// "every symbol is defined" rule leaves nothing to link against.
pub const CORPUS_COMPILE_FLAGS: &[&str] = &[
    "-fcf-protection=none",
    "-fno-stack-protector",
    "-fno-asynchronous-unwind-tables",
    "-fno-math-errno",
];

/// Optimisation levels the corpus is built at.
///
/// This is not thoroughness for its own sake. `-O0` and `-O2` emit different
/// jump-table dispatch shapes from `-O1`; `-O2` splits cold paths into a
/// section of their own reached by a conditional tail call; `-O3` vectorises
/// loops that `-O2` leaves scalar; and `-Os` reaches for whole-register moves
/// and string instructions where the others unroll. A transpiler meant for
/// binaries it did not compile has to handle what it is given.
pub const ALL_OPTIMISATION_LEVELS: [&str; 5] = ["-O0", "-O1", "-O2", "-O3", "-Os"];

/// Which compiler built the object under test.
///
/// The two disagree about far more than style: gcc and clang pick different
/// jump-table idioms, different SSE movers for the same struct copy, and
/// different points at which a loop becomes worth vectorising. A transpiler
/// meant for binaries it did not compile has to handle both.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Compiler {
    Gcc,
    Clang,
}

impl Compiler {
    pub fn program(self) -> &'static str {
        match self {
            Compiler::Gcc => "gcc",
            Compiler::Clang => "clang",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Compiler::Gcc => "gcc",
            Compiler::Clang => "clang",
        }
    }
}

pub const ALL_COMPILERS: [Compiler; 2] = [Compiler::Gcc, Compiler::Clang];

/// How the corpus object under test was compiled.
///
/// Position independence is not a stylistic difference: it changes how the
/// compiler reaches data and, more importantly, which of the two jump-table
/// idioms it emits. Testing both is the only way to know both are handled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CodeModel {
    PositionIndependent,
    Absolute,
}

impl CodeModel {
    pub fn flag(self) -> &'static str {
        match self {
            CodeModel::PositionIndependent => "-fPIE",
            CodeModel::Absolute => "-fno-pie",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CodeModel::PositionIndependent => "pic",
            CodeModel::Absolute => "absolute",
        }
    }
}

pub fn corpus_source(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus")
        .join(name)
}

/// Compiles a corpus source to a native relocatable object.
pub fn compile_corpus_object(workspace: &WorkingDirectory, name: &str) -> PathBuf {
    compile_corpus_object_as(workspace, name, CodeModel::PositionIndependent)
}

pub fn compile_corpus_object_as(
    workspace: &WorkingDirectory,
    name: &str,
    model: CodeModel,
) -> PathBuf {
    compile_corpus_object_with(workspace, name, Compiler::Gcc, model, "-O1")
}

/// Links a corpus source into a static executable, the shape M6's front end
/// consumes.
///
/// `-nostdlib -static` and an entry point of our own: what is being tested
/// is the *front end*, and a libc would bring in thousands of functions
/// before the first one under test. The result is a complete ELF — program
/// headers, an entry point, absolute addresses, no relocations — which is
/// everything that makes linked mode different, at a size a test can reason
/// about.
pub fn link_corpus_executable(
    workspace: &WorkingDirectory,
    name: &str,
    entry: &str,
    optimisation: &str,
) -> PathBuf {
    link_corpus_executable_with(
        workspace,
        name,
        entry,
        optimisation,
        Unwind::Omitted,
        CodeModel::Absolute,
    )
}

/// Whether the linked executable carries `.eh_frame`.
///
/// The corpus is otherwise built without unwind tables, since a relocatable
/// object's is a section the translator has no use for. A real static binary
/// has one — C is compiled with asynchronous unwind tables by default — and
/// it is the only witness to where the functions are once the symbol table
/// has been stripped away.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Unwind {
    Omitted,
    Present,
}

pub fn link_corpus_executable_with(
    workspace: &WorkingDirectory,
    name: &str,
    entry: &str,
    optimisation: &str,
    unwind: Unwind,
    model: CodeModel,
) -> PathBuf {
    let source = corpus_source(name);
    let suffix = match unwind {
        Unwind::Omitted => "",
        Unwind::Present => ".unwound",
    };
    let output = workspace
        .path()
        .join(format!("{name}{optimisation}{suffix}.{}.elf", model.label()).replace('/', "."));
    let source_text = source.to_string_lossy().into_owned();
    let output_text = output.to_string_lossy().into_owned();
    let entry_flag = format!("-Wl,-e,{entry}");
    let mut arguments: Vec<&str> = if is_assembly(name) {
        Vec::new()
    } else {
        let mut flags = CORPUS_COMPILE_FLAGS.to_vec();
        if unwind == Unwind::Present {
            flags.retain(|flag| *flag != "-fno-asynchronous-unwind-tables");
            flags.push("-fasynchronous-unwind-tables");
        }
        flags.push(optimisation);
        flags.push(model.flag());
        flags
    };
    arguments.extend(["-static", "-nostdlib", "-nostartfiles", &entry_flag]);
    arguments.push(&source_text);
    arguments.push("-o");
    arguments.push(&output_text);
    run_tool(Compiler::Gcc.program(), &arguments);
    output
}

/// A copy of a linked executable with its symbol table removed, which is how
/// most shipped binaries arrive.
pub fn strip(workspace: &WorkingDirectory, path: &std::path::Path) -> PathBuf {
    let output = workspace.path().join(format!(
        "{}.stripped",
        path.file_name().expect("a file name").to_string_lossy()
    ));
    std::fs::copy(path, &output).expect("copy before stripping");
    run_tool("strip", &["--strip-all", &output.to_string_lossy()]);
    output
}

pub fn compile_corpus_object_with(
    workspace: &WorkingDirectory,
    name: &str,
    compiler: Compiler,
    model: CodeModel,
    optimisation: &str,
) -> PathBuf {
    let source = corpus_source(name);
    let object = workspace.path().join(format!(
        "{name}.{}.{}{optimisation}.o",
        compiler.label(),
        model.label()
    ));
    // A hand-written assembly source is assembled, not compiled: the C flags
    // mean nothing to it, and clang says so rather than ignoring them.
    let mut arguments: Vec<&str> = if is_assembly(name) {
        Vec::new()
    } else {
        let mut flags = CORPUS_COMPILE_FLAGS.to_vec();
        flags.push(optimisation);
        flags.push(model.flag());
        flags
    };
    arguments.push("-c");
    let source_text = source.to_string_lossy().into_owned();
    let object_text = object.to_string_lossy().into_owned();
    arguments.push(&source_text);
    arguments.push("-o");
    arguments.push(&object_text);
    run_tool(compiler.program(), &arguments);
    object
}

fn is_assembly(name: &str) -> bool {
    name.ends_with(".s") || name.ends_with(".S")
}

/// Runs a host tool, panicking with its full output on failure.
pub fn run_tool(program: &str, arguments: &[&str]) {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .unwrap_or_else(|error| panic!("failed to run {program}: {error}"));
    if !output.status.success() {
        panic!(
            "{program} {}\nexit: {}\nstdout:\n{}\nstderr:\n{}",
            arguments.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    // wasm-ld warnings are failures for us: every emitted object must link
    // cleanly, not merely successfully.
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("warning:") {
        panic!("{program} produced warnings:\n{stderr}");
    }
}

/// What a host tool did, for the cases that are *about* whether it succeeds.
///
/// `run_tool` treats any failure — and any warning — as a panic, which is
/// right when a tool invocation is a step towards something else. The interop
/// spikes invert that: the question under test is whether `wasm-ld` complains,
/// so its complaint has to come back as a value.
pub struct ToolOutcome {
    pub succeeded: bool,
    pub stdout: String,
    pub stderr: String,
}

impl ToolOutcome {
    /// Whether the tool said anything matching `needle`, on either stream.
    pub fn mentions(&self, needle: &str) -> bool {
        self.stdout.contains(needle) || self.stderr.contains(needle)
    }

    pub fn report(&self) -> String {
        format!(
            "succeeded: {}\nstdout:\n{}\nstderr:\n{}",
            self.succeeded, self.stdout, self.stderr
        )
    }
}

pub fn run_tool_capturing(program: &str, arguments: &[&str]) -> ToolOutcome {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .unwrap_or_else(|error| panic!("failed to run {program}: {error}"));
    ToolOutcome {
        succeeded: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Compiles C with clang's own wasm backend into a relocatable wasm object:
/// the *foreign* side of an interop test, standing in for any wasm object
/// zaqaru did not produce.
pub fn compile_foreign_wasm_object(
    workspace: &WorkingDirectory,
    name: &str,
    source_text: &str,
) -> PathBuf {
    let source = workspace.write(&format!("{name}.c"), source_text);
    let object = workspace.path().join(format!("{name}.foreign.wasm.o"));
    let source_path = source.to_string_lossy().into_owned();
    let object_path = object.to_string_lossy().into_owned();
    run_tool(
        "clang",
        &[
            "--target=wasm32",
            "-O1",
            "-c",
            &source_path,
            "-o",
            &object_path,
        ],
    );
    object
}

/// Links relocatable wasm objects into a module, exporting everything the
/// objects marked exported.
pub fn link_wasm(objects: &[PathBuf], output: &Path, extra_arguments: &[&str]) {
    let mut arguments: Vec<String> = vec!["--no-entry".into()];
    arguments.extend(extra_arguments.iter().map(|argument| argument.to_string()));
    arguments.extend(
        objects
            .iter()
            .map(|object| object.to_string_lossy().into_owned()),
    );
    arguments.push("-o".into());
    arguments.push(output.to_string_lossy().into_owned());

    let borrowed: Vec<&str> = arguments.iter().map(String::as_str).collect();
    run_tool("wasm-ld", &borrowed);
}

/// The same link, handing back what the linker said instead of panicking.
pub fn try_link_wasm(objects: &[PathBuf], output: &Path, extra_arguments: &[&str]) -> ToolOutcome {
    let mut arguments: Vec<String> = vec!["--no-entry".into()];
    arguments.extend(extra_arguments.iter().map(|argument| argument.to_string()));
    arguments.extend(
        objects
            .iter()
            .map(|object| object.to_string_lossy().into_owned()),
    );
    arguments.push("-o".into());
    arguments.push(output.to_string_lossy().into_owned());

    let borrowed: Vec<&str> = arguments.iter().map(String::as_str).collect();
    run_tool_capturing("wasm-ld", &borrowed)
}

/// Checks a module against the wasm specification, with the proposals a
/// linked zaqaru module relies on enabled.
pub fn validate_wasm(bytes: &[u8]) {
    let mut validator =
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::default());
    validator
        .validate_all(bytes)
        .unwrap_or_else(|error| panic!("emitted module failed validation: {error}"));
}

/// Renders a module as WebAssembly text, for readable failure output.
pub fn print_wasm(bytes: &[u8]) -> String {
    wasmprinter::print_bytes(bytes).expect("render wasm text")
}

/// Compiles a corpus source into a shared library, which is the native side
/// of a differential comparison. Position-independent code is a build-time
/// difference only: the function it defines computes the same thing.
pub fn compile_corpus_shared_library(workspace: &WorkingDirectory, sources: &[&str]) -> PathBuf {
    let library = workspace.path().join("libcorpus.so");
    let library_text = library.to_string_lossy().into_owned();
    let source_texts: Vec<String> = sources
        .iter()
        .map(|name| corpus_source(name).to_string_lossy().into_owned())
        .collect();

    let mut arguments: Vec<&str> = CORPUS_COMPILE_FLAGS.to_vec();
    arguments.extend(["-O1", "-fPIC", "-shared"]);
    arguments.extend(source_texts.iter().map(String::as_str));
    arguments.extend(["-o", &library_text]);
    run_tool("gcc", &arguments);
    library
}

pub fn specimen_source(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("specimens")
        .join(name)
}

/// Compiles a reference specimen with clang's own wasm backend, producing
/// the known-good relocatable object our emitter is compared against.
pub fn compile_specimen(workspace: &WorkingDirectory, name: &str) -> PathBuf {
    let source = specimen_source(name);
    let object = workspace.path().join(format!("{name}.wasm.o"));
    let source_text = source.to_string_lossy().into_owned();
    let object_text = object.to_string_lossy().into_owned();
    run_tool(
        "clang",
        &[
            "--target=wasm32",
            "-O0",
            "-c",
            &source_text,
            "-o",
            &object_text,
        ],
    );
    object
}

/// Runs the transpiler over a native object, checking that what comes out is
/// a valid wasm module before it ever reaches the linker.
pub fn transpile_object(input: &Path, output: &Path, mode: zaqaru::structurer::Mode) {
    let wasm = try_transpile_object(input, mode)
        .unwrap_or_else(|error| panic!("transpiling {} ({mode:?}): {error:?}", input.display()));
    std::fs::write(output, &wasm).expect("write transpiled object");
}

/// The same, handing back the failure rather than panicking, so that a sweep
/// over hundreds of configurations can report all of them at once instead of
/// stopping at the first.
pub fn try_transpile_object(
    input: &Path,
    mode: zaqaru::structurer::Mode,
) -> anyhow::Result<Vec<u8>> {
    try_transpile_object_with_signatures(input, mode, &zaqaru::abi::SignatureTable::new())
}

pub fn try_transpile_object_with_signatures(
    input: &Path,
    mode: zaqaru::structurer::Mode,
    signatures: &zaqaru::abi::SignatureTable,
) -> anyhow::Result<Vec<u8>> {
    let bytes = std::fs::read(input).expect("read native object");
    let object = zaqaru::reader::ObjectFile::parse(&bytes)?;
    let wasm = zaqaru::transpile::Transpiler::new(&object)
        .with_mode(mode)
        .with_signatures(signatures.clone())
        .transpile()?;
    validate_wasm(&wasm);
    Ok(wasm)
}

/// How an object under test is transpiled. Every combination is meant to
/// compute the same thing, which is what makes running a corpus through all
/// of them a test rather than a survey.
#[derive(Clone, Copy, Debug)]
pub struct TranspileOptions {
    pub mode: zaqaru::structurer::Mode,
    /// Promotion off leaves every machine-state access on the globals. A
    /// miscompile that disappears with it off is a promotion bug, located —
    /// which is why a new cell in the machine model is run both ways.
    pub promote: bool,
    pub resume: bool,
}

impl TranspileOptions {
    pub fn new(mode: zaqaru::structurer::Mode) -> Self {
        Self {
            mode,
            promote: true,
            resume: false,
        }
    }

    pub fn label(&self) -> String {
        format!(
            "{:?}/promote={}/resume={}",
            self.mode, self.promote, self.resume
        )
    }
}

pub fn transpile_object_configured(input: &Path, output: &Path, options: TranspileOptions) {
    let bytes = std::fs::read(input).expect("read native object");
    let object = zaqaru::reader::ObjectFile::parse(&bytes)
        .unwrap_or_else(|error| panic!("parsing {}: {error:?}", input.display()));
    let wasm = zaqaru::transpile::Transpiler::new(&object)
        .with_mode(options.mode)
        .with_promotion(options.promote)
        .with_resume(options.resume)
        .transpile()
        .unwrap_or_else(|error| {
            panic!(
                "transpiling {} ({}): {error:?}",
                input.display(),
                options.label()
            )
        });
    validate_wasm(&wasm);
    std::fs::write(output, &wasm).expect("write transpiled object");
}

/// Transpiles with the checkpoint-resume machinery on: resume IDs at call
/// sites, a resume body per function, and the weak `x86_resume` driver.
pub fn transpile_object_resumable(input: &Path, output: &Path, mode: zaqaru::structurer::Mode) {
    let bytes = std::fs::read(input).expect("read native object");
    let object = zaqaru::reader::ObjectFile::parse(&bytes)
        .unwrap_or_else(|error| panic!("parsing {}: {error:?}", input.display()));
    let wasm = zaqaru::transpile::Transpiler::new(&object)
        .with_mode(mode)
        .with_resume(true)
        .transpile()
        .unwrap_or_else(|error| panic!("transpiling {} ({mode:?}): {error:?}", input.display()));
    validate_wasm(&wasm);
    std::fs::write(output, &wasm).expect("write transpiled object");
}

/// Transpiles with signatures recovered from the machine code, plus whatever
/// was declared. Declarations win: inference fills in what was not declared
/// rather than competing with it.
pub fn transpile_object_inferring(
    input: &Path,
    output: &Path,
    mode: zaqaru::structurer::Mode,
    declared: &zaqaru::abi::SignatureTable,
) {
    let bytes = std::fs::read(input).expect("read native object");
    let object = zaqaru::reader::ObjectFile::parse(&bytes).expect("parse native object");
    let inference = zaqaru::abi::infer::infer(&object, declared)
        .unwrap_or_else(|error| panic!("inferring signatures for {}: {error:?}", input.display()));
    let mut signatures = inference.signatures();
    for (name, signature) in declared.iter() {
        signatures.insert(name.clone(), signature.clone());
    }
    transpile_object_with_signatures(input, output, mode, &signatures);
}

pub fn transpile_object_with_signatures(
    input: &Path,
    output: &Path,
    mode: zaqaru::structurer::Mode,
    signatures: &zaqaru::abi::SignatureTable,
) {
    let wasm = try_transpile_object_with_signatures(input, mode, signatures)
        .unwrap_or_else(|error| panic!("transpiling {} ({mode:?}): {error:?}", input.display()));
    std::fs::write(output, &wasm).expect("write transpiled object");
}

/// The signatures a corpus source defines, as clang's wasm backend states
/// them — the exact interface of the far side of a boundary, which is what
/// makes it knowledge rather than inference.
pub fn foreign_wasm_signatures(sources: &[&str]) -> SignatureTable {
    let workspace = WorkingDirectory::new("foreign-types");
    let mut table = SignatureTable::new();
    for source in sources {
        let text = std::fs::read_to_string(corpus_source(source))
            .unwrap_or_else(|error| panic!("reading corpus source `{source}`: {error}"));
        let object = compile_foreign_wasm_object(&workspace, &source.replace('.', "_"), &text);
        let bytes = std::fs::read(&object).expect("read foreign wasm object");
        let defined = zaqaru::wasm_reader::defined_signatures(&bytes)
            .expect("read the foreign object's function types");
        for (name, signature) in defined {
            table.insert(name, signature);
        }
    }
    table
}

/// Reads a declaration file from the corpus directory.
pub fn corpus_signatures(name: &str) -> zaqaru::abi::SignatureTable {
    zaqaru::abi::SignatureTable::read(&corpus_source(name))
        .unwrap_or_else(|error| panic!("reading corpus signatures `{name}`: {error:?}"))
}

/// Every source in the corpus directory, in a stable order.
/// Corpus sources the breadth sweep does not transpile, and why.
///
/// The sweep asks "is there anything in the corpus the transpiler refuses",
/// over *relocatable objects*. For these the answer is yes on purpose, in
/// two different ways:
///
/// - **Something untranslatable, on purpose.** A translation policy that
///   gives an untranslatable function a trapping body needs something
///   untranslatable to give one to, and `hlt` is a privileged instruction
///   nothing a container runs will ever legitimately execute — a stable
///   stand-in for whatever the gap list holds this week.
/// - **Something that only means anything linked.** `data_in_text.s` takes
///   the address of a table that lives *in* `.text`. In a relocatable object
///   the assembler resolves that reference itself — same section, known
///   distance — so no relocation survives, and the transpiler is left with a
///   text-section address whose only wasm spelling is a function's table
///   index. `0x10` is not a function, so it refuses, correctly and by the
///   address-space split `docs/design.md` opens with.
///
///   **This collision is the fixture's, not a real input's**, and the
///   distinction is worth keeping straight. The shapes it reproduces are
///   real and measured — `libcrypto.so.3` keeps AES's Te0 table in `.text`
///   at `0xb66c0` and takes its address from two `lea`s, and `RC4_options`
///   states a size that spans the strings it returns — but libcrypto is a
///   *linked* shared object, where a text address is an address and all of
///   it works. Nothing has produced this refusal except a corpus source
///   written for the linked tier and swept as a relocatable one, and
///   nothing much could: compilers put constants in `.rodata`, so it takes
///   hand-written assembly, and hand-written assembly arrives here inside
///   linked libraries. `tests/data_in_text.rs` is where the source is
///   tested, in the pipeline it was written for.
pub const DELIBERATELY_UNTRANSLATABLE: [&str; 3] = [
    "untranslatable.s",
    "calls_untranslatable.s",
    "data_in_text.s",
];

pub fn corpus_sources() -> Vec<String> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus");
    let mut names: Vec<String> = std::fs::read_dir(&directory)
        .expect("read the corpus directory")
        .map(|entry| entry.expect("corpus directory entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".c") || is_assembly(name))
        .filter(|name| !DELIBERATELY_UNTRANSLATABLE.contains(&name.as_str()))
        .collect();
    names.sort();
    names
}

/// A deterministic generator, so that a differential failure is reproducible
/// from the test alone rather than from whatever the clock happened to be.
pub struct Pseudorandom {
    state: u64,
}

impl Pseudorandom {
    pub fn new(seed: u64) -> Self {
        Self { state: seed | 1 }
    }

    pub fn next_u64(&mut self) -> u64 {
        // xorshift64*
        self.state ^= self.state >> 12;
        self.state ^= self.state << 25;
        self.state ^= self.state >> 27;
        self.state.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub fn next_i32(&mut self) -> i32 {
        self.next_u64() as i32
    }

    pub fn next_i64(&mut self) -> i64 {
        self.next_u64() as i64
    }
}

/// An instantiated module, with a store, ready to have exports called.
pub struct LinkedModule {
    store: wasmtime::Store<()>,
    instance: wasmtime::Instance,
}

impl LinkedModule {
    pub fn instantiate(path: &Path) -> Self {
        let bytes = std::fs::read(path).expect("read linked module");
        let engine = wasmtime::Engine::default();
        let module = wasmtime::Module::new(&engine, &bytes)
            .unwrap_or_else(|error| panic!("wasmtime rejected the linked module: {error}"));
        let mut store = wasmtime::Store::new(&engine, ());
        let instance = wasmtime::Instance::new(&mut store, &module, &[])
            .unwrap_or_else(|error| panic!("instantiation failed: {error}"));
        Self { store, instance }
    }

    pub fn call<Parameters, Results>(&mut self, name: &str, parameters: Parameters) -> Results
    where
        Parameters: wasmtime::WasmParams,
        Results: wasmtime::WasmResults,
    {
        let function = self
            .instance
            .get_typed_func::<Parameters, Results>(&mut self.store, name)
            .unwrap_or_else(|error| panic!("export `{name}` not usable: {error}"));
        function
            .call(&mut self.store, parameters)
            .unwrap_or_else(|error| panic!("call to `{name}` trapped: {error}"))
    }

    /// Calls an export that is expected to trap, handing the trap back rather
    /// than failing the test — for the cases where trapping *is* the correct
    /// behaviour under test.
    pub fn try_call<Parameters, Results>(
        &mut self,
        name: &str,
        parameters: Parameters,
    ) -> Result<Results, wasmtime::Error>
    where
        Parameters: wasmtime::WasmParams,
        Results: wasmtime::WasmResults,
    {
        let function = self
            .instance
            .get_typed_func::<Parameters, Results>(&mut self.store, name)
            .unwrap_or_else(|error| panic!("export `{name}` not usable: {error}"));
        function.call(&mut self.store, parameters)
    }

    /// Calls a host-entry wrapper: the six integer argument registers and the
    /// eight floating-point ones in, `rax` and `xmm0` out.
    ///
    /// The wrapper knows nothing about the function's real signature, so this
    /// is where the SysV knowledge lives instead: a caller fills the slots
    /// the C signature it is testing actually uses, leaves the rest at zero,
    /// and reads back the half of the result its return type lives in. A
    /// `float` occupies only the low half of its register, so its four bytes
    /// travel in the low half of the `f64` — reinterpreted, never converted.
    pub fn call_guest_fully(
        &mut self,
        name: &str,
        integers: [i64; 6],
        floats: [f64; 8],
    ) -> (i64, f64) {
        self.call::<(
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            f64,
            f64,
            f64,
            f64,
            f64,
            f64,
            f64,
            f64,
        ), (i64, f64)>(
            name,
            (
                integers[0],
                integers[1],
                integers[2],
                integers[3],
                integers[4],
                integers[5],
                floats[0],
                floats[1],
                floats[2],
                floats[3],
                floats[4],
                floats[5],
                floats[6],
                floats[7],
            ),
        )
    }

    /// The integer-only case: six argument registers in, `rax` out.
    pub fn call_guest(&mut self, name: &str, arguments: [i64; 6]) -> i64 {
        self.call_guest_fully(name, arguments, [0.0; 8]).0
    }

    /// Two doubles in, a double out — the shape a `long double` corpus
    /// function has, since the extended value itself cannot cross.
    pub fn call_guest_float(&mut self, name: &str, arguments: [f64; 2]) -> f64 {
        let mut floats = [0.0; 8];
        floats[..2].copy_from_slice(&arguments);
        self.call_guest_fully(name, [0; 6], floats).1
    }

    /// The same, for a function answering with an integer.
    pub fn call_guest_float_returning_integer(&mut self, name: &str, arguments: [f64; 2]) -> i64 {
        let mut floats = [0.0; 8];
        floats[..2].copy_from_slice(&arguments);
        self.call_guest_fully(name, [0; 6], floats).0
    }
}

/// Both halves of a differential comparison over the same corpus sources:
/// the natively compiled functions, and the transpiled-and-linked module.
pub struct DifferentialFixture {
    _workspace: WorkingDirectory,
    pub native: libloading::Library,
    /// One linked module per variant, labelled so that a mismatch says which
    /// combination produced it.
    pub transpiled: Vec<(String, LinkedModule)>,
}

/// The control-flow translations every corpus is run through. Comparing both
/// against the same native oracle is what makes the dispatcher useful beyond
/// being a fallback: if the structured translation ever mis-nests a loop, the
/// two disagree with native in different places.
pub const ALL_MODES: [zaqaru::structurer::Mode; 2] = [
    zaqaru::structurer::Mode::Structured,
    zaqaru::structurer::Mode::Dispatcher,
];

pub const ALL_CODE_MODELS: [CodeModel; 2] = [CodeModel::PositionIndependent, CodeModel::Absolute];

impl DifferentialFixture {
    /// Builds every side: the native oracle, and one linked module for each
    /// combination of compiler, code model, optimisation level and
    /// control-flow translation.
    ///
    /// One native oracle serves all of them. The corpus is written to be free
    /// of unspecified behaviour, so every configuration computes the same
    /// function; what differs is the machine code, which is exactly what is
    /// under test.
    ///
    /// Each source becomes its own object and is transpiled *separately*, so
    /// linking several of them together is the ordinary path rather than a
    /// special case.
    pub fn build(label: &str, sources: &[&str]) -> Self {
        let workspace = WorkingDirectory::new(label);
        let mut transpiled = Vec::new();

        for compiler in ALL_COMPILERS {
            for model in ALL_CODE_MODELS {
                for optimisation in ALL_OPTIMISATION_LEVELS {
                    let native_objects: Vec<PathBuf> = sources
                        .iter()
                        .map(|source| {
                            compile_corpus_object_with(
                                &workspace,
                                source,
                                compiler,
                                model,
                                optimisation,
                            )
                        })
                        .collect();

                    for mode in ALL_MODES {
                        let variant = format!(
                            "{}/{}{optimisation}/{mode:?}",
                            compiler.label(),
                            model.label()
                        );
                        let mut wasm_objects = Vec::new();
                        for (source, native_object) in sources.iter().zip(&native_objects) {
                            let wasm_object = workspace
                                .path()
                                .join(format!("{source}.{variant}.wasm.o").replace('/', "."));
                            transpile_object(native_object, &wasm_object, mode);
                            wasm_objects.push(wasm_object);
                        }
                        let linked = workspace
                            .path()
                            .join(format!("linked.{variant}.wasm").replace('/', "."));
                        // Unconditionally: an archive contributes only what
                        // is referenced, so a fixture with no x87 in it
                        // links exactly as before.
                        wasm_objects.push(x87_staticlib());
                        link_wasm(&wasm_objects, &linked, &[]);
                        transpiled.push((variant, LinkedModule::instantiate(&linked)));
                    }
                }
            }
        }

        let library_path = compile_corpus_shared_library(&workspace, sources);
        let native = unsafe { libloading::Library::new(&library_path) }
            .expect("load the natively compiled corpus");

        Self {
            _workspace: workspace,
            native,
            transpiled,
        }
    }
}

/// Both halves of a differential comparison for a program that is only
/// *partly* transpiled.
///
/// The native oracle is built the ordinary way: every source, guest and
/// foreign alike, compiled for x86-64 and linked into one library. The wasm
/// side is the interop path end to end — the guest sources transpiled, the
/// foreign sources compiled by clang's own wasm backend and never transpiled,
/// and a generated thunk object bridging every call from the first to the
/// second. Calls the other way need no bridge: a typed host-entry wrapper is
/// already an ordinary wasm function.
///
/// The comparison therefore answers a question the ordinary differential
/// fixture cannot ask, because there every wasm function came from us.
pub struct MixedFixture {
    _workspace: WorkingDirectory,
    pub native: libloading::Library,
    pub transpiled: Vec<(String, LinkedModule)>,
}

impl MixedFixture {
    pub fn build(
        label: &str,
        guest_sources: &[&str],
        foreign_sources: &[&str],
        signature_file: &str,
    ) -> Self {
        let workspace = WorkingDirectory::new(label);
        let signatures = corpus_signatures(signature_file);

        // The foreign half does not vary with any of the guest's build
        // choices — it is never transpiled, so nothing about how the guest
        // was compiled can reach it. Building it once is not a shortcut; a
        // per-variant rebuild would produce identical bytes.
        let foreign_objects: Vec<PathBuf> = foreign_sources
            .iter()
            .map(|source| {
                let text = std::fs::read_to_string(corpus_source(source))
                    .unwrap_or_else(|error| panic!("reading corpus source `{source}`: {error}"));
                compile_foreign_wasm_object(&workspace, &source.replace('.', "_"), &text)
            })
            .collect();

        // A wasm object states the type of everything it defines, which is
        // where a foreign signature is known rather than guessed. Reading it
        // is what lets the outgoing declarations go: a function whose
        // arguments are all passed straight through leaves no trace of them
        // in the caller, so no amount of analysis of the *native* side can
        // recover what only the far side wrote down.
        let mut known = SignatureTable::new();
        for object in &foreign_objects {
            let bytes = std::fs::read(object).expect("read foreign wasm object");
            let defined = zaqaru::wasm_reader::defined_signatures(&bytes)
                .expect("read the foreign object's function types");
            for (name, signature) in defined {
                known.insert(name, signature);
            }
        }
        // Declarations still win where they are given.
        for (name, signature) in signatures.iter() {
            known.insert(name.clone(), signature.clone());
        }

        let mut transpiled = Vec::new();
        for compiler in ALL_COMPILERS {
            for model in ALL_CODE_MODELS {
                for optimisation in ALL_OPTIMISATION_LEVELS {
                    let guest_objects: Vec<PathBuf> = guest_sources
                        .iter()
                        .map(|source| {
                            compile_corpus_object_with(
                                &workspace,
                                source,
                                compiler,
                                model,
                                optimisation,
                            )
                        })
                        .collect();

                    // What counts as foreign is a property of the whole link
                    // set, so the thunk object is generated from all of the
                    // guest objects at once rather than one at a time.
                    let parsed: Vec<Vec<u8>> = guest_objects
                        .iter()
                        .map(|path| std::fs::read(path).expect("read native object"))
                        .collect();
                    let objects: Vec<zaqaru::reader::ObjectFile> = parsed
                        .iter()
                        .map(|bytes| {
                            zaqaru::reader::ObjectFile::parse(bytes).expect("parse native object")
                        })
                        .collect();
                    let foreign_names = zaqaru::thunks::foreign_functions(&objects)
                        .expect("classify foreign functions");
                    let thunk_signatures =
                        zaqaru::thunks::foreign_signatures(&known, &known, &objects)
                            .expect("settle the foreign signatures");
                    let thunk_bytes =
                        zaqaru::thunks::build_thunk_object(&foreign_names, &thunk_signatures)
                            .expect("build the thunk object");
                    validate_wasm(&thunk_bytes);

                    for mode in ALL_MODES {
                        let variant = format!(
                            "{}/{}{optimisation}/{mode:?}",
                            compiler.label(),
                            model.label()
                        );
                        let flat = variant.replace('/', ".");

                        let mut wasm_objects = Vec::new();
                        for (source, guest_object) in guest_sources.iter().zip(&guest_objects) {
                            let wasm_object =
                                workspace.path().join(format!("{source}.{flat}.wasm.o"));
                            transpile_object_inferring(guest_object, &wasm_object, mode, &known);
                            wasm_objects.push(wasm_object);
                        }
                        wasm_objects
                            .push(workspace.write(&format!("thunks.{flat}.wasm.o"), &thunk_bytes));
                        wasm_objects.extend(foreign_objects.iter().cloned());

                        let linked = workspace.path().join(format!("linked.{flat}.wasm"));
                        wasm_objects.push(x87_staticlib());
                        // `--fatal-warnings` is the recipe, not decoration: a
                        // boundary signature that disagrees with the foreign
                        // side has to stop the build rather than be noted.
                        link_wasm(&wasm_objects, &linked, &["--fatal-warnings"]);
                        transpiled.push((variant, LinkedModule::instantiate(&linked)));
                    }
                }
            }
        }

        let all_sources: Vec<&str> = guest_sources
            .iter()
            .chain(foreign_sources.iter())
            .copied()
            .collect();
        let library_path = compile_corpus_shared_library(&workspace, &all_sources);
        let native = unsafe { libloading::Library::new(&library_path) }
            .expect("load the natively compiled mixed program");

        Self {
            _workspace: workspace,
            native,
            transpiled,
        }
    }
}

/// Looks up a native function of a given signature.
///
/// Taking the library by reference rather than the whole fixture keeps the
/// borrow disjoint from the wasm side, so a test can hold both at once.
///
/// # Safety
/// The caller must name a signature matching the corpus source.
pub unsafe fn native_function<'library, Signature: Copy>(
    library: &'library libloading::Library,
    name: &str,
) -> libloading::Symbol<'library, Signature> {
    unsafe { library.get(name.as_bytes()) }
        .unwrap_or_else(|error| panic!("native symbol `{name}`: {error}"))
}

// ---- the container pipeline ------------------------------------------------
//
// Everything below builds the M1 artefact: a transpiled guest, the generated
// kernel seam, and kisal compiled to wasm, linked into one module the runner
// can instantiate. It lives in the shared support so that every milestone
// after this one inherits it rather than re-deriving it.

/// The kisal staticlib, built for `wasm32-unknown-unknown` once per test
/// binary and cached.
///
/// Built into a target directory of its own: cargo takes a lock per
/// directory, and sharing the host build's would deadlock a test that is
/// itself running under cargo.
/// The x87 archive, built for wasm32 once per test run.
///
/// It joins every link unconditionally rather than only where an object
/// uses the stack: an archive contributes only the members something
/// references, so a container with no x87 in it pays nothing, and the
/// alternative is a "does this need x87" question asked in several places
/// that can disagree.
///
/// Its own target directory, for the reason `kisal_staticlib` has one:
/// cargo takes a lock per directory and these tests are themselves running
/// under cargo.
/// Builds a workspace crate for wasm32 and answers its staticlib.
///
/// One helper rather than one per crate: the engine made the third, and a
/// third copy of the same twenty lines is a copy too many.
pub fn wasm_staticlib(crate_name: &'static str, library: &'static str) -> PathBuf {
    use std::sync::{Mutex, OnceLock};
    static BUILT: OnceLock<Mutex<std::collections::HashMap<&'static str, PathBuf>>> =
        OnceLock::new();
    let built = BUILT.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut built = built.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(path) = built.get(crate_name) {
        return path.clone();
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = root.join("target").join(format!("wasm-{crate_name}"));
    let output = Command::new(env!("CARGO"))
        .current_dir(root)
        .env("CARGO_TARGET_DIR", &target)
        .args([
            "build",
            "-p",
            crate_name,
            "--target",
            "wasm32-unknown-unknown",
            "--release",
        ])
        .output()
        .unwrap_or_else(|error| panic!("run cargo to build {crate_name} for wasm32: {error}"));
    assert!(
        output.status.success(),
        "building {crate_name} for wasm32 failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = target
        .join("wasm32-unknown-unknown")
        .join("release")
        .join(library);
    built.insert(crate_name, path.clone());
    path
}

pub fn x87_staticlib() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"));
            let target = root.join("target").join("wasm-x87");
            let output = Command::new(env!("CARGO"))
                .current_dir(root)
                .env("CARGO_TARGET_DIR", &target)
                .args([
                    "build",
                    "-p",
                    "x87",
                    "--target",
                    "wasm32-unknown-unknown",
                    "--release",
                ])
                .output()
                .expect("run cargo to build x87 for wasm32");
            assert!(
                output.status.success(),
                "building x87 for wasm32 failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            target
                .join("wasm32-unknown-unknown")
                .join("release")
                .join("libx87.a")
        })
        .clone()
}

pub fn kisal_staticlib() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"));
            let target = root.join("target").join("wasm-kisal");
            let output = Command::new(env!("CARGO"))
                .current_dir(root)
                .env("CARGO_TARGET_DIR", &target)
                .args([
                    "build",
                    "-p",
                    "kisal",
                    "--target",
                    "wasm32-unknown-unknown",
                    "--release",
                ])
                .output()
                .expect("run cargo to build kisal for wasm32");
            assert!(
                output.status.success(),
                "building kisal for wasm32 failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            target
                .join("wasm32-unknown-unknown")
                .join("release")
                .join("libkisal.a")
        })
        .clone()
}

/// The generated kernel seam object.
pub fn seam_object() -> Vec<u8> {
    zaqaru::seam::build_seam_object().expect("build the kernel seam object")
}

/// Links a container: transpiled guest objects, the seam, kisal, and an
/// image.
///
/// `--fatal-warnings` is the recipe rather than decoration, exactly as it is
/// for the interop link: the seam's whole value is that a disagreement about
/// `kisal_syscall`'s shape stops the build.
pub fn link_container(workspace: &WorkingDirectory, guests: &[PathBuf], label: &str) -> PathBuf {
    let image = baker::object::empty().expect("build an empty image object");
    link_container_with_image(workspace, guests, &image, label)
}

/// The same, carrying a filesystem.
///
/// Every container links an image, even an empty one. kisal references the
/// image symbols unconditionally, so a link that omitted one fails with an
/// undefined symbol rather than producing a module that silently has no
/// files — which is the difference between a container and a module.
pub fn link_container_with_image(
    workspace: &WorkingDirectory,
    guests: &[PathBuf],
    image: &[u8],
    label: &str,
) -> PathBuf {
    link_container_for_program(workspace, guests, image, label, None)
}

/// The same, for a container carrying a program that has to be *loaded*.
///
/// `program_top` is the highest address the linked guest's segments reach.
/// Passing it moves the module's own data above that, which is the only
/// thing that keeps the image blob, the allocator and the arenas out of the
/// addresses the program must occupy — see [`baker::layout`].
pub fn link_container_for_program(
    workspace: &WorkingDirectory,
    guests: &[PathBuf],
    image: &[u8],
    label: &str,
    program_top: Option<u64>,
) -> PathBuf {
    let seam = workspace.write(&format!("seam.{label}.wasm.o"), seam_object());
    let image = workspace.write(&format!("image.{label}.wasm.o"), image);
    let mut objects: Vec<PathBuf> = guests.to_vec();
    objects.push(seam);
    objects.push(image);
    objects.push(kisal_staticlib());
    objects.push(x87_staticlib());
    let linked = workspace.path().join(format!("container.{label}.wasm"));
    // `--export-table` is not decoration: the host embedder needs the
    // indirect function table to install a continuation's slot, which is how
    // a thread is started from outside the module.
    let reservation = baker::layout::link_arguments(program_top);
    let mut arguments = vec![
        "--fatal-warnings",
        "--export=cabi_realloc",
        // How the host starts a container. Nothing inside the module calls
        // it, so without this the linker would drop it as unreachable.
        "--export=kisal_boot",
        // The exec map and the seam's yield slot, so a test can ask the
        // module where an address resolves to without running it.
        "--export=x86_slot_of",
        "--export=x86_yield_slot",
        "--export-table",
        "--growable-table",
    ];
    arguments.extend(reservation.iter().map(String::as_str));
    link_wasm(&objects, &linked, &arguments);
    linked
}

/// The mount table an M1 container boots with: a console and a kernel log,
/// both sinks a test can read back.
pub fn m1_mounts() -> runner::store::MountTable {
    let mut mounts = runner::store::MountTable::new();
    mounts.mount(&[b"iso", b"console"], Box::new(runner::store::Sink::new()));
    mounts.mount(&[b"iso", b"log"], Box::new(runner::store::Sink::new()));
    mounts
}

/// The same, plus the boot entropy a container needs to have any.
///
/// Separate from `m1_mounts` on purpose: a container with no `/iso/random`
/// mount has no randomness, and that is the capability model rather than an
/// oversight. A default that quietly supplied entropy would make the
/// distinction untestable.
/// Runs a corpus program both ways and requires the two to agree: the same
/// ELF executed by Linux, and the same ELF baked into a container.
///
/// Returns the exit status the two agreed on, so a caller can say what it
/// expects that to be — which is worth saying, because a program that failed
/// to reach its own work would agree with a native run that failed the same
/// way.
///
/// Comparing the bytes as well as the status is the point: a program can add
/// up to the same total having gone somewhere else entirely.
pub fn program_agrees_with_native(
    workspace: &WorkingDirectory,
    source: &str,
    label: &str,
) -> i32 {
    let elf = link_corpus_executable(workspace, source, "_start", "-O1");
    let bytes = std::fs::read(&elf).expect("read the program");
    let object = zaqaru::reader::ObjectFile::parse(&bytes).expect("parse the program");

    let native = std::process::Command::new(&elf)
        .env_clear()
        .output()
        .expect("run the program natively");
    let native_status = native.status.code().expect("a native exit status");

    let top = object
        .segments
        .iter()
        .map(|segment| segment.address + segment.memory_size)
        .max()
        .expect("a linked program has segments");
    let translation = zaqaru::transpile::Transpiler::new(&object)
        .translate()
        .expect("translate the program");
    let guest = workspace.write(&format!("{label}.wasm.o"), &translation.module);

    let root = workspace.path().join(format!("image-{label}"));
    std::fs::create_dir_all(&root).expect("create the image tree");
    let mut placed = bytes.clone();
    baker::program::apply(&mut placed, &translation.patches).expect("apply the patches");
    let program = root.join("init");
    std::fs::write(&program, &placed).expect("place the program");
    // Executable, because the kernel asks: `execve` on a file with no execute
    // bit is `EACCES`, here as on Linux. A baked image carries the mode from
    // the tree it was baked from, where the compiler set it; a test that
    // writes the patched bytes itself has to say so on its own behalf.
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("make the program executable");
    }
    let image = baker::object::emit(&baker::bake_directory(&root).expect("bake"))
        .expect("emit the image object");

    let module = link_container_for_program(
        workspace,
        std::slice::from_ref(&guest),
        &image,
        label,
        Some(top),
    );
    let mut container = runner::Container::instantiate(
        &std::fs::read(&module).expect("read the container"),
        mounts_seeded(&[0x33; 32]),
    )
    .expect("instantiate the container");

    let status = container.boot().unwrap_or_else(|error| {
        let log = container
            .mounts()
            .read(&[b"iso".to_vec(), b"log".to_vec(), b"error".to_vec()])
            .ok()
            .flatten()
            .unwrap_or_default();
        panic!(
            "the container did not finish: {error:?}\nkernel log: {}",
            String::from_utf8_lossy(&log)
        )
    });

    let written = container
        .mounts()
        .read(&[b"iso".to_vec(), b"console".to_vec(), b"stdout".to_vec()])
        .expect("the console mount failed")
        .unwrap_or_default();
    assert_eq!(
        written, native.stdout,
        "[{label}] the transpiled program wrote something else than the native one"
    );
    assert_eq!(
        i64::from(status),
        i64::from(native_status),
        "[{label}] the transpiled program exited differently"
    );
    native_status
}

/// Builds a C source as an ordinary dynamic program, bakes it with
/// everything it loads, runs it, and requires the container and Linux to
/// agree on both what it wrote and how it ended.
///
/// The default on every distribution this decade is a dynamic
/// position-independent executable, which is the input the container tier
/// exists for — and the one that has a libc, which anything using `setjmp`
/// or `dlopen` needs.
pub fn dynamic_program_agrees_with_native(
    workspace: &WorkingDirectory,
    source: &Path,
    label: &str,
) -> String {
    let elf = workspace.path().join(format!("{label}.elf"));
    run_tool(
        "gcc",
        &[
            "-O2",
            &source.to_string_lossy(),
            "-o",
            &elf.to_string_lossy(),
        ],
    );

    let native = std::process::Command::new(&elf)
        .env_clear()
        .output()
        .expect("run the program natively");
    let native_status = native.status.code().expect("a native exit status");

    let mut tree = baker::tree::Tree::new();
    tree.resolve_or_create(b"/tmp").expect("a /tmp in the image");
    let baked = baker::bake::container(&elf, Path::new("/"), tree)
        .expect("bake the program and what it loads");
    let guest = workspace.write(&format!("{label}.wasm.o"), &baked.module);
    let module = link_container_for_program(
        workspace,
        std::slice::from_ref(&guest),
        &baked.image,
        label,
        Some(baked.top),
    );
    let mut mounts = mounts_seeded(&[0x77; 32]);
    mounts.mount(&[b"iso", b"shutdown"], Box::new(runner::store::Sink::new()));
    mounts.mount(&[b"iso", b"time"], Box::new(runner::store::Clock::new()));
    let mut container = runner::Container::instantiate(
        &std::fs::read(&module).expect("read the container"),
        mounts,
    )
    .expect("instantiate");

    let finished = container.boot();
    let written = container
        .mounts()
        .read(&[b"iso".to_vec(), b"console".to_vec(), b"stdout".to_vec()])
        .expect("the console mount failed")
        .unwrap_or_default();
    let written = String::from_utf8(written).expect("the guest wrote something unreadable");
    let status = finished.unwrap_or_else(|error| {
        let log = container
            .mounts()
            .read(&[b"iso".to_vec(), b"log".to_vec(), b"error".to_vec()])
            .ok()
            .flatten()
            .unwrap_or_default();
        panic!(
            "[{label}] the container did not finish: {error:?}\nkernel log: {}\n\
             stdout so far:\n{written}",
            String::from_utf8_lossy(&log)
        )
    });

    assert_eq!(
        written,
        String::from_utf8_lossy(&native.stdout),
        "[{label}] the container and the native run wrote different things"
    );
    assert_eq!(
        i64::from(status),
        i64::from(native_status),
        "[{label}] the container and the native run ended differently"
    );
    written
}

pub fn mounts_seeded(seed: &[u8]) -> runner::store::MountTable {
    let mut mounts = m1_mounts();
    mounts.mount(&[b"iso", b"random"], Box::new(runner::store::Sink::new()));
    mounts
        .write(
            &[
                b"iso".to_vec(),
                b"random".to_vec(),
                b"bytes".to_vec(),
                b"32".to_vec(),
            ],
            seed,
        )
        .expect("seed the container");
    mounts
}
