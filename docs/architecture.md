# Architecture

zaqaru runs an unmodified x86-64 container image inside one WebAssembly
module. This document says how, from the artifact down.

## The artifact

A container module is two things linked by `wasm-ld`:

- **the image object**: the container's filesystem — every layer of the
  OCI image flattened, whiteouts applied — as two data segments, a packed
  index and a blob of file contents, plus the command line, environment
  and working directory the image says to start with;
- **the guest archive** (`libguest.a`): the kernel, the interpreter and
  the FPU compiled for `wasm32-unknown-unknown`. It is the same in every
  container and is embedded in the `zaqaru` binary.

The module imports two functions and exports two. It imports `ll_read` and
`ll_write` — the store, the whole host interface. It exports `zaqaru_boot`,
which the host calls once and which returns the container's exit status,
and `cabi_realloc`, which the host calls to place the bytes a read
returns. Nothing about the module says what programs are inside it.

The bake never reads the program. There is no analysis, no translation, no
guess about where code is: the program's bytes go into the image as data,
and the interpreter decodes them at the program counter when they run.
That is what makes the bake work for any image — a JIT, a shell script
that writes a program and runs it, a wheel installed by pip at run time,
code that rewrites itself.

## The machine (`crates/cpu`)

An x86-64 interpreter, and the loop that drives it.

**Thread state is a struct.** A control block holds the sixteen general
registers, `rip`, the segment base, the sixteen XMM registers and MXCSR,
the x87 state, a lazy-flags record, and a count of retired instructions.
A context switch is a pointer swap and a snapshot is a copy.

**The guest address space is linear memory, identity-mapped.** A guest
virtual address is an offset into the module's memory. `/proc/self/maps`
is honest, a program reading its own headers sees the truth, and the
interpreter's loads and stores are wasm loads and stores at the guest's
address. The space is 4 GiB, which is wasm32's.

**A page table of bitmaps** — readable, writable, executable, and
"holds code somebody decoded" — one bit per 4 KiB page each. Every load,
store and fetch tests it, so a wild pointer is a real `SIGSEGV` a handler
can catch, `PROT_NONE` guard pages fault, and a fetch from a
non-executable page is refused. A store to a page that holds decoded code
queues that page for invalidation, and the run loop drains the queue
before it fetches the next block, so self-modifying code is correct by
construction: no stale bytes are ever executed.

**Blocks.** The engine decodes a run of instructions from the program
counter to the first control transfer, or past a conditional branch to
the next one, capped in length, and caches it against the pages it came
from. Each instruction is pre-decoded into a compact form with resolved
operands, and the run loop interprets the block's instructions in order.
Anything the compact form does not model falls back to a general
executor over the full decoded instruction, so an unhandled operand shape
can only make the engine slower, never wrong. An instruction the engine
does not implement stops the container and names itself.

**Flags are lazy.** Arithmetic records what it did — the rule, the width,
the operands and the result — and a consumer derives exactly the bit it
asks for. A `jne` is one comparison. Only `pushf` and the signal frame
materialize all six. An arithmetic instruction whose flags are overwritten
before anything can read them does not record them at all.

**The bytecode accelerator.** Each decoded block is also transpiled into
a flat register-machine bytecode and run by a dense switch loop. A
direct branch within the block is a jump inside the stream rather than a
return to the run loop; a compare and the branch that consumes it fuse
into one op; an indirect transfer whose target is already transpiled is
resolved through the block cache and stays inside the bytecode. Anything
the transpiler does not model becomes a *defer*, which hands one
instruction back to the interpreter and resumes. The accelerator covers
93–96% of the instructions a real workload retires and is on by default;
`--no-bytecode` runs the plain interpreter, which is also what the
hardware oracle compares against.

**One counter, two jobs.** Every retired instruction increments a counter.
The scheduler's quantum is denominated in it, so preemption is a pure
function of execution; and `rdtsc` answers from it, so time as the guest
measures it is deterministic too.

**The FPU** (`crates/x87`) is a soft implementation of the x87 register
stack in 80-bit extended precision, bit-exact against the hardware for
arithmetic, conversion and comparison, and within measured ulps for the
transcendentals, where Intel and AMD themselves differ.

## The kernel (`crates/kernel`)

A Linux-personality kernel that runs inside the module. A `syscall`
instruction stops the run loop; the loop reads six registers from the
control block, calls a Rust function, and writes one back. There is no
mode switch and no host crossing.

**The kernel sees the world through one trait.** Everything it cannot
answer from its own memory — time, entropy, the console, external I/O, a
shutdown request — is a path under `/iso` read or written through a
`Store`. The module's real store is the two host imports; the native tests
supply in-memory doubles. What a container may touch is exactly what the
host mounts.

**Filesystem.** The root is an overlay: the baked image below, a memory
store above, copy-up on the first write. `/proc`, `/sys` and `/dev` are
synthetic mounts over kernel state. Path resolution — `..`, symlinks,
`openat` relativity, trailing slashes, mount crossings — happens in one
loop in the kernel; a store answers `lookup(dir, name)` and nothing else.
The image index is packed so that a `stat` is an index multiply and some
field copies, and a miss costs what a hit costs, because CPython's import
machinery probes far more paths than it opens.

**Processes.** The kernel is replicated per process — the descriptor
table, the VMA tree, the signal dispositions are copied with everything
else, so inheritance across `fork` is correct by construction. All
processes share the one linear memory: a page-ownership table records
whose bytes are at each page, and a switch moves only the pages two
processes both map. The shared things — pipe and socket rings, the port
table — live in one arena the whole process tree reaches. `fork`,
`vfork`, `clone`, `execve`, `wait4`, `SIGCHLD` and reparenting to the
first process are all built; a child resumes by being interpreted from a
`rip` already past its `syscall`.

**Threads** are control blocks inside a process. The scheduler is
round-robin on a quantum of retired instructions, across processes and
threads alike, so two compute-bound threads make progress against each
other and the interleaving is the same on every run. Futexes, `clone`
with `CLONE_THREAD`, `clear_child_tid` and the rest are ordinary kernel
state.

**Signals** are delivered between blocks: the frame Linux would build is
written to the guest stack through the same address space every other
store uses, `rip` is set to the handler, and interpretation continues;
`rt_sigreturn` reads the frame back. A fault in the interpreter is a
signal.

**Sockets.** A connection with both ends in the container is two rings
crossed in the shared arena — `connect` to a loopback port is a queue
push onto the listener — and never touches the host. A connection with
one end outside is the *edge*: the host accepts real TCP and hands the
kernel a stream of events and bytes through `/iso/net`, and from the
first byte in it is indistinguishable from a loopback connection. There
is no TCP state machine and no packet anywhere. A port is published with
`-p`; an unpublished listener is loopback-only, and the guest cannot
tell.

**Waiting.** When nothing in the container is runnable and something is
parked on the edge or a deadline, the kernel makes the one store read
that is allowed to take time, with the earliest deadline as its bound.
An idle server costs nothing.

## The host boundary (`crates/guest`, `crates/host`)

Two imports, path-routed:

```
ll_read (path: list<list<u8>>)              -> result<option<list<u8>>, string>
ll_write(path: list<list<u8>>, data: list<u8>) -> result<list<list<u8>>, string>
```

The shapes are the canonical component ABI's, so the module can be
wrapped as a component mechanically. The host places the bytes a read
returns by calling the guest's `cabi_realloc` into a bump arena that
lives for one syscall.

The paths a container uses: `/iso/console/{stdout,stderr}`,
`/iso/log/{error,debug,statistics}`, `/iso/random/bytes/32`,
`/iso/time/{realtime,monotonic}`, `/iso/shutdown/{requested,complete}`,
`/iso/config/{trace,bytecode}`, and `/iso/net/...` for the edge. A
capability is a mount: no `/iso/net`, no network; no `/iso/random`, no
entropy — and the kernel says so by name rather than inventing any.

**Determinism and replay.** Every nondeterministic input enters as a
store answer, and every store answer arrives at a point that is a pure
function of execution and earlier answers. Recording the answers records
the run; replaying them from a tape, with nothing mounted, reproduces it
byte for byte — the schedule, the clock the guest saw, a served HTTP
session.

## The bake (`crates/image`, `crates/bake`)

The packager reads a `docker save` tarball — the manifest, each layer's
tar with whiteouts and opaque directories applied in order, and the
config's entrypoint, command, environment and working directory — or a
plain directory, into a tree; the tree becomes the index and the blob.
File contents are zstd-compressed and decompressed on first open inside
the module.

The bake writes the image as a relocatable wasm object with two data
segments and two hidden symbols, `__image_index` and `__image_blob`, and
links it with the guest archive. The one layout decision is
`--global-base`: the module's own data is placed 64 MiB up, so a program
that states its own addresses — a `-no-pie` executable at `0x400000` —
has room below it. Position-independent programs and shared objects are
placed by the kernel wherever there is room, as a real kernel does.

## Testing

Three levels, cheapest first.

- **The kernel natively, in milliseconds.** The kernel is ordinary Rust
  over the `Store` and `Machine` traits, so every syscall row is tested
  against in-memory doubles and a bare register file. The filesystem,
  memory and dispatch suites live here.
- **The interpreter against hardware.** The lockstep oracle runs a probe
  program under `ptrace` on the real CPU and in the interpreter, one
  instruction at a time, and compares every register, the flags the
  architecture defines, the x87 stack and the vector file after each. The
  bytecode has its own tests against the interpreter, block by block.
- **Containers, end to end.** Programs compiled with the host's `gcc`,
  static and dynamic, baked and run under wasmtime through the same code
  path the tool uses; and the same programs run natively, with the output
  compared. The demo's nginx and Django stack is traced both ways and the
  difference accounted for.

What the kernel does not do, and what a program would have to do to
notice, is in [fidelity.md](fidelity.md). What it costs is in
[performance.md](performance.md).
