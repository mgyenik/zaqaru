//! The host: a wasmtime embedder that supplies the two `ll-store` imports
//! and nothing else.
//!
//! "Nothing else" is the design, not a stage of it. Every syscall a container
//! makes is handled inside the module by kisal; what varies is the backend
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
/// the core-wasm lowering of featherweight's WIT, settled in M0.
const HOST_MODULE: &str = "env";
const READ_IMPORT: &str = "ll_read";
const WRITE_IMPORT: &str = "ll_write";

/// The export the host reaches back through to place returned bytes in guest
/// memory, exactly as the component canonical ABI does.
const REALLOC_EXPORT: &str = "cabi_realloc";
const MEMORY_EXPORT: &str = "memory";
const TABLE_EXPORT: &str = "__indirect_function_table";

/// The one thing the host calls to run a container.
///
/// Everything the run consists of is inside the module: the program's bytes
/// come from the image the module carries, its segments are copied within
/// linear memory, and the only things that cross this boundary are what the
/// mount table exposes. The return value is the exit status, which also
/// arrives through the store at `/iso/shutdown/complete` — here so that a
/// host that mounts nothing can still tell how the run ended.
pub const BOOT_EXPORT: &str = "kisal_boot";

/// The same, for a container that carries an interpreter instead of a
/// translation.
///
/// Two names rather than one because they are two entry points into two
/// different things — `kisal_boot` enters a program the bake turned into
/// wasm functions, `targum_boot` enters a loop over a program the bake
/// never read — and one runner serves both because everything *around* the
/// entry is identical: the same image, the same kernel, the same two
/// imports, the same mount table.
pub const INTERPRETED_BOOT_EXPORT: &str = "targum_boot";

/// The canonical ABI's alignment for a `list`'s `(pointer, length)` pair.
const LIST_ALIGNMENT: u32 = 4;

pub struct Container {
    store: wasmtime::Store<MountTable>,
    instance: wasmtime::Instance,
}

impl Container {
    /// Instantiates a linked container module against a mount table.
    pub fn instantiate(module_bytes: &[u8], mounts: MountTable) -> Result<Self> {
        // JIT frames are anonymous machine code, so a host profiler shows
        // the engine as a single unresolved address unless wasmtime writes
        // the map that names them. Off by default: it writes a file per
        // process into /tmp and exists only to be profiled.
        let mut configuration = wasmtime::Config::new();
        if std::env::var_os("ZAQARU_PERFMAP").is_some() {
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

    /// Calls a guest function through its host-entry wrapper, in the uniform
    /// zero-information shape: every argument register of both files in,
    /// `rax` and `xmm0` out.
    pub fn call_guest(&mut self, name: &str, integers: [i64; 6]) -> Result<i64> {
        let function = self
            .instance
            .get_typed_func::<(
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
            ), (i64, f64)>(&mut self.store, name)
            .with_context(|| format!("the module has no usable export `{name}`"))?;
        let (result, _) = function
            .call(
                &mut self.store,
                (
                    integers[0],
                    integers[1],
                    integers[2],
                    integers[3],
                    integers[4],
                    integers[5],
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                ),
            )
            .with_context(|| format!("the guest trapped inside `{name}`"))?;
        Ok(result)
    }

    /// Calls any export by name and type. The uniform wrapper has a shape of
    /// its own ([`Container::call_guest`]); everything else the seam
    /// exports — the register-file helpers, the scheduler's catch — is an
    /// ordinary typed function reached through here.
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
    ///
    /// One call, because a container is one program: the kernel loads it,
    /// enters it, and catches the throw its `exit_group` becomes.
    pub fn boot(&mut self) -> Result<i32> {
        // Whichever the module has. A container built either way is a
        // container, and a runner that only knew one name would make the
        // choice of execution path visible in the command line.
        let entry = match self
            .instance
            .get_func(&mut self.store, INTERPRETED_BOOT_EXPORT)
            .is_some()
        {
            true => INTERPRETED_BOOT_EXPORT,
            false => BOOT_EXPORT,
        };
        self.call::<(), i32>(entry, ())
    }

    /// The mount table. A host that cannot see what the guest wrote cannot
    /// diagnose anything, so this is the diagnostic face of the boundary as
    /// well as the way a test reads back a run.
    pub fn mounts(&mut self) -> &mut MountTable {
        self.store.data_mut()
    }

    /// How much linear memory the instance has right now.
    ///
    /// It grows, so a caller that wants to scan all of it has to ask rather
    /// than assume — a fixed guess reads out of bounds on a small module and
    /// misses the tail of a large one.
    pub fn memory_size(&mut self) -> Result<usize> {
        let memory = self.memory()?;
        Ok(memory.data_size(&self.store))
    }

    pub fn read_memory(&mut self, address: u32, length: usize) -> Result<Vec<u8>> {
        let memory = self.memory()?;
        let mut bytes = vec![0u8; length];
        memory.read(&mut self.store, address as usize, &mut bytes)?;
        Ok(bytes)
    }

    pub fn write_memory(&mut self, address: u32, bytes: &[u8]) -> Result<()> {
        let memory = self.memory()?;
        memory.write(&mut self.store, address as usize, bytes)?;
        Ok(())
    }

    /// Guest memory from the canonical ABI's transfer arena.
    ///
    /// **Valid only until the guest's next syscall.** The arena exists to
    /// carry an `ll-store` result into the guest and is reset at the top of
    /// every syscall, which is what stops the boundary leaking — so anything
    /// placed here is overwritten by the next call's return value, silently,
    /// because the bytes are simply different afterwards.
    ///
    /// The name says so because the trap is invisible otherwise: a prefix
    /// placed here once came back as `"iso"`, the first segment of the
    /// console mount's result path, and every path the guest built from it
    /// was wrong from that point on.
    ///
    /// Nothing in the runtime needs a longer-lived placement. Everything the
    /// host supplies — `/iso` reads, and argv and envp at process start —
    /// is consumed by the syscall that asked for it, and the initial stack
    /// is built by the kernel in its own memory. If that ever stops being
    /// true it wants a design, not a second allocator bolted on beside this
    /// one.
    pub fn allocate_transfer(&mut self, length: u32, align: u32) -> Result<u32> {
        self.call::<(u32, u32, u32, u32), u32>(REALLOC_EXPORT, (0, 0, align, length))
    }

    /// Puts a host function of the guest convention into a fresh slot of the
    /// indirect function table, and hands back the slot.
    ///
    /// This is how a continuation is handed to `x86_run_thread` from outside
    /// the module: a thread is started by naming a slot, and the host needs a
    /// way to name one. It also does the only thing that can prove the
    /// scheduler's catch reports a *return* rather than a yield, before any
    /// real guest continuation exists.
    pub fn install_continuation(&mut self) -> Result<i32> {
        let table = match self.instance.get_export(&mut self.store, TABLE_EXPORT) {
            Some(wasmtime::Extern::Table(table)) => table,
            _ => bail!("the container module does not export `{TABLE_EXPORT}`"),
        };
        let guest_type = wasmtime::FuncType::new(self.store.engine(), [], []);
        let function = wasmtime::Func::new(&mut self.store, guest_type, |_, _, _| Ok(()));
        let slot = table.grow(&mut self.store, 1, wasmtime::Ref::Func(Some(function)))?;
        Ok(slot as i32)
    }

    fn memory(&mut self) -> Result<wasmtime::Memory> {
        match self.instance.get_export(&mut self.store, MEMORY_EXPORT) {
            Some(wasmtime::Extern::Memory(memory)) => Ok(memory),
            _ => {
                bail!("the container module does not export its linear memory as `{MEMORY_EXPORT}`")
            }
        }
    }
}

/// Where the kernel sends its own complaints. Stated here as well as in
/// `kisal::paths` because the runner has to guarantee it is reachable, and a
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
            // and read by `kisal::abi::ReadResult`.
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
