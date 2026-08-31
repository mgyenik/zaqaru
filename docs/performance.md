# Performance: what it costs, where it goes, and what has been tried

Status: **reference** — current as of 2026-08-31, and every number in it
was measured on one machine (Ryzen AI 9 HX PRO 370, 24 threads, 5.16 GHz
boost, a laptop with a scaling governor) on the commit named beside it.
Where this disagrees with a plan document, this is right and the plan is
stale.

Read the ratios to two significant figures and no further. The section on
how they are measured is not preamble: two of the numbers this document
used to contain were artefacts of the harness rather than facts about the
engine, and both were caught only because they were absurd rather than
because anything checked.

## 1. What a container costs

`demo/hello-django` — an ordinary Dockerfile, `python:3.12-slim` plus
nginx, gunicorn and Django — run by `docker run` and by `zaqaru-run`, same
image, same client. Measured at `ba24d46`.

| | native | wasm | ratio |
| --- | --- | --- | --- |
| boot to first HTTP 200 | 0.26 s | **24.4 s** | 94× |
| first request after that | 2.0 ms | 169 ms | 85× |
| sequential p50 | 0.3 ms | **149 ms** | 496× |
| sequential p90 / p99 | 0.9 / 1.3 ms | 166 / 171 ms | — |
| throughput | 2358 req/s | **6.66 req/s** | 354× |
| four clients, p50 | 1.5 ms | 2214 ms | — |
| four clients, throughput | 2060 req/s | 1.61 req/s | 1280× |

The run retires 3.36 G instructions in 49.6 s: **67.7 MIPS**. Compiling
the 162 MB module costs 0.23 s and is not in those figures — it is paid
once, it parallelises, and it scales with *code* rather than image size,
because a container's filesystem is data the compiler walks past.

Against the same measurement at the start of this work (`32b5ed3`): boot
55.1 s, p50 252 ms, 3.90 req/s, 38.0 MIPS. So the container is **2.3×
faster**, and its request latency has gone from about 490× native to 496×
— which is not a contradiction but a warning about single numbers: the
native side is also noisy between runs, and 2358 req/s here against 1504
in the previous run is the same native binary on the same machine.

## 2. What the shapes cost

Nine kernels, each a workload pattern rather than a program, from
`tools/microbench/bench.c`. Per unit of each kernel's own scale, so the
two columns are comparable within a row and not across rows.

| kernel | native | wasm | ratio | wasm MIPS | ratio before |
| --- | --- | --- | --- | --- | --- |
| syscalls (pipe round trip) | 172 ns | 441 ns | **3×** | 75 | 2× |
| memory_random (pointer chase) | 12.0 ns | 54.3 ns | **5×** | 92 | 10× |
| branches (unpredictable) | 4.9 ns | 288 ns | **58×** | 59 | 94× |
| float | 5.3 ns | 414 ns | **78×** | 41 | 71× |
| alloc (malloc churn) | 43.2 ns | 6.23 µs | **144×** | 79 | 671× |
| memory_sequential (32 MB pass) | 0.96 ms | 149 ms | **156×** | 113 | 375× |
| alu (dependent chain) | 0.8 ns | 217 ns | **276×** | 74 | 548× |
| calls (fib recursion) | 33.4 µs | 9.49 ms | **284×** | 115 | 697× |
| string (glibc SSE) | 1.43 µs | 1.26 ms | **882×** | 32 | 995× |

**The ratio is mostly a fact about native, not about the engine.** The
wasm column runs 41–115 MIPS across every shape; the spread in the ratio
column comes from how fast the hardware happens to be on that pattern.

Two rows moved the wrong way against the "before" column — `float` from
71× to 78×, `syscalls` from 2× to 3× — and neither is a regression in the
engine, which runs both faster in absolute terms than it did. They are
rows where the *native* leg measured quicker this time. It is the reason
this table carries absolute times rather than only ratios.

- **3× on syscalls**, because a syscall here is a Rust function call
  rather than a mode switch. kisal is genuinely cheaper than Linux at
  this, and it very nearly cancels the cost of interpreting.
- **4× on pointer chasing**, because native is stalled on memory rather
  than executing, and the interpreter hides behind the same stalls.
- **913× on glibc's string routines**, which is the outlier and the one
  shape that did not improve. One SSE instruction moves 32 bytes natively
  and costs a full decode-and-dispatch here, and none of it is lowered —
  `string` runs at 30 MIPS where everything else clears 48.

## 3. How to measure it

    tools/microbench/bench.c              nine kernels, one binary
    tools/microbench/measure.py           native against wasm
    tools/microbench/latency.py           an HTTP client with percentiles
    tools/microbench/django-latency.sh    the whole comparison, both ways

**Three costs are kept apart**, because they behave nothing alike.
*Compile* is wasmtime turning the module into machine code: fixed per
process, proportional to code. *Start-up* is everything before the
program's own work. *Steady state* is the workload once warm.

**Every timing is a difference.** Each kernel runs at scale S and at 2S
and the answer is `min(2S) − min(S)`, which cancels compile, start-up, and
any setup a kernel does before its loop — the array fill in
`memory_sequential`, the two-million-element shuffle in `memory_random`,
both large enough under interpretation to swamp what is being measured.

**The two scales are interleaved**, and this is not fussiness. Taking
every low sample and then every high one lets the core's clock decay
across the measurement, so the subtraction takes a later slower run from
an earlier faster one. That measured `calls` at 25.5, then 96.8, then 47.1
MIPS on three runs of identical code, and it was nearly filed as a
two-fold regression in the change that happened to be under test.
Alternating brings the spread to 2.5%.

**Runs are pinned to one core and the minimum of several is taken**,
because interference is one-sided. About 5% of spread remains, and a
single sample can sit 30% above the truth — which is larger than most
remaining optimisations, so nothing is believed on one measurement.

**Native runs are calibrated upward** until each lasts at least 0.3 s.
Below that a run finishes during the frequency ramp, where time is not
proportional to work: 150, 300 and 600 iterations of `calls` measured
11.0, 17.8 and 25.9 ms, which is not a straight line through anything.
An earlier version of this harness reported a native figure implying 96
billion instructions a second, about three times what the machine can do.

**Every kernel prints a checksum and the two legs are compared at matched
scales.** Three timings of programs that computed different things are not
three timings of the same thing.

### What not to report

`kisal/examples/interpret` runs the same engine compiled to x86-64 instead
of to wasm. It is two to three times faster and correspondingly quicker to
iterate on, and it is a **development instrument, not a result**. Nobody
runs an x86 interpreter on x86; the number that means anything is the
container. Its output belongs in a decision about what to optimise and
never in a report about what this costs.

## 4. Where the time goes

Three instruments, each answering a different question, each off unless
asked for because each runs once per retired instruction.

### Which instructions — `--features targum/histogram`

    cargo run --release -p kisal --example interpret \
        --features targum/histogram -- <image.tar> [argv...]

Counts retired instructions per mnemonic, and separately how many took the
fast path. The gap in a partly-lowered row is an operand shape
`quick.rs` declines, which is not visible any other way.

Django's import graph, 731 M instructions, **94.5% on the fast path**:
`Mov` 30.3%, `Cmp` 8.0%, `Je` 7.2%, `Push` 6.5%, `Test` 6.4%, `Pop` 6.3%,
`Jne` 4.8%, `Lea` 3.9%. Control transfers are **16.4%** of everything,
which is what makes a basic block about five instructions long and is the
measurement that motivated extending them.

### Which guest code — `--features targum/profile`

    TARGUM_PROFILE_OUT=/tmp/profile.tsv cargo run --release -p kisal \
        --example interpret --features targum/profile -- <image.tar> [argv...]

Counts retired instructions per guest address and attributes them against
the guest's own memory map. Django's import: **90.9% `libpython3.12.so`**,
4.8% the dynamic loader, **4.3% libc**.

By function, against the dynamic symbol table because a distro library is
stripped: `_PyEval_EvalFrameDefault` 4.39%, `PyUnicode_New` 0.70%,
`PyList_Append` 0.61%, `PyDict_SetDefault` 0.53%. **The twenty-five
hottest functions are 13% of the run between them.** Half the run needs
2,301 distinct addresses and ninety percent needs 20,392, out of 213,562
executed.

There is no hot spot, and that kills the obvious idea: intercepting
glibc's `memcpy` and `strlen` to run them on the host has a **4.3%
ceiling** on this workload, however good the interception is. What it also
says is that tier 1 is well targeted — 90% coverage is about four thousand
blocks, a working set a translator holds comfortably.

### Which engine code — `perf` with wasmtime's map

    ZAQARU_PERFMAP=1 perf record -F 999 -o /tmp/p.data \
        ./target/release/zaqaru-run <module.wasm>
    perf report -i /tmp/p.data --stdio --no-children

`ZAQARU_PERFMAP` makes wasmtime write the map that names JIT frames;
without it the whole engine is one unresolved address. **This is the
instrument that mattered**, and section 6 is about why.

Django's import at `ba24d46`: `Cpu::quick` 53%, `Engine::run` 16%,
`Cpu::push` 4.4%, `Cpu::step` 4.2%, `BlockCache::entry` 2.1%,
`Flags::status` 1.2%. The dispatch and the run loop are 69% between them,
so further gains mean less work per instruction rather than finding
another mislaid function.

Two things it cannot do. **valgrind cannot run this at all**: under it the
client binary loads inside the guest's 0x10000–4 GiB reservation and the
arena maps `MAP_FIXED` over the interpreter's own code. **gdb cannot
attach** while `kernel.yama.ptrace_scope` is 1. `perf` needs
`linux-tools`, and on this kernel the `-oem` package ships none, so the
6.8 binary is used against a 6.17 kernel and works.

## 5. What is built

Since `280f607`, in the engine:

- **Instructions are pre-decoded.** The block cache decoded bytes once and
  then re-derived the decode on every execution — operand kinds four
  times, registers three times, each through an `Option` and an error
  closure, all dispatched through a match over 1,700 mnemonics.
  `targum/src/quick.rs` lowers what it understands into a compact form
  with a dense opcode and resolved operands. **What cannot be lowered is
  not lowered**: `Op::General` falls through to the untouched `step`, so
  a bug in the lowering can make the engine slower and cannot make it
  wrong about an instruction the lowering declined.
- **Blocks extend past conditional branches.** They used to end at the
  first control transfer, and control transfers are a fifth of a real run.
  Django's import decodes 33,505 blocks where it decoded 58,074.
- **`rip` is not consulted after an instruction that cannot move it.**
  Which is nearly all of them, and it is a property of the instruction
  rather than of the lowering.
- **The block lookup is direct-mapped and self-validating.** A hit is
  believed only after the block it names is confirmed still entered at the
  address asked for, so nothing has to remember to invalidate it.
- **The edge is flushed every quantum**, while the inbound read stays on
  the slice — see `network-plan.md`.

## 6. What was tried and rejected

Every one of these was argued from reading the code, and the numbers are
why the arguments do not count.

| tried | expected | measured |
| --- | --- | --- |
| `lto = "fat"`, `codegen-units = 1` | tens of percent | **2%**, and triples build times |
| a page-permission cache in `Space` | 10–25% | **0%**, on memory-heavy shapes too |
| boxing `Trap` (32-byte return → 16) | several percent | **worse**: alu 1.47→1.41 |
| `#[inline(never)]` on `step` | large | **0.0%** to three digits |
| `#[inline(always)]` on `Cpu::read`/`write` | a win | **alu 2.59→1.49** |
| `#[inline(always)]` on `push`/`pop` | a win | **calls 3.41→3.26** |

The permission cache is the one worth dwelling on: it would have left a
permanent obligation — a fourth site that moves a bitmap and `mprotect`
silently stops being observable — in exchange for nothing measurable.

### The inlining rule, which is the whole of section 5's speed

Every win after `perf` arrived was an inlining decision the compiler had
made and recorded nowhere. **Whether LLVM honours an `#[inline]` is
invisible from the source**, and Cranelift does not inline across wasm
functions at all, so a helper LLVM declined becomes a real call per use.

- **Force the small helpers on the fast path inline.** `quick_load` and
  `quick_store` were **25.9% of `alu` as their own frames** — a call per
  operand access. `#[inline(always)]` took alu from 1.41× to 2.46×.
  `Space::load`/`store` took `memory_sequential` from 1.69× to 3.58×.
  `Tcb::read_register`/`write_register`, `Flags::record` and
  `Condition::holds` likewise.
- **Never force anything `step` also calls.** `step` matches over the
  whole of `Mnemonic`; a copy of anything sizeable inside it makes one
  wasm body Cranelift cannot allocate registers for.
- **Size decides it, not heat.** `record` is five stores and `holds` a
  sixteen-way match, so a copy inside `step` costs nothing. `read`,
  `write`, `push` and `pop` are large enough that it costs a great deal.
- **When two callers want opposite things, give them both.** `push` is an
  `#[inline(always)]` core the fast path calls and a plain wrapper the
  general path calls. Treating this as a property of the *function* rather
  than of each *call site* is what made it look like a choice.

Both directions are written at the call sites with the numbers that
decided them, because none of it can be recovered by reading.

## 7. What is open

- **Concurrency is broken.** Four clients get a p99 of 4.5 s and 1.61
  req/s — *worse* than one client at a time, where honest queueing behind
  a single sync worker would have held throughput flat. It survived every
  pump arrangement tried, so the cause is elsewhere and is not known.
- **Boot dominates and is untouched.** 24 s, and 2.1 G of the run's 3.36 G
  instructions, is Python's import graph — re-executed identically on
  every start. Halving the engine again gets 12 s. Snapshotting a booted
  container gets 0.2 s, and it is the only idea on the table at the right
  order of magnitude. The determinism the design already has is what makes
  it plausible; kisal's state being an `Rc`/`RefCell` graph is what makes
  it work.
- **`string` at 913×.** glibc's SSE routines, unlowered, 30 MIPS against
  41–115 everywhere else. Only ~2.4% of the *import*, but HTTP serving has
  not been profiled separately and is where those routines live.
- **`Cpu::quick` at 53% and `Engine::run` at 16%.** The dispatch and the
  loop. Fusing `cmp`/`test` with the `jcc` that follows is the shaped idea
  here — the histogram puts those at 14.4% and 16.4% of the stream — and
  it removes a dispatch, a flag record and a flag read per pair.
- **Tier 1 is still the step change**, and section 4 is the argument that
  it would work rather than thrash.
