# Performance: what it costs, where it goes, and what has been tried

Status: **reference** — current as of 2026-09-02, and every number in it
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
image, same client. Measured 2026-09-02, after step 1 of the plan in
section 7, with readiness read off the process's CPU use rather than
asked for with requests, which section 3 explains changed every number
below.

| | native | wasm | ratio | before step 1 |
| --- | --- | --- | --- | --- |
| boot, until the process goes quiet | 0.26 s | **29–30 s** | ~110× | 32–35 s |
| first request after that | 2.0 ms | 33–90 ms | — | 106–162 ms |
| warm request, sequential p50 | 0.3 ms | **20–22 ms** | 70× | 41–48 ms |
| sequential throughput | 2358 req/s | 46–49 req/s | 50× | 19–20 req/s |
| four clients, p50 | 1.5 ms | 70–73 ms | — | 129–136 ms |
| four clients, throughput | 2060 req/s | **52–55 req/s** | 38× | 29 req/s |
| module on disk | — | **70 MB** | — | 170 MB |

The ranges are two clients: `curl` at the low end, `latency.py`'s
`http.client` at the high, on the same module in the same minute. A boot
retires **3.29 G** instructions, all of it interpretation at about
100 MIPS. A warm request retires **1.1 M** — the difference between a boot
with forty requests and one with none, and the host interpreter's own
count agrees to three digits — which is 11 ms of interpretation inside a
20 ms request; the rest is host round trips and the edge pump, and the
profile of section 4 is now the interpreter and nothing else. Four
clients at once queue honestly behind the single sync worker (70 ms is
about four requests deep), and overlap the host round trips that a single
client waits on, which is why they get more throughput.

The "before" column is the same day, before the process switch stopped
copying whole address spaces — step 1, which took the thirty milliseconds
a warm request was spending outside the interpreter down to about nine —
and before the rootfs was compressed, which is gate T0 of
[tier1-plan.md](tier1-plan.md) and costs nothing measurable at boot.

Compiling the 170 MB module costs 0.23 s and is not in those figures — it
is paid once, it parallelises, and it scales with *code* rather than image
size, because a container's filesystem is data the compiler walks past.

What this table used to say, and why it was wrong: 24.4 s to the first
200, 149 ms a request, 6.66 req/s, and four clients at 1.61 req/s. The
first three were the readiness poll's doing — see section 3 — and the
fourth was a kernel defect, found by tracing it and fixed the same day:
under concurrent load nginx's next `connect` reused a ring slot the
half-closed worker socket still named, the worker lost `POLLHUP`, and
every request paid gunicorn's full two-second close timeout. Sequentially
nobody reconnects in the gap, which is why it never showed. The engine
figures at the start of this work (`32b5ed3`: 38.0 MIPS, p50 252 ms) were
polluted the same way and are not comparable with anything here.

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

**Readiness is never asked for with requests.** A request made while
gunicorn's worker is still importing Django queues behind it and is served,
in full, once the worker is up — so a poll every quarter second for thirty
seconds is a hundred queued requests, the first 200 arrives only after most
of them have been answered, and the timings taken afterwards interleave
with the rest of the queue draining. That is where 24.4 s to the first 200
and 149 ms a request came from; the same run measured 43 ms a request the
moment the poll was replaced. `latency.py` now reads the module's
readiness off its own CPU use (busy, then two quiet seconds), and asks the
native container every two seconds, where a request costs a third of a
millisecond and nothing worth counting queues. The boot figure is the
process going quiet, not the first 200.

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

**Serving is a different profile from booting**, and it was measured for
the first time on 2026-09-02 by attaching `perf` only for the requests.
Thirty sequential requests, as found: `Cpu::quick` 44%, **`__memmove`
23%**, `Engine::run` 12%, `__memset` 3%, `Dormant::gather` 2%, plus 3.5%
of `realloc` growing the vector the pages were gathered into. Four
clients at once, after the socket fix: `quick` 39%, **`memmove` 32%**,
`memset` 5%, `gather` 3%. The `memmove` was the process switch. Inside
the module there is no page table, every process shared one address
range, and `Dormant::gather` copied every mapped page of the outgoing
process into the kernel's heap and zeroed it, then `restore` copied the
incoming one's back — for a 60 MB CPython worker against a 4 MB nginx, a
few hundred megabytes of memory traffic per request round trip.

After step 1 (`kisal/src/resident.rs`), four clients at once: `quick`
68%, `Engine::run` 14%, `step` 6%, `note_code_write` 4%,
`BlockCache::entry` 3%. No `memmove` above the 1.5% line. The first
version of step 1 — ownership without placement — still had `memmove` at
21%, all of it under `activate`: every program bump-allocated from the
same base, so nginx's binary, libraries and heap sat exactly where the
worker's did and a few thousand pages moved on every switch. Placing each
exec'd program in the emptiest quarter of the 512 MB guest block is what
made the colliding set empty.

**A request's instruction mix is the boot's**: the histogram of a hundred
requests, taken by difference, is `Mov` 36%, `Je` 6%, `Test` 6%, `Cmp` 6%,
`Push` 5%, `Pop` 5%, `Lea` 4%, `Jne` 4% — the same shape as the import and
lowered the same way. Nothing about serving is exotic to the engine.

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
| dead-flag elimination (skip the lazy record) | flags were 2.2 ns/op | **alu +5.7%**, CPython within noise |
| a *code-page* cache in `Space` (not permission) | the store's extra check | **stores +9.5%**, CPython within noise |

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

## 6b. The interpreter's floor, measured (2026-09-03)

Section 6's rejections were "reading the code was wrong." This is the
inverse: reading the code said the interpreter was near its floor, and
measuring said it was not — but for compute-bound guests, not for CPython.

**The decomposition.** Five component kernels, each an unrolled run of one
instruction class (`tools/microbench/bench.c`: nops, regmov, regadd, loads,
stores), so the interpreter's retired-instruction rate is that op's per-op
cost. Native, ns per op: dispatch (nop) **2.42**, +register 1.44, +flags
**2.18**, +memory read 1.04, +store **3.92**. Dispatch is lean; the flags
and the store's code-page check are the reducible parts — and the two
changes above take them, worth ~6–9% on arithmetic- and store-heavy code.

**The reference.** The same freestanding kernels through Blink — a mature
x86-64 interpreter — pin the floor. Our interpreter, *under wasmtime and
paying its tax*, matches or beats Blink's **native** interpreter
(1.05–1.45×); native to native we are 1.5–2.6× ahead. So the floor, defined
as the best interpreter anyone has built, is below us, not above. The
ceiling above is a JIT (Blink's own, and qemu): 3–8× faster, but runtime
native codegen the wasm sandbox forbids — the reason tier 1 is bake-time,
and tier 1 lost on CPython (`tier1-plan.md`).

**CPython is the exception that stays at the floor.** Both changes are
within noise on it. Its blocks are short, so its arithmetic flags are live
at the computed goto (which dead-flag deliberately will not treat as dead),
and its stores hit varied pages the clean-page cache does not hold. So the
headroom the decomposition found is real for compute-bound guests and
absent for an interpreter loop — which is the shape a mature interpreter is
already built to run. For the container, the interpreter *is* near its
floor; the levers left are startup and, for compute-bound guests, these
per-op wins.

## 6c. The bytecode transpiler, v1 measured (2026-09-03)

§6b's floor prototype (`tools/bytecode-floor`) suggested a register-machine
bytecode could be 7.7× the x86 interpreter. That prototype was *idealised* —
no faithful widths, no lazy flags, a `Vec<u8>` for memory, a local
retirement counter. The real transpiler (`targum::bytecode`, `targum/bytecode`
feature) carries x86's weight: width masking on every op, the engine's
`Space` and its permission check, retirement and flags into the control
block. So the honest number is lower, and the design doc estimated 3–6×.

**Measured lower still.** Component kernels, baked both ways and run under
wasmtime (`ZAQARU_ENGINE_FEATURES=targum/bytecode` bakes the accelerator in;
each is one fully-covered block whose back-edge stays inside the bytecode),
MIPS interpreter → bytecode:

| kernel  | interp | bytecode | ratio |
| ---     | ---    | ---      | ---   |
| nops    | 240    | 319      | 1.33× |
| regmov  | 165    | 341      | 2.06× |
| regadd  | 129    | 262      | 2.03× |
| loads   | 123    | 221      | 1.79× |
| stores  |  96    | 186      | 1.94× |

So v1 — the switch-loop, faithful, dead-flag elimination folded in at
transpile time — is **1.3–2.1×** on fully-covered hot loops, not 7.7×. The
gap from the prototype is the faithfulness the prototype skipped; the gap
from the 3–6× estimate is the two optimisations the design stages after v1
and that are *not* built: op fusion (compare-branch, the lazy-flags removal
on the hot conditional) and tail-call threaded dispatch (a distinct indirect
site per op, ~1.3–1.5× in the literature).

**The finding that matters most is about coverage, not the ratio: an
uncovered op in the hot loop is a *loss*, not neutral** — a defer is
exit-the-trace + interpret-one + re-enter, dearer than interpreting the block.
Watch one kernel cross the line as its ops get covered. The `mixed` kernel
(an LCG: a multiply and a shift per iteration):

| coverage of `mixed`        | ratio |
| ---                        | ---   |
| integer core only          | 0.91× (slower than interpreting) |
| + `imul`                   | 1.25× |
| + shifts                   | 1.42× |

Each covered op moves it up; while any op in the loop defers it is at or below
break-even.

**And once a loop is covered end to end, the win depends on its op mix — and
for compute it reaches the estimate.** With `imul`, shifts, and rotates
covered, the two dependent-arithmetic kernels — which a native CPU cannot
speed up by reordering, so per-op interpreter cost dominates — measure:

| kernel | interp | bytecode | ratio |
| ---    | ---    | ---      | ---   |
| alu    | 76     | 271      | **3.59×** |
| mixed  | 74     | 102      | 1.38× |

`alu` (a pure dependent chain) lands in the middle of the 3–6× estimate; the
component loops (1.3–2.1×) and `mixed` (a checked load and store plus an
unpredictable branch per iteration, so memory- and branch-bound) sit lower,
because the permission-checked access and the branch are a smaller fraction of
what the bytecode makes cheaper. So v1 is not one number: it is ~1.4–2× on
memory-bound loops and ~3.6× on compute-bound ones, and *zero or a loss* on
anything with a deferred op in the loop. Correctness holds throughout — the 52
engine tests pass through the integrated run loop, self-modifying code and
faults included — so this is a speed question, not a safety one.

**Where that leaves the go/no-go.** v1 already reaches the estimate on
compute-bound loops it covers, and the coverage bar is total, not partial: a
workload is faster only if *every* op in its hot loops is covered. The integer
core now covers `mov`/`lea`, the ALU, `imul`, shifts, rotates, `setcc`/`cmov`/
`adc`/`sbb`, loads/stores, and the branches; what still defers in hot code is
`div`, the SSE/vector ops, and the computed-goto and call/ret dispatch — which
needs the address cache.

**Three further lifts are now built.** The lazy flags are held in a local
through the trace, flushed to the control block only at a leave, so a
`cmp`/`jcc` pair or an `adc` chain touches a register-resident record rather
than the control-block pointer per op. A flag-producer immediately followed by
a `jcc` that consumes its flags fuses into one `FusedBranch` — the two-op
dispatch becomes one, and when the flags are dead past the branch (which the
liveness pass proves, accounting for *both* the fall-through and the taken
target) the record is skipped entirely. And an **address cache** — the block
cache's own address→trace resolution — lets an indirect transfer (a computed
goto, a `ret`, an out-of-trace `call`) whose target is already transpiled stay
*inside* the interpreter, the register file and flags carrying over, only the
stream switching; a miss exits to the run loop, which transpiles the target
and warms the cache. So a whole call tree or a multi-block loop runs without
round-tripping the run loop per transfer.

The picture across shapes, under wasmtime (interpreter → bytecode MIPS):

| kernel   | shape                         | ratio |
| ---      | ---                           | ---   |
| alu      | dependent arithmetic          | 3.5–4.2× |
| branches | multi-block, branch-heavy     | 3.3× |
| regadd   | arithmetic component          | 2.0–2.2× |
| loads/stores | permission-checked memory | 1.8–1.9× |
| calls    | recursive, call/ret-heavy     | 1.65× |
| mixed    | memory + unpredictable branch | 1.3–1.4× |

So the win runs from ~1.3× on memory- and call-bound code to ~4× on compute,
entirely a function of how much of the hot path the bytecode covers and keeps
internal.

**Measured on real CPython, not kernels.** A python:3.12-slim rootfs running a
pure-Python compute loop — seven billion guest instructions through the eval
loop — comes back correct (identical result to the interpreter) at **1.5×**;
`os.fork` and a recursive workload at 1.6×; a JSON/regex/dict/class workload at
1.27×. With a fixed seed the two engines retire the *identical* instruction
count, so deterministic time and record/replay survive the bytecode path. The
accelerator covers **93–96%** of a real workload's instructions; the rest
defer (the wider SSE — float ops, memcpy shuffles — the vector unit still
owns). More coverage past this has bounded upside (`div` and the common SSE2
were added and moved the number by ~5% on the workloads that use them); the
1.3–1.6× on real code is the covered path's *per-op* cost, not coverage.

**And the per-op cost has no cheaper-dispatch lever.** Tail-call threaded
dispatch, the planned v2, is a measured regression under wasmtime — ~5.2×
slower on an identical bytecode loop, root-caused in `tools/thread-dispatch/`
to the sandbox's checked indirect tail call (~3.0 ns) against a `br_table`'s
in-function jump. The switch-loop is already the fastest dispatch wasm offers.
What is left is **fewer** dispatches — superinstruction fusion, extending the
compare-branch fusion already built — and cheaper per-op work; a real CPython
eval-loop is where those get measured next.

## 7. The plan

Three steps, decided 2026-09-02, in the order they pay. Step 1 is being
built; steps 2 and 3 wait on a design discussion and are not to be
started before it.

1. **Page ownership for the process switch — built, 2026-09-02.**
   Linear memory keeps every process's pages in place. An owner table
   (`kisal/src/resident.rs`) says whose bytes are resident at each page; a
   switch walks only the pages the incoming process has had taken from
   it, and a page moves only when two processes map the same address and
   one of them is running. A fork child collides with its parent
   everywhere by construction and stays a copy, paid once per fork; an
   `execve`'d program is placed in the emptiest quarter of the 512 MB
   guest block, so unrelated programs do not collide at all. The same
   code runs natively — the per-process memory files are gone — so the
   kernel suite tests what the module does. Expected 15 ms a request;
   measured 20–22 ms, with the interpreter now the whole of the profile.
2. **Tier 1: hot blocks compiled to wasm at bake time — waiting.** The
   only lever that reaches boot and request latency both, and it needs
   nothing from the host and no run of the user's image: a wasm module
   cannot create code, so the compile happens in the bake, and the
   result is linked in beside the interpreter and keyed by its bytes — a
   block that never runs or has been overwritten is a function nobody
   enters, so the bake may guess. It draws on three sources: a corpus of
   runtime profiles matched by content, a ranked and budgeted static
   sweep of the image's own ELFs, and an optional profile of one's own
   workload. The ceiling is measured, not guessed: the AOT transpiler
   with register promotion reached parity with clang's own wasm backend
   on the integer and memory kernels, about a hundred times above the
   interpreter. A block compiler lands short of that; five to twenty
   times is the band this shape sits in, which is a boot of a few
   seconds and requests of a few milliseconds. Size: the runtime's text
   is 5 MB of the image's 39, and the rootfs, 124 MB uncompressed,
   compresses to 37 — so the plan's first gate compresses it, and a
   module with tier 1 comes out smaller than one without it is now.
   [tier1-plan.md](tier1-plan.md) is the design.
3. **Snapshot a booted container — waiting.** The alternative for boot
   that does not depend on engine speed. The interpreter holds no guest
   state on the wasm stack and kisal's state is a graph in linear memory,
   so a memory image taken at a quantum boundary, with the run loop
   re-entered rather than restarted, is a booted container in well under a
   second. The host-side state to reconstruct is the mount table and the
   listeners; open connections are the reason to snapshot before the
   first one.

## 8. What is open

- **A fork family still moves together.** gunicorn's arbiter and its
  worker map the same 20 MB at the same addresses, so the arbiter's
  once-a-second wake-up moves those pages out and back: a few
  milliseconds a second, invisible in the request path, and the one
  shape step 1 leaves as a copy. Copy-on-write at page granularity —
  skip a page whose bytes the two still agree on — is the refinement if
  a workload ever forks and keeps both sides busy.
- **The nine milliseconds a request spends outside the interpreter** are
  host round trips: the edge pump's reads and writes, and the slice
  boundary the inbound read waits for. Not profiled yet.
- **Boot dominates and is untouched.** 32–35 s, all of it interpretation
  of 3.29 G instructions, most of them Python's import graph — re-executed
  identically on every start. Halving the engine again gets 16 s.
  Snapshotting a booted container gets under a second, and it is the only
  idea on the table at the right order of magnitude. The determinism the
  design already has is what makes it plausible; kisal's state being an
  `Rc`/`RefCell` graph in linear memory, and the interpreter holding no
  guest state on the wasm stack, is what makes it work.
- **`string` at 882×.** glibc's SSE routines, unlowered, 32 MIPS against
  41–115 everywhere else. About 2.4% of the import, and the request-path
  histogram says it is no larger there.
- **`Cpu::quick` at 53% and `Engine::run` at 16%.** The dispatch and the
  loop. Fusing `cmp`/`test` with the `jcc` that follows is the shaped idea
  here — the histogram puts those at 14.4% and 16.4% of the stream — and
  it removes a dispatch, a flag record and a flag read per pair. Worth
  perhaps 1.3× on the engine; not the order of magnitude.
- **Tier 1 is the step change**, and section 4 is the argument that it
  would work rather than thrash. The ceiling is known: the AOT transpiler
  with register promotion reached parity with clang's own wasm backend on
  the integer and memory kernels (`docs/archive/promotion-plan.md`), about
  a hundred times where the interpreter sits. A trace compiler will not
  get all of that, but five to twenty is the band the literature puts
  this shape in, and it is the only lever that reaches boot and request
  latency both.
