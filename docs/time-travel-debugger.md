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
| `processes/{pid}/threads/{tid}/disassembly` | up to forty instructions from `rip`: address, bytes, text |
| `processes/{pid}/memory/{address}/{length}` | up to 4096 bytes of the process's memory, hex, as far as they are readable |
| `cache` | blocks decoded, blocks live, bytes of bytecode, flushes |
| `meta/...` | the spec's meta lens: which paths are readable |

The disassembly is iced's fast formatter, behind the `disassembly` cargo
feature on the cpu, kernel and guest crates; the engine itself never
prints an instruction. Measured at 54 KB on a module that is 2.6 MB before
its image, so the guest the tool embeds carries it, and the manifest
declares the path only when it is compiled in. The two memory paths are
served for the running process only: every process maps the same range and
only the running one's bytes are in place (`resident`), so a dormant
process's memory is refused with `unavailable` rather than misread.

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
`processes`; the harness enqueues the Request, calls the run export with
the current count, and returns the Response. That is the same client
code that drives the wasmtime host.

**Live.** Without a tape the container runs against the page's own clock
and entropy, with a syscall trace on, and every answer the host gives is
recorded. A checkpoint's mount table is then a *replay* over that same
recording from the cursor at the checkpoint, so a seek behind the
frontier restores a checkpoint and re-executes against the answers the
live run was actually given — which the recording goes on accumulating
as the live run continues. "Play" advances the frontier, checkpointing on
the way; the slider views anything behind it.

**The edge.** `Edge` in `web/zaqaru.js` speaks the `/iso/net` protocol the
wasmtime host speaks to real TCP — `listen`, `events`, `conn/{j}/rx/{room}`,
`conn/{j}/tx`, `conn/{j}/ctl` — to requests made by the page. A request is
one connection whose whole request is already in; it resolves with
everything the guest sent when the guest ends the connection, and the
client's side stays open until then, as curl's would. It has to: nginx
treats a client that half-closes while its request is being proxied as one
that went away, and answers nothing — which is how the first Django
request through the edge came back empty. A request to a published port the guest has not listened on
yet waits, as a client retrying would, and connects the moment the
listener registers. The one wait the kernel makes, `wait/{ms}`, cannot
block in a browser and answers what there is; the worker sleeps briefly on
an idle turn instead of spinning. So the page is `curl` to a server inside
the container: `fixture.sh` bakes one that answers "pong", and the
browser test sends it "ping".

### Starting from a file

A booted container, written to a file and continued from: `web/snapshot.js`
is the format and `web/preboot.mjs` the tool. The tool runs a module live
under Node, with the demo's ports published on an edge nobody sends to,
until it is *quiet* — for three seconds of wall time it retires fewer than
two million instructions, which is a server whose processes are all parked
on timeouts. It then writes the pages of memory that differ from a fresh
instance's (found with the checkpoint diff against the fresh memory), the
stack pointer, the retired count, and every store's state as JSON
(`MountTable.save`): the console so far, the clock's monotonic reading so
the guest's clock continues rather than runs backwards, the entropy, the
config, and which ports the guest listens on. The boot's own syscall log
is dropped, since nothing can seek into the time before the file. The file
is gzip as a whole; the browser inflates it with `DecompressionStream`.

Continuing is `MountTable.load` of that state — with this run's edge, and
recording — and `Container.continueFrom`, which is the restore with two
differences from a checkpoint's: the pages are relative to a fresh
instance, so what the file lacks is left as instantiated rather than put
to zero; and the table is used as built rather than copied, because a copy
of a recording table is a replay over its recording, which for a run that
is only beginning is a tape that has run out at the first clock read. (It
was: the container's first idle check found no clock and called itself
deadlocked.) History begins at the file's instant: the first checkpoint
is taken there, and the slider does not go below it.

For Django (`web/demo.sh`): the boot is 3.29 G instructions, 32 s under
Node — the same rate as wasmtime; 71,900 pages of the 887 MB memory
differ from a fresh instance, 281 MB, 75 MB compressed, beside a 74 MB
module. Headless Chrome loads both and stands the container up listening
on port 80 in 1.0 s; a `GET /` through the edge is answered by nginx,
gunicorn and Django in 0.3 s and 3.9 M instructions; a seek into the
middle of the request restores and re-executes in 0.3 s.

Check: the recorded Django run replays in the browser to the same console
output and retired count as under wasmtime, and `statistics` read at the
end agrees.

### Checkpoints and seeking

While playing, take a snapshot every `K` retired instructions. Seeking
to `n` restores the last checkpoint at or before `n` and steps to `n`.
Deterministic re-execution means `K` can be large: at 100 MIPS a gap of
200 M instructions is a two-second seek worst case.

Memory is the constraint, and it bites before Django does: a container's
memory is hundreds of megabytes even for a small program, because the
kernel reserves its guest block at boot and wasm memory never shrinks. The
fixture's is 600 MB. Nearly all of it is zero, and nearly all of what is
not is still between two checkpoints — the image blob never changes, and
page ownership keeps a dormant process's pages in place.

So a checkpoint is a map from 4 KiB page index to the page's bytes, holding
only pages that are not zero (`web/checkpoints.js`). The pages are
immutable and shared between checkpoints. A checkpoint after the first
records the pages that changed, found by comparing memory in place
against the previous map as 32-bit words — about 50 ms for 300 MB under
V8, so no helper is needed — and a full map is kept every sixteenth
checkpoint at the cost of the map alone, since its pages are the ones
already held. Restoring never builds a dense image: a fresh wasm memory
is zero, so the pages are written straight into it. The last reconstructed
map is kept, so seeking within one stretch of the run pays it once.

Measured on the fixture: ten checkpoints hold 16 MB where full copies
would hold 6 GB. Checked under Node: every checkpoint of a run
reconstructs byte for byte against a full snapshot taken at the same
point, and a container restored from one runs to the same end.

### The page

A timeline over retired instructions; play, pause, step forward by one
instruction, step back as a seek, and previous and next syscall. The
syscall log as a clickable time axis, holding rows only for a window of
three hundred around the present — a run is a million syscalls long before
it is interesting — and rebuilt when the present leaves the window. Panels
for the running process and thread, each a read of the container's store:
processes, registers, the disassembly from `rip`, 256 bytes of stack under
`rsp` as quadwords, the memory map, descriptors, the console, and the
edge, where a request is typed and its answer shown with the instants it
was sent and answered, the latter a link that seeks there. Live, "play"
advances the frontier four million instructions a tick and renders each
one, so the panels move while the container runs.

## Two things the page must say

**Stale flags.** An arithmetic instruction whose six status flags are
overwritten before anything reads them does not record them (dead-flag
elimination in the interpreter; liveness in the transpiler). At an
instant inside such a span, the flags word is the last *recorded*
writer's. The `registers` value carries `flags_stale`, and the page shows
the flags greyed with a note rather than as truth. `docs/fidelity.md`
records the same divergence for signal frames.

**Speed.** The interpreter runs at 30–100 MIPS; V8 turned out to run the
module at wasmtime's rate. A Django boot is 3.29 G instructions, half a
minute either way, so the demo starts from the file `preboot.mjs` writes
rather than boot live — the same booted snapshot the performance notes
want for start-up. Checkpoints of an 887 MB memory cost about a hundred
milliseconds each, so a live run of that size checkpoints every twenty
million instructions (the `every` parameter; the page picks that default
when given a snapshot) and a seek re-executes at most that far.

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
- the browser harness, the worker with delta checkpoints and seeking, and
  the page with the timeline, the syscall log, and the panels;
- live mode, with the recording that makes its past seekable, and the
  edge through which the page is the client of a server in the container;
- the disassembly and memory paths, and the panels that read them;
- the snapshot file, the tool that writes one from a quiet container, and
  the page continuing from it — and the demo: `web/demo.sh` makes the
  Django module and its snapshot, and the browser test drives the page
  through a request and a seek into it when they are present.

Not built:

- **A real wait in live mode.** The kernel's `wait/{ms}` read cannot block
  in a browser Worker without JSPI or `Atomics.wait` on shared memory; the
  worker sleeps between idle turns instead, which costs nothing here but
  is not the design's blocking read.

## Order of work and size

| Part | Size | Where |
| --- | --- | --- |
| The container as a store | medium | kernel, guest, host |
| Re-entrant guest and exact stop | small | guest, kernel, cpu, host |
| Snapshot and restore | medium; the acceptance test | host, bake tests |
| Browser harness | medium | new: `web/` |
| Checkpoints and seeking | medium | `web/` |
| The page | large | `web/` |
| Disassembly and memory paths | small | cpu, kernel, `web/` |
| The snapshot file and the demo | medium | `web/` |

Everything through snapshot and restore is testable under wasmtime with
the existing test infrastructure. The browser work starts only once the
snapshot test passes.
