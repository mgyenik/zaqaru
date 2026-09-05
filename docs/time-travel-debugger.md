# A time-travel debugger for a container

Status: design, 2026-09-05. Nothing below is built yet; each part says what
it is, what it changes, and how it is checked. Code comments should name
the mechanisms described here, not this document's numbering.

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
neither is true of an ordinary emulator, which is why this has not been
built before.

## What is already true

- **Every run is replayable.** Time, entropy, the network and the shutdown
  switch all arrive as store answers; `zaqaru run --record` keeps every
  answer, including refusals, and `--replay` reproduces the run byte for
  byte with nothing mounted. The tape checks the path of every question
  against the recording, so a divergence is found at its first step.
- **The schedule is a function of retired instructions.** The quantum is
  100,000 retired instructions per thread, a slice is sixteen quanta per
  process, and `rdtsc` answers from the same counter. Two runs of the same
  tape interleave identically, across threads and processes.
- **No guest state lives on the wasm stack.** A thread is a control block
  in linear memory; a context switch is a pointer swap. The kernel's state
  is a graph of `Rc` and `RefCell` in the same memory. Between two
  instructions, memory is the machine.
- **The module has one mutable global**, the shadow stack pointer, and it
  is at its base whenever no export is executing.
- **The kernel can already describe itself**: `/proc/self/maps` is a
  rendering of the VMA tree, and the stall report walks every process and
  thread and says what each is parked on.

## What is not yet true

- The boot export runs the container to completion inside one call, so
  there is never a moment when the host holds the wasm stack empty and can
  copy memory.
- The bytecode accelerator checks its budget only at back-edges, so a
  quantum can overshoot by a block. The interpreter stops at any count; the
  accelerator stops at the next back-edge after it. "Instruction N" is
  reachable today only if N is a stopping point of the recorded run.
- Nothing snapshots or restores.
- There is no browser harness: the two imports exist only in the wasmtime
  host.

## The parts, in dependency order

### A re-entrant guest

The boot export splits in two:

```
zaqaru_boot() -> i32            load the program; 0, or a status if it could not
zaqaru_step() -> i32            one scheduler turn; answers a Turn
```

`Turn` is `Running`, `Idle` (nothing runnable; the host may wait on the
tape or the world and call again), or `Finished` with the exit status in
the high bits. The `System` moves into a static so that between calls the
wasm stack is empty. The host-side run loop that `zaqaru run` uses today
becomes a loop over `zaqaru_step`, and the idle path's blocking store
read stays where it is, inside the kernel, because a step that idles
makes it and returns.

Changes: `crates/guest/src/boot.rs`, a `System::step` beside `System::run`
in `crates/kernel/src/system.rs` that performs exactly one iteration of
the existing loop, and the host's `Container::boot` becoming the loop.

Check: the container test runs the module through `zaqaru_step` and
through the old single call, and the console output and the retired count
are identical.

### Two counters the host can read

```
zaqaru_retired() -> i64         instructions retired by every process, living or gone
zaqaru_current() -> i32         the pid whose turn it is, or -1
```

The first is the debugger's time axis. It is the number the statistics
already report at exit, exposed as a query.

### An exact stop

```
zaqaru_step_to(n: i64) -> i32   run until zaqaru_retired() == n, or a Turn ends the run first
```

The system gives the current thread a budget of `n - retired` instead of
a full quantum, and the engine runs that budget **interpreted**, skipping
the block's bytecode trace, so the stop lands on the instruction exactly.
That is a one-flag change in `Engine::run` — the same path a deferred
instruction already takes — and it changes nothing about the machine's
state at `n`: interpreting and running the trace compute the same
instructions to the same results. What it must never do is *continue*
past `n` in that mode, because the accelerated run would have preempted
at a different point and the recorded schedule would diverge. Seeking
always resumes from a checkpoint, so it never does.

A recorded schedule depends on whether the bytecode was on. The tape
gains a header recording it, and the debugger replays in the same mode.

Check: for random `n` within a recorded run, restore the initial
snapshot, `zaqaru_step_to(n)`, then run to the end, and compare console
output and the final retired count with a straight replay. Then stop at
`n`, snapshot, restore, and continue to the end: same comparison.

### Snapshot and restore

A snapshot is:

1. **linear memory**, minus the image's data segment, which is constant
   and is restored by instantiation;
2. **the shadow stack pointer**, which will always be its initial value
   between calls and is recorded anyway so that the check below can
   notice if that stops being true;
3. **host state**: the tape cursor, the console and log buffers, and the
   config mounts. Under replay there is no network and no clock to
   reconstruct.

Restore is a fresh instance of the same compiled module, sized to the
snapshot, a copy of memory back into it, and the global set. A fresh
instance rather than writing into the old one because wasm memory only
grows: an older snapshot restored into a memory that has since grown
would report a larger limit to the kernel than the run had at that
instant.

Built first under wasmtime, in `crates/host`, as `Container::snapshot()`
and `Container::restore(&snapshot)`, and tested in `crates/bake/tests`
before any browser exists.

Check: this is the acceptance test for the whole design. Run to `N`,
snapshot, continue to `M`, and record memory and output. Then restore the
snapshot and continue to `M`: memory, output, and the retired count are
byte-identical. Do it at several `N`, including one that falls inside a
`fork` and one during a signal delivery.

### Introspection

```
zaqaru_describe(kind: i32, buffer: i32, capacity: i32) -> i32   bytes written, or the size needed
```

`kind` selects a rendering, written as JSON so the page needs no parser
of its own:

- `processes`: every process, its pid, parent, state, and each thread's
  tid, state, and what it is parked on. The stall report, structured.
- `registers` for a pid and tid: the sixteen general registers, `rip`,
  the segment base, the flags as materialised, and whether the flags at
  this instant may be stale (see below).
- `maps` for a pid: the VMA tree, as `/proc/self/maps` renders it.
- `descriptors` for a pid: each fd, what backs it, its offset and flags.
- `cache`: blocks decoded, blocks live, bytes of bytecode.

The syscall trace already exists (`/iso/config/trace` on, lines to
`/iso/log/debug`). Each line gains the retired count at which the call
was made, so the trace is a time axis the page can seek by. Optionally a
`disassembly` kind for the block at `rip`, which needs iced's formatter
compiled in behind a feature, since the engine deliberately leaves it out
of the shipped module.

### The browser harness

The two imports in JavaScript, in a Worker:

- a mount table over `/iso` with the console, the log, the config, the
  random seed, the clock, and the shutdown switch, each a small object;
- a **replay store** that answers from the tape and checks the path of
  each question, exactly as the host's does;
- the canonical-ABI lowering: paths as `(pointer, length)` pairs, results
  through the return area, bytes placed by calling the module's
  `cabi_realloc`. The layouts are in `crates/guest/src/wire.rs`.

Under replay nothing ever blocks, because the one waiting read is
answered from the tape, so the first harness needs neither JSPI nor
`Atomics.wait`. The stepping loop calls `zaqaru_step` in batches and posts
progress to the page. A live mode with a JavaScript clock and entropy
that records as it runs, and a JavaScript edge that turns the page's own
`fetch` into `/iso/net` events so the page can request the container's own
Django page, are natural follow-ons and are not needed for the demo.

Check: the recorded Django run replays in the browser to the same
console output and retired count as under wasmtime.

### Checkpoints and seeking

While playing, take a snapshot every `K` retired instructions. Seeking
to `n` restores the last checkpoint at or before `n` and calls
`zaqaru_step_to(n)`. Deterministic re-execution means `K` can be large:
at 100 MIPS a gap of 200 M instructions is a two-second seek worst case,
and re-execution is the cheap direction.

Memory is the constraint. A Django module's memory is several hundred
megabytes, and ten full snapshots would not fit a tab. So: the first
checkpoint is full, and every later one is a delta against the previous,
computed by comparing pages in a small wasm helper and storing the pages
that differ. The kernel's page ownership means most of memory does not
change between checkpoints while a server idles, and the image segment
never does. Restoring a delta chain is a copy per link, which bounds the
useful chain length; a full snapshot every few dozen deltas keeps it
short.

### The page

- A timeline over retired instructions with checkpoint marks, play,
  pause, step forward by one instruction, and step back, which is a seek.
- Lanes per process showing who holds the quantum when, from
  `zaqaru_current` sampled at each step.
- The syscall log as a clickable time axis; click to seek.
- Panels for the selected process and thread: registers, memory map,
  descriptors, and the block at `rip`.

## Two things the page must say

**Stale flags.** An arithmetic instruction whose six status flags are
overwritten before anything reads them does not record them (dead-flag
elimination in the interpreter; liveness in the transpiler). At an
instant inside such a span, the flags word is the last *recorded*
writer's. The `registers` description carries a `flags_stale` field, and
the page shows the flags greyed with a note rather than as truth.
`docs/fidelity.md` records the same divergence for signal frames.

**Speed.** The interpreter runs at 30–100 MIPS, and a browser engine may
be slower than wasmtime. A Django boot is 3.29 G instructions. The demo
should start from a checkpoint taken after boot rather than boot live,
which is the same snapshot the performance plan wants for start-up
anyway.

## Order of work and size

| Part | Size | Where |
| --- | --- | --- |
| Re-entrant guest | small | guest, kernel, host |
| Counters | small | guest, kernel |
| Exact stop | small | cpu, kernel, guest; tape header in host |
| Snapshot and restore | medium; the acceptance test | host, bake tests |
| Introspection | small to medium | kernel, guest |
| Browser harness | medium | new: `web/` |
| Checkpoints and seeking | medium | `web/` |
| The page | large | `web/` |

Everything through introspection is testable under wasmtime with the
existing test infrastructure. The browser work starts only once the
snapshot test passes.
