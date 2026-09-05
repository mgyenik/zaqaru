# A time-travel debugger for a container

Status: built, 2026-09-05, through the page in `web/`; see "Where it
stands" at the end for what is not. Each part says what it is, what it
changes, and how it is checked. Code comments should name the mechanisms
described here, not this document's headings.

## What it is

A page that loads a container module and a tape of one of its runs, plays
the run, and lets you drag a slider backwards and forwards through it.
At any instant you see the process tree, each thread's registers, the
memory map, the descriptor table, and the syscall stream up to that
point; click a syscall and the machine stands at it. The demo moment is
a request that failed, the failing syscall found in the trace, and the
machine rewound to the instruction that made it.

It is possible because a run is a pure function of its tape, and because
the whole machine is linear memory. Neither is true of Docker, and
neither is true of an ordinary emulator.

## The interface it uses, and why

A container module is an isotope Block. The spec (`structfs/isotope/spec`)
gives a Block two faces, and this design uses both as written rather than
adding a third:

- **Inside, the Block is a StructFS client.** Everything it needs from the
  host it reads or writes under `/iso/`, through the two imports the
  module already has. This is how the kernel gets time, entropy, the
  console and the network today, and it is the boundary the tape
  records.
- **Outside, the Block is a StructFS store.** Anyone who wants something
  *from* the Block reads or writes its store. The runtime turns that into
  a Request the Block reads from `/iso/server/requests` (or, without
  blocking, from `/iso/server/requests/pending`), and the Block answers by
  writing a Response to the Request's `respond_to` path. The spec calls
  this the Server Protocol, and it is the spec's answer to "how does the
  outside ask a Block a question".

So the debugger's questions — what processes exist, what is thread 3's
`rip`, what is mapped at this address — are reads of the container's
public store, served by the kernel. No new import, no new export, and no
path invented under `/iso/`. The same client code drives the wasmtime
host and a browser, because both are just StructFS clients of the same
store.

What the spec leaves to the runtime is how a Block is *driven*: its
non-goals are scheduling and the execution engine, and its lifecycle
chapter lists preemption and checkpointing as open questions. Stopping
the machine at an exact instruction and snapshotting it are therefore
runtime-level mechanisms, below the Block interface, and they are the
only additions this design makes at the wasm level.

## What is already true

- **Every run is replayable.** Time, entropy, the network and the shutdown
  switch all arrive as store answers; `zaqaru run --record` keeps every
  answer, including refusals, and `--replay` reproduces the run byte for
  byte with nothing mounted. The tape checks the path of every question
  against the recording, so a divergence is found at its first step.
- **The schedule is a function of retired instructions.** The quantum is
  100,000 retired instructions per thread, a slice is sixteen quanta per
  process, and `rdtsc` answers from the same counter.
- **No guest state lives on the wasm stack.** A thread is a control block
  in linear memory; the kernel's state is a graph of `Rc` and `RefCell` in
  the same memory. Between two instructions, memory is the machine.
- **The module has one mutable global**, the shadow stack pointer, at its
  base whenever no export is executing. The module exports `memory`,
  `zaqaru_boot` and `cabi_realloc`, and nothing else.
- **The kernel already polls the host at a deterministic point.** Once a
  slice it reads `/iso/net/events`, refreshes its timebase and checks for
  shutdown. That is where it will read `/iso/server/requests/pending`.
- **The kernel can already describe itself**: `/proc/self/maps` is a
  rendering of the VMA tree, and the stall report walks every process and
  thread and says what each is parked on.

## What was not true when this was written

Each of these is now built; the parts below say how.

- The boot export ran the container to completion inside one call, so
  there was never a moment when the host held the wasm stack empty and
  could copy memory.
- The bytecode accelerator checks its budget only at back-edges, so a
  quantum can overshoot by a block, and "instruction N" was reachable only
  if N was a stopping point of the recorded run.
- The container served no store. It was a StructFS client only.
- Nothing snapshotted or restored.
- There was no browser harness.

## The parts, in dependency order

### The container as a store

The kernel gains a server: once a slice, at the same point it pumps the
edge, it reads `/iso/server/requests/pending` and answers every Request
in the batch by writing a Response to its `respond_to` path. It also
answers at every return to the host (below), so a stopped machine can be
asked about the instant it stopped at. Values are JSON; the module's
`manifest()` export declares `application/json` and the paths below, and
the kernel writes the same declaration to `/iso/self/interface` at boot,
both as the spec asks.

The paths the store serves, all read-only:

| path | value |
| --- | --- |
| `statistics` | retired, accelerated, decoded, current pid; the line `/iso/log/statistics` carries at exit, as a value, at any time |
| `processes` | every process: pid, parent, state, exit status if unreaped, and each thread's tid and state, with what it is parked on. The stall report, structured |
| `processes/{pid}/threads/{tid}/registers` | the sixteen general registers, `rip`, the segment base, the flags as materialised, and `flags_stale` (see below) |
| `processes/{pid}/maps` | the VMA tree, the fields `/proc/self/maps` shows |
| `processes/{pid}/descriptors` | each fd: what backs it, offset, flags |
| `cache` | blocks decoded, blocks live, bytes of bytecode, flushes |
| `meta/...` | the spec's meta lens: which paths are readable |

Optionally `processes/{pid}/threads/{tid}/disassembly`, the block at
`rip`, which needs iced's formatter compiled in behind a feature since the
engine deliberately leaves it out of the shipped module.

**The one rule that keeps replay honest.** Serving a Request reads kernel
state and writes a Response; it never changes anything the guest can
observe. That rule is what lets the tape exclude `/iso/server/*`: the
answers to `requests/pending` are not inputs to the run, so the recording
store does not record them and the replay store does not check them, and
a debugger can ask questions during a replay without the run diverging.
The rule is checked, not assumed: the acceptance test below asks
questions at every step of one replay and none of another and requires
the same output, the same retired count, and the same description of the
machine at the end. (Not the same memory: a question's answer is built on
the kernel's heap, whose layout the guest cannot see.)

The syscall trace stays where it is (`/iso/config/trace` on,
`/iso/log/debug` out), and each line gains the retired count at which the
call was made, so the trace is a time axis the page can seek by.

Changes: a `server` module in `crates/kernel` with the renderers, the poll
in `System`'s slice boundary, the `manifest` export and the interface
write in `crates/guest`, the tape exclusion in `crates/host/src/store.rs`.

Check: native kernel tests read each path through an in-memory server
store and assert against the same programs the dispatch tests already
run; the container test reads `processes` and `statistics` from a running
module under wasmtime.

### A re-entrant guest, and an exact stop

The one addition at the wasm level. The boot export becomes a step:

```
zaqaru_step(until: i64) -> i32
```

The first call boots. Each call runs the scheduler until the global
retired count reaches `until`, or the container finishes, or nothing is
runnable; it answers `Running`, `Idle`, or `Finished` with the status in
the high bits. A negative `until` means to completion, which is what
`zaqaru run` uses. A call with `until` equal to the current count runs no
instruction, serves pending Requests, and returns: that is how the
debugger asks about the instant the machine is stopped at. The `System`
moves into a static so that between calls the wasm stack is empty.

The last block before `until` runs **interpreted** rather than as a
bytecode trace, so the stop lands on the instruction exactly. That is a
flag on the engine's run loop, the same path a deferred instruction
already takes, and it changes nothing about the state at `until`. What
it must never do is continue past `until` in that mode, because the
accelerated run would have preempted at a different point and the
recorded schedule would diverge; seeking always resumes from a
checkpoint, so it never does. A recorded schedule depends on whether the
bytecode was on, so the tape gains a header saying so and replay matches
it.

If featherweight's `run` export turns out to have a fixed nullary
signature, the module exports that too, as `zaqaru_step(-1)`, and the
stepping export stays the runtime extension it is.

Check: the container test runs a module to completion in one call and in
steps of one quantum, and the output and retired count are identical.
For random `n` within a recorded run, stopping at `n` and continuing to
the end matches a straight replay.

### Snapshot and restore

A snapshot is linear memory (minus the image's data segment, which is
constant and comes back with instantiation), the shadow stack pointer,
and the host's own state: the tape cursor, the console and log buffers,
the config mounts, and the server store's queue, which is empty between
steps. Restore is a fresh instance of the compiled module sized to the
snapshot, memory copied back, the global set. A fresh instance rather
than the old one because wasm memory only grows, and an older snapshot in
a grown memory would report a larger limit to the kernel than the run had.

Nothing in the module knows a snapshot happened. This is entirely the
host's, built first under wasmtime as `Container::snapshot` and
`Container::restore` and tested in `crates/bake/tests`.

Check, and this is the acceptance test for the whole design: run to `N`,
snapshot, continue to `M`, and record memory and output. Restore and
continue to `M` again: memory, output and retired count are
byte-identical. Then restore once more and continue to `M` with the
debugger reading `processes` at every step: output, retired count and the
machine's own description of itself are identical, though memory is not
compared on this leg — answering a question allocates on the kernel's
heap, and the heap's layout is the one thing in memory a question
changes. That is the exact statement of "serving a Request changes
nothing the guest can observe".

### The browser harness

The two imports in JavaScript, in a Worker: a mount table over `/iso`
with the console, the log, the config, the random seed, the clock, the
shutdown switch, the **replay store** answering from the tape and
checking paths as the host's does, and the **server store**: the runtime
half of the Server Protocol, queueing Requests the page makes and
correlating Responses. The canonical-ABI lowering is in
`crates/guest/src/wire.rs`. Under replay nothing blocks, because the one
waiting read is on the tape, so the first harness needs neither JSPI nor
`Atomics.wait`.

The page is then a StructFS client of the container. It reads
`processes`; the harness enqueues the Request, calls `zaqaru_step` with
the current count, and returns the Response. That is the same client
code that will drive the wasmtime host.

Check: the recorded Django run replays in the browser to the same console
output and retired count as under wasmtime, and `statistics` read at the
end agrees.

### Checkpoints and seeking

While playing, take a snapshot every `K` retired instructions. Seeking
to `n` restores the last checkpoint at or before `n` and steps to `n`.
Deterministic re-execution means `K` can be large: at 100 MIPS a gap of
200 M instructions is a two-second seek worst case.

Memory is the constraint. A Django module's memory is several hundred
megabytes, so the first checkpoint is full and every later one is a delta
against the previous, computed by comparing pages in a small wasm helper.
Page ownership means most of memory is still while a server idles, and
the image segment never changes. A full snapshot every few dozen deltas
bounds the restore chain.

### The page

A timeline over retired instructions with checkpoint marks; play, pause,
step forward by one instruction, step back as a seek. Lanes per process
from `statistics` sampled at each step. The syscall log as a clickable
time axis. Panels for the selected process and thread: registers, maps,
descriptors, and the block at `rip`, each a read of the container's
store.

## Two things the page must say

**Stale flags.** An arithmetic instruction whose six status flags are
overwritten before anything reads them does not record them (dead-flag
elimination in the interpreter; liveness in the transpiler). At an
instant inside such a span, the flags word is the last *recorded*
writer's. The `registers` value carries `flags_stale`, and the page shows
the flags greyed with a note rather than as truth. `docs/fidelity.md`
records the same divergence for signal frames.

**Speed.** The interpreter runs at 30–100 MIPS and a browser engine may be
slower than wasmtime. A Django boot is 3.29 G instructions. The demo
should start from a checkpoint taken after boot rather than boot live,
which is the same snapshot the performance plan wants for start-up.

## Out of scope, and noted

The kernel reads its configuration at `/iso/config/{trace,bytecode}`.
The spec puts configuration at `/config/`, wired by the assembly, and
reserves `/iso/` for runtime services. That is a pre-existing divergence
this design does not fix and does not extend; it is recorded here so it
is decided deliberately.

## Where it stands

Built, and checked by the tests named above plus `web/test.mjs` (the
harness under Node, against the wasmtime host's own run) and
`web/browser-test.mjs` (the page in headless Chrome):

- the container as a store, served once a slice and at every return;
- `zaqaru_run(until)` and `zaqaru_stop_at(target)`, with the flags'
  staleness recorded at an exact stop;
- snapshot and restore, on the wasmtime host and in the browser, with the
  byte-identical acceptance test;
- the tape's engine-mode header;
- the browser harness, the worker with checkpoints and seeking, and the
  page with the timeline, the syscall log, and the panels.

Not built:

- **Delta checkpoints.** Every checkpoint is a full copy of memory. The
  fixture's module is a few megabytes; a Django module's is hundreds, and
  a run of any length would exhaust a tab. Page-diff deltas against the
  previous checkpoint are the next piece of work.
- **The disassembly panel**, which needs iced's formatter compiled into
  the guest behind a feature.
- **A live mode** in the browser: a JavaScript clock and entropy, and an
  edge that turns the page's own `fetch` into `/iso/net` events. The
  harness has the stores; nothing wires a live run yet.
- **A pre-booted Django snapshot** to start the demo from.

## Order of work and size

| Part | Size | Where |
| --- | --- | --- |
| The container as a store | medium | kernel, guest, host |
| Re-entrant guest and exact stop | small | guest, kernel, cpu, host |
| Snapshot and restore | medium; the acceptance test | host, bake tests |
| Browser harness | medium | new: `web/` |
| Checkpoints and seeking | medium | `web/` |
| The page | large | `web/` |

Everything through snapshot and restore is testable under wasmtime with
the existing test infrastructure. The browser work starts only once the
snapshot test passes.
