//! The host: a wasmtime embedder that supplies the two `ll-store` imports
//! and nothing else.
//!
//! "Nothing else" is the design, not a stage of it. Every syscall a container
//! makes is handled inside the module by the kernel; what varies is the backend
//! behind the object it touches, and the only backends that reach out here
//! are time, entropy, external I/O edges and instance creation. So this crate
//! stays small on purpose: two imports, a mount table, and a way to start the
//! guest.

pub mod net;
pub mod store;

use store::MountTable;
use wasmtime::error::Context;
use wasmtime::{Result, bail};

/// The module and field names the linked module imports the store under —
/// the core-wasm lowering of featherweight's WIT.
const HOST_MODULE: &str = "env";
const READ_IMPORT: &str = "ll_read";
const WRITE_IMPORT: &str = "ll_write";

/// The export the host reaches back through to place returned bytes in guest
/// memory, exactly as the component canonical ABI does.
const REALLOC_EXPORT: &str = "cabi_realloc";
const MEMORY_EXPORT: &str = "memory";

/// The one thing the host calls to run a container.
///
/// Everything the run consists of is inside the module: the program's bytes
/// come from the image the module carries, its segments are copied within
/// linear memory, and the only things that cross this boundary are what the
/// mount table exposes. The return value is the exit status, which also
/// arrives through the store at `/iso/shutdown/complete` — here so that a
/// host that mounts nothing can still tell how the run ended.
pub const RUN_EXPORT: &str = "zaqaru_run";

/// What one call of the run export did. The guest packs the kind into the
/// low byte and, when finished, the exit status into the byte above.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turn {
    /// The count asked for was reached; call again to go on.
    Running,
    /// Nothing was runnable and the container waited on the host once.
    Idle,
    /// The container has exited with this status.
    Finished(i32),
}

impl Turn {
    fn decode(word: i32) -> Result<Self> {
        match word & 0xff {
            0 => Ok(Turn::Running),
            1 => Ok(Turn::Idle),
            2 => Ok(Turn::Finished((word >> 8) & 0xff)),
            other => bail!("the guest answered an unknown turn kind {other}"),
        }
    }
}

/// The canonical ABI's alignment for a `list`'s `(pointer, length)` pair.
const LIST_ALIGNMENT: u32 = 4;

pub struct Container {
    store: wasmtime::Store<MountTable>,
    instance: wasmtime::Instance,
}

/// How to set the engine up.
#[derive(Clone, Copy, Default, Debug)]
pub struct Options {
    /// Have wasmtime write a perf map. JIT frames are anonymous machine
    /// code, so a host profiler shows the engine as a single unresolved
    /// address unless wasmtime writes the map that names them. Off by
    /// default: it writes a file per process into /tmp and exists only to
    /// be profiled.
    pub perfmap: bool,
}

impl Container {
    /// Instantiates a linked container module against a mount table.
    pub fn instantiate(module_bytes: &[u8], mounts: MountTable) -> Result<Self> {
        Self::instantiate_with(module_bytes, mounts, Options::default())
    }

    /// The same, with the engine set up as `options` says.
    pub fn instantiate_with(module_bytes: &[u8], mounts: MountTable, options: Options) -> Result<Self> {
        let mut configuration = wasmtime::Config::new();
        if options.perfmap {
            configuration.profiler(wasmtime::ProfilingStrategy::PerfMap);
        }
        let engine = wasmtime::Engine::new(&configuration)
            .context("configuring the wasm engine")?;
        let module = wasmtime::Module::new(&engine, module_bytes)
            .context("wasmtime rejected the container module")?;
        let mut store = wasmtime::Store::new(&engine, mounts);
        let mut linker = wasmtime::Linker::new(&engine);
        define_store_imports(&mut linker)?;
        if !store.data().resolves(&kernel_log_path()) {
            bail!(
                "nothing is mounted at /iso/log, so the kernel would have no \
                 way to report an unimplemented syscall — and the report is \
                 the whole of the loud-error policy"
            );
        }
        let instance = linker
            .instantiate(&mut store, &module)
            .context("instantiating the container module")?;
        Ok(Self { store, instance })
    }

    /// Calls any export by name and type.
    pub fn call<Parameters, Results>(
        &mut self,
        name: &str,
        parameters: Parameters,
    ) -> Result<Results>
    where
        Parameters: wasmtime::WasmParams,
        Results: wasmtime::WasmResults,
    {
        let function = self
            .instance
            .get_typed_func::<Parameters, Results>(&mut self.store, name)
            .with_context(|| format!("the module has no usable export `{name}`"))?;
        function
            .call(&mut self.store, parameters)
            .with_context(|| format!("calling `{name}`"))
    }

    /// Runs the container to completion, and reports the exit status.
    pub fn boot(&mut self) -> Result<i32> {
        loop {
            if let Turn::Finished(status) = self.step(-1)? {
                return Ok(status);
            }
        }
    }

    /// Runs the container until it has retired `until` instructions in
    /// total, finished, or idled once. The first call boots it. A negative
    /// `until` runs to completion.
    ///
    /// Between calls nothing is on the guest's stack: linear memory is the
    /// whole machine.
    pub fn step(&mut self, until: i64) -> Result<Turn> {
        Turn::decode(self.call::<i64, i32>(RUN_EXPORT, until)?)
    }

    /// Reads a path of the container's own store — the isotope Server
    /// Protocol from the outside — and answers the Response as the kernel
    /// wrote it, JSON of the shape `{"result":"ok","value":...}` or
    /// `{"result":"error","error":{...}}`.
    ///
    /// The kernel serves at the instant the machine is stopped at: the
    /// Request is queued, the container is asked to run to the count it has
    /// already reached, which runs nothing and serves, and the Response is
    /// collected. Needs the mount table to have called
    /// [`MountTable::serve`].
    pub fn ask(&mut self, path: &str) -> Result<String> {
        let Some(id) = self.mounts().ask(path) else {
            bail!("nothing is mounted at /iso/server, so the container's store cannot be asked");
        };
        self.step(0)?;
        match self.mounts().answer(id) {
            Some(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
            None => bail!("the container did not answer the read of `{path}`"),
        }
    }

    /// The mount table. A host that cannot see what the guest wrote cannot
    /// diagnose anything, so this is the diagnostic face of the boundary as
    /// well as the way a test reads back a run.
    pub fn mounts(&mut self) -> &mut MountTable {
        self.store.data_mut()
    }
}

/// Where the kernel sends its own complaints. Stated here as well as in
/// `kernel::paths` because the host has to guarantee it is reachable, and a
/// guarantee that reads a different constant is not one.
fn kernel_log_path() -> Vec<Vec<u8>> {
    vec![b"iso".to_vec(), b"log".to_vec(), b"error".to_vec()]
}

fn define_store_imports(linker: &mut wasmtime::Linker<MountTable>) -> Result<()> {
    linker.func_wrap(
        HOST_MODULE,
        READ_IMPORT,
        |mut caller: wasmtime::Caller<'_, MountTable>,
         path: u32,
         path_length: u32,
         result: u32|
         -> Result<()> {
            let memory = memory_of(&mut caller)?;
            let segments = read_path(&caller, &memory, path, path_length)?;
            let answer = caller.data_mut().read(&segments);

            // The return area's layout is the canonical ABI's, written here
            // and read by `kernel::abi::ReadResult`.
            match answer {
                Ok(Some(bytes)) => {
                    let placed = place(&mut caller, &memory, &bytes)?;
                    write_u32(&mut caller, &memory, result, 0, 0)?;
                    // `some` is case 1. The canonical ABI numbers a variant's
                    // cases in declaration order and `option` is
                    // `none | some`, so writing 0 here would be our own
                    // convention wearing the ABI's name — invisible while
                    // both sides agree, and an inversion of every read the
                    // day this is wrapped as a real component.
                    write_u32(&mut caller, &memory, result, 4, 1)?;
                    write_u32(&mut caller, &memory, result, 8, placed)?;
                    write_u32(&mut caller, &memory, result, 12, bytes.len() as u32)?;
                }
                Ok(None) => {
                    write_u32(&mut caller, &memory, result, 0, 0)?;
                    write_u32(&mut caller, &memory, result, 4, 0)?;
                    write_u32(&mut caller, &memory, result, 8, 0)?;
                    write_u32(&mut caller, &memory, result, 12, 0)?;
                }
                Err(message) => {
                    let placed = place(&mut caller, &memory, message.as_bytes())?;
                    write_u32(&mut caller, &memory, result, 0, 1)?;
                    write_u32(&mut caller, &memory, result, 4, placed)?;
                    write_u32(&mut caller, &memory, result, 8, message.len() as u32)?;
                    write_u32(&mut caller, &memory, result, 12, 0)?;
                }
            }
            Ok(())
        },
    )?;

    linker.func_wrap(
        HOST_MODULE,
        WRITE_IMPORT,
        |mut caller: wasmtime::Caller<'_, MountTable>,
         path: u32,
         path_length: u32,
         data: u32,
         data_length: u32,
         result: u32|
         -> Result<()> {
            let memory = memory_of(&mut caller)?;
            let segments = read_path(&caller, &memory, path, path_length)?;
            let payload = read_bytes(&caller, &memory, data, data_length)?;
            let answer = caller.data_mut().write(&segments, &payload);

            match answer {
                Ok(result_path) => {
                    let placed = place_path(&mut caller, &memory, &result_path)?;
                    write_u32(&mut caller, &memory, result, 0, 0)?;
                    write_u32(&mut caller, &memory, result, 4, placed)?;
                    write_u32(&mut caller, &memory, result, 8, result_path.len() as u32)?;
                }
                Err(message) => {
                    let placed = place(&mut caller, &memory, message.as_bytes())?;
                    write_u32(&mut caller, &memory, result, 0, 1)?;
                    write_u32(&mut caller, &memory, result, 4, placed)?;
                    write_u32(&mut caller, &memory, result, 8, message.len() as u32)?;
                }
            }
            Ok(())
        },
    )?;
    Ok(())
}

fn memory_of(caller: &mut wasmtime::Caller<'_, MountTable>) -> Result<wasmtime::Memory> {
    match caller.get_export(MEMORY_EXPORT) {
        Some(wasmtime::Extern::Memory(memory)) => Ok(memory),
        _ => bail!("the container module does not export its linear memory as `{MEMORY_EXPORT}`"),
    }
}

/// A `list<list<u8>>`, read out of guest memory: a count of eight-byte
/// `(pointer, length)` records.
fn read_path(
    caller: &wasmtime::Caller<'_, MountTable>,
    memory: &wasmtime::Memory,
    pointer: u32,
    count: u32,
) -> Result<Vec<Vec<u8>>> {
    // Not `saturating_mul`: saturating here would read one record short of
    // what was asked for and `chunks_exact` would quietly drop the last
    // segment, resolving the call against a different mount than the guest
    // named. An impossible count is an error, not a shorter path.
    let Some(bytes) = count.checked_mul(8) else {
        bail!("a path of {count} segments does not fit the address space");
    };
    let records = read_bytes(caller, memory, pointer, bytes)?;
    let mut segments = Vec::with_capacity(count as usize);
    for record in records.chunks_exact(8) {
        let segment_pointer = u32::from_le_bytes(record[0..4].try_into().expect("four bytes"));
        let segment_length = u32::from_le_bytes(record[4..8].try_into().expect("four bytes"));
        segments.push(read_bytes(caller, memory, segment_pointer, segment_length)?);
    }
    Ok(segments)
}

fn read_bytes(
    caller: &wasmtime::Caller<'_, MountTable>,
    memory: &wasmtime::Memory,
    pointer: u32,
    length: u32,
) -> Result<Vec<u8>> {
    // The length is guest-controlled, so the range is checked against the
    // guest's own memory before anything is allocated on the host's behalf.
    // Allocating first and discovering the overrun afterwards lets a guest
    // name four gigabytes and have the host reserve it.
    let size = memory.data_size(caller);
    let end = (pointer as u64) + (length as u64);
    if end > size as u64 {
        bail!(
            "the guest asked for {length} bytes at {pointer:#x}, past the end \
             of its {size}-byte memory"
        );
    }
    let mut bytes = vec![0u8; length as usize];
    memory
        .read(caller, pointer as usize, &mut bytes)
        .with_context(|| format!("reading {length} bytes at {pointer:#x} of guest memory"))?;
    Ok(bytes)
}

/// Writes one word of a return area, at an offset from the guest's pointer.
///
/// The offset is applied with `checked_add` because the pointer is the
/// guest's: `result + 12` on a pointer near the top of the address space
/// wraps, and the write then lands on low guest memory reporting success.
/// The payload reads are bounds-checked; the return area was not.
fn write_u32(
    caller: &mut wasmtime::Caller<'_, MountTable>,
    memory: &wasmtime::Memory,
    address: u32,
    offset: u32,
    value: u32,
) -> Result<()> {
    let at = address
        .checked_add(offset)
        .ok_or_else(|| wasmtime::Error::msg("the return area wraps the address space"))?;
    let size = memory.data_size(&caller);
    if (at as u64) + 4 > size as u64 {
        bail!("the return area at {at:#x} is past the end of the guest's memory");
    }
    memory
        .write(caller, at as usize, &value.to_le_bytes())
        .with_context(|| format!("writing the return area at {at:#x}"))?;
    Ok(())
}

/// Puts bytes in guest memory through the guest's own allocator, which is
/// the only way anything travels host-to-guest — so nothing is ever freed
/// across the boundary.
fn place(
    caller: &mut wasmtime::Caller<'_, MountTable>,
    memory: &wasmtime::Memory,
    bytes: &[u8],
) -> Result<u32> {
    if bytes.is_empty() {
        return Ok(LIST_ALIGNMENT);
    }
    let realloc = match caller.get_export(REALLOC_EXPORT) {
        Some(wasmtime::Extern::Func(function)) => function
            .typed::<(u32, u32, u32, u32), u32>(&caller)
            .context("`cabi_realloc` has the wrong type")?,
        _ => bail!(
            "the container module does not export `{REALLOC_EXPORT}`, so the \
             host has no way to return bytes to it"
        ),
    };
    let address = realloc
        .call(&mut *caller, (0, 0, LIST_ALIGNMENT, bytes.len() as u32))
        .context("the guest's `cabi_realloc` failed")?;
    memory
        .write(caller, address as usize, bytes)
        .context("writing returned bytes into guest memory")?;
    Ok(address)
}

/// The same, for a result path: an element array of `(pointer, length)`
/// records, each pointing at bytes placed the same way.
fn place_path(
    caller: &mut wasmtime::Caller<'_, MountTable>,
    memory: &wasmtime::Memory,
    path: &[Vec<u8>],
) -> Result<u32> {
    let mut records = Vec::with_capacity(path.len() * 8);
    for segment in path {
        let placed = place(caller, memory, segment)?;
        records.extend_from_slice(&placed.to_le_bytes());
        records.extend_from_slice(&(segment.len() as u32).to_le_bytes());
    }
    place(caller, memory, &records)
}
