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
| `processes` | every process: pid, parent, `exe` (the path it was started from) and `comm` (the basename of what `execve` was asked to run — a script's name, not its interpreter's — or the name it set with `prctl`), state, exit status if unreaped, and each thread's tid and state, with what it is parked on. The stall report, structured |
| `processes/{pid}/threads/{tid}/registers` | the sixteen general registers, `rip`, the segment base, the flags as materialised, and `flags_stale` (see below) |
| `processes/{pid}/maps` | the VMA tree, the fields `/proc/self/maps` shows |
| `processes/{pid}/descriptors` | each fd: what backs it, the path it was opened by (as `/proc/self/fd` shows it), offset, flags |
| `processes/{pid}/files/{path}` | the process's view of the filesystem at a path — its own view, since a fork copies the writable layer and the working directory: a directory's entries with kind and size, a regular file's contents (text, or hex when not text; capped at 16 KiB), a symlink's target; with the working directory. The final component is not followed |
| `net` | every socket in the container: family, kind, state (idle, bound, listening with its backlog and queue, connected with both addresses and the bytes queued each way), the host's edge number when the host terminates it, and which processes hold a descriptor on it |
| `processes/{pid}/threads/{tid}/disassembly` | up to forty instructions from `rip`: address, bytes, text |
| `processes/{pid}/memory/{address}/{length}` | up to 4096 bytes of the process's memory, hex, as far as they are readable |
| `processes/{pid}/mapped` | the pages the process can reach, as `[start, end)` runs — the permission bits themselves |
| `layout` | where the guest block is in linear memory |
| `cache` | the running process's block cache: blocks decoded, live, flushes |
| `caches` | what the kernel keeps that a snapshot need not: decompressed files, decoded blocks, pooled page buffers |
| `caches/decompressed` | every decompressed file's buffer, address and length; **writable**: `refill` |
| `caches/blocks` | every process's block cache; **writable**: `flush` |
| `caches/pool` | pooled page buffers; **writable**: `zero` |
| `meta/...` | the spec's meta lens: which paths are readable |

Three paths take a write, and they are the exception to the rule below
rather than a breach of it: each changes kernel memory in a way the guest
cannot observe — a file decompressed again into the buffer it already had,
a block cache the kernel will decode again on demand, a pooled buffer
that is filled before it is handed out — and they exist so that a
snapshot can leave out what they put back. See "Starting from a file".

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

**What the file leaves out.** As first written, the file was 75 MB
compressed for Django, and a breakdown by address showed 223 of its 281
MB of pages were the kernel's heap, not the guest's processes. The heap
held three things a booted container does not need carried: 110 MB of
decoded blocks — the interpreter's cache of 66,000 blocks, which it
decodes again on demand; 31 MB of decompressed files, which are a
function of the image; and stale copies in the page pool. So before the
file is written the tool asks the kernel, through the three writable
paths above, to flush every block cache, zero the pool, and name the
decompressed buffers, and it leaves those buffers out; whoever continues
from the file writes `refill` first. Flushing frees memory without
clearing it, so the module's allocator (`crates/guest/src/alloc.rs`)
zeroes what it frees while the kernel's `SCRUB_FREED` flag is set, which
the flush sets around itself. The tool also leaves out the guest pages no
process can reach, taken from `processes/{pid}/mapped` — the permission
bits the interpreter checks — rather than from the memory map, which is
for people: the first attempt read the map, the map did not name the
`brk` heap, every process's malloc arena was zeroed, and gunicorn quietly
restarted its worker. The map now names `[heap]`, as Linux does. Last,
a tenth of the pages are repeats of another — a forked process's page
that never diverged, in place for one process and displaced for the
other — and the file stores each once. Together: 75 MB to 39 MB
compressed, the refill costing nothing measurable at load. What remains
is mostly the processes themselves and their displaced copies.

**Brotli.** Browsers inflate gzip natively (`DecompressionStream`) and
brotli not at all, so the page carries a decoder: `crates/brotli`, the
`brotli-decompressor` crate as a 237 KB wasm module with three exports,
built by the fixture and demo scripts into `web/brotli.wasm`. A brotli
file is wrapped with its inflated length so the output is placed at its
exact size (`web/brotli.js`). Measured on the Django file, 223 MB raw:

| compression | file | to compress | to inflate |
| --- | --- | --- | --- |
| gzip 6 | 39.4 MB | 2.5 s | 0.4 s, native |
| brotli 9 | 32.7 MB | 25 s | 0.6–0.7 s, Chrome and Firefox |
| brotli 10 | 29.3 MB | 125 s | the same |
| brotli 11 | 28.6 MB | 228 s | 0.8 s |

The demo uses quality 10: a quarter off the download for a third of a
second more inflating, and two minutes of CI rather than four for the
last 0.7 MB. Without `--brotli` the tool writes gzip, which needs no
decoder.

For Django (`web/demo.sh`): the boot is 3.29 G instructions, 28–32 s
under Node — the same rate as wasmtime; 71,600 pages of the 887 MB memory
differ from a fresh instance, of which 63,400 are kept, 29 MB as brotli,
beside a 74 MB module. Headless Chrome loads both, refills the 31 MB of
files, and stands the container up listening on port 80 in 1.0 s; a
`GET /` through the edge is answered by nginx, gunicorn and Django in
0.12–0.3 s and 3.8 M instructions; a seek into the middle of the request
restores and re-executes in 0.3 s.

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

A store browser with a time axis. A timeline over retired instructions;
play, pause, step forward by one instruction, step back as a seek, and
previous and next event. Live, "play" advances the frontier four million
instructions a tick and renders each one, so the panels move while the
container runs.

**The panels come from the store.** The worker reads the container's
`meta` lens once and, at every instant, reads every path it declares
that the instant can fill: `{pid}` and `{tid}` from the process and
thread in view — the running ones unless a process card was clicked —
`{path}` from where the page is browsing, the memory under `rsp` once the
registers say where it is. Each panel is titled by the concrete path it
read, offers the raw JSON value beside its rendering, and a path the
kernel adds appears on the page with nothing changed here. A path bar
reads any path at the instant in view. The few patterns with a renderer
of their own — processes, files, descriptors, net, registers,
disassembly, memory, maps — stand in the open; the rest show as the
values they are, and the machine-level ones fold under "the machine".

**The timeline is the container's traffic with the host.** The tape is
a StructFS store, and the page shows it: every read and write under
`/iso` — the clock, entropy, the console, the bytes of a request crossing
the edge — as a row beside the syscalls, with what crossed. An exchange
is placed at the syscall it was made in: the kernel stamps a syscall on
its timeline as the call returns, so the host counts stamps and an
exchange made during syscall *N* carries *N*. That is exact for
everything a syscall asks the host, and one syscall early for the few
reads the kernel makes between turns (its poll of the network's events
and of the shutdown switch), which land on the next stamp. The rows of a
syscall's exchanges come before the syscall's own row, since the row is
what the call returned. An idle container polls the host at every wake
— the clock, the network's events, the shutdown switch — so by default
the polls that found nothing are left out and repeats of one path at one
stamp fold into a row that says how many; "everything, unfolded" shows
each as it was. The list holds rows only for a window of three hundred
around the present and is rebuilt when the present leaves it.

**Files at an instant.** The `files` panel browses the process's
filesystem — the image with the process's writable layer over it — as
it stood at the instant: a directory lists, a file shows its contents, a
descriptor's path is a link into it.

**Pin and diff.** Values are structured, so what changed between two
instants is a diff of two JSON values. Pinning an instant makes every
panel show, under its rendering, the keys added, gone and changed since:
arrays whose elements carry an identity — a pid, an fd, a socket id, a
name — are matched by it, so a descriptor closed in the middle shows as
that one descriptor gone.

**Names, lanes, and the opening.** A process is named by its `comm`, which the `processes` path carries — and keeps one
colour for the run: on its card, on every timeline row it made, and on
a lane strip under the slider that shows which process ran when, from
the pid on each syscall's stamp. Live, the page opens on the edge box
alone, since nothing has happened yet; when the first answer arrives it
marks the request's span on the strip, from sent to answered, and stands
the machine on the instant the request came in — the `accept` that took
the connection — with everything shown. Each answer offers that instant
and its own as links.

**The row you click chooses what the panels look at.** A path in a
syscall's arguments is a link into the files panel; a descriptor number
is a link to what it names — its file, browsed to, or its socket in the
net panel, lit; bytes crossing a connection light the socket the host
terminates. Clicking the row itself opens the first of those it has.

The edge, where a request is typed and its answer shown, and the console
are the host's side of the boundary rather than paths of the container's
store, and the page says so in their titles. Registers, disassembly, the
stack and the memory map fold under "the machine"; the statistics, the
caches and the layout, which exist for the snapshot tool, fold under
"kernel internals".

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
  through a request and a seek into it when they are present, in headless
  Chrome and in headless Firefox;
- the demo published, built from source on every push by
  `.github/workflows/pages.yml` and checked to answer before it is deployed,
  at <https://mgyenik.github.io/zaqaru/>;
- the page as a store browser: panels generated from the `meta` lens and
  titled by their paths, the raw value beside each rendering, a path bar,
  a process chosen by clicking it; the `files` and `net` paths and the
  descriptors' paths in the kernel; the exchanges with the host on the
  timeline beside the syscalls, stamped by the host with the syscall each
  was made in; and pin-and-diff between two instants.

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
