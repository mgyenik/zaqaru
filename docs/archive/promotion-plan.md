# zaqaru — register promotion: implementation plan

Status: **complete** — every milestone's acceptance criterion passes;
archived on 2026-08-26. What the work settled is recorded in
[design.md](../design.md)'s "Register promotion" section, its machine-model
section, and its decision log.
Date: 2026-08-26

Companion to [design.md](../design.md), whose "Calls and ABI" section holds
the emulated convention this plan optimises, and whose interop section
holds the seams this plan must not move. The shape mirrors the archived
[MVP plan](implementation-plan.md),
[float plan](float-plan.md) and
[interop plan](interop-plan.md): each milestone has an acceptance
criterion and is done when the criterion passes, not before.

## Outcome

All five milestones landed, in order, each verified against its own
criterion; per-milestone outcomes sit under their headings below.

| Milestone | Acceptance | Outcome |
|---|---|---|
| 1. Benchmark harness | Baselines for four kernels, transpiled vs clang-native | Passed. Ratios 2.1×–5.1×, worst exactly where promotion had the most to claim |
| 2. Choke points | Byte-identical output, suite green | Passed — and the byte-identity gate found the emitter was *already* nondeterministic (jump tables in `HashMap` order); fixed with a `BTreeMap` |
| 3. Promotion | Full matrix and interop corpus pass; deltas reported | Passed. Integer and memory kernels reached **parity** with clang-native wasm; calls halved to 2.5×; float unmoved at 3.1× — its time is translation quality, not state traffic |
| 4. Measured refinement | Improvements measured, or skips justified by numbers | Both named refinements skipped with the numbers: bounded ≈5% on one kernel, against gaps owned by the guest stack's real memory traffic and by SSE translation cost |
| 5. Standing proof | Mutations 1–5 caught; mutation 6 two-sided | Passed. One mutation had to be corrected to mutate the *discipline* rather than the promotion decision — the original form accidentally proved the global-fallback property. The flags narrowing is behaviourally invisible and worth up to +62% |

## Why

Every translated instruction reads and writes machine state through wasm
globals. A wasm global is observable module state: in every major engine it
compiles to a load or store on the instance, and the engine cannot keep it
in a machine register across accesses, because someone else might look. So
a three-instruction x86 sequence that a real CPU runs entirely in registers
becomes, today, a chain of memory round-trips.

Wasm locals are the opposite. They are function-scoped and unobservable
from outside, which is exactly what lets Cranelift, TurboFan and Ion build
SSA from them and register-allocate them. Register promotion is moving the
machine state, inside each function body, from globals into locals — and
letting the engine do the actual work of putting it in registers.

The interop plan filed this as a performance project that "starts with a
benchmark, not a hunch". That is still the position: milestone 1 is the
benchmark, and the refinement milestones are gated on what it measures.

## The core observation: no IR, no SSA

Promotion sounds like it needs a compiler middle-end — an IR, SSA
construction, a register allocator. It needs none of them, for one reason:
**wasm locals are mutable**. A one-local-per-storage-cell mapping is
semantically identical to the current one-global-per-cell mapping at every
point *except* where someone outside the function can observe the state.
So the whole transformation is a storage-class change plus a flush
discipline at the observation points:

- **Entry**: copy each cell the function touches from its global into its
  local.
- **Body**: every access that today hits a global hits the local instead.
  Sub-register merges, XMM halves, flag reads — the same emitted logic
  against a different storage index.
- **Escape points**: before a call, flush dirty locals back to their
  globals; after it, reload. Before a return, flush.

Everything else the middle-end would do — SSA construction, dead-store
elimination, register allocation — the engine already does, on locals.
Building our own would duplicate the consumer. An IR earns its keep only
for cross-instruction rewriting the engine *cannot* do (fusing `cmp`+`jcc`
without materialising flags, folding addressing chains), and whether any of
that is worth having is a question milestone 1's benchmark answers — not
one to decide in advance.

## What can observe machine state, exhaustively

Promotion is correct if and only if the globals are up to date whenever
something outside the current function body can read them, and the locals
are refreshed whenever something outside can have written them. The full
list of such points, from the code as it stands:

| Point | Where emitted | Discipline |
|---|---|---|
| Direct call | `translate.rs` `emit_transfer` | flush before, reload after |
| Indirect call | same function, `call_indirect` arm | same |
| Return | `translate.rs` `emit_return` | flush after the `rsp += 8` |
| Tail jump | `emit_tail_call` = transfer + return | covered by the two above |

That is the whole list. Internal control flow — including the dispatcher
mode's `br_table` loop and recovered jump tables — stays inside the
function, where locals persist. The structurer's trapping paths
(`unreachable` for an invalid dispatch state) observe nothing. Memory is
untouched by this plan: loads and stores through `rsp`, `rbp` or anything
else keep going to linear memory, which remains the shared truth. Every
exit the structurer emits passes through `emit_return` first, and every
call of either kind passes through `emit_transfer`, so the escape
discipline lives in exactly two functions.

Two properties make the discipline cheap to get right:

- **Reload-all after a call needs no interprocedural analysis.** A callee
  may rewrite any global. But SysV callee-saved registers are preserved
  *through* the globals by the callee's own push/pop emulation, so "reload
  every promoted register after every call" is correct knowing nothing
  about the callee — including an indirect one.
- **Flags never cross a call at all.** See the next section.

## The one deliberate narrowing: flags are call-clobbered

The machine model so far has been faithful: translated code emulates the
machine, and ABI assumptions live only at seams. This plan adopts one
assumption *inside* bodies: **status flags are dead across call
boundaries**, so flag locals are neither flushed before a call or return
nor reloaded after one.

What licenses it: the SysV ABI makes ZF, SF, CF, OF and PF (all five
modelled flags) call-clobbered. Conforming code — which is to say,
compiler output, the input this project targets — never reads a flag it
did not set since the last call. Reading a clobbered flag is reading an
undefined value, and a stale local is as good an undefined value as any.

Why it is worth a narrowing at all: this is where much of the win lives.
The translator computes all five flags eagerly on every arithmetic
instruction, and almost none of those computations are ever read. While
flags live in globals, every one of those writes is observable and the
engine must perform it. Once they live in locals *and nothing flushes them
at exits*, a flag write with no reader before the next write is a dead
store to a local, and the engine deletes it — along with the parity
popcount, the overflow xor-chain, and the rest of the computation feeding
it. Flushing flags at exits would make every one of those computations
live and forfeit the whole effect.

What it forbids: hand-written assembly that passes a flag across a `call`
or relies on a callee preserving one. That code is non-conforming on the
real machine's ABI too; it was never in scope. The decision log entry for
this narrowing goes in design.md when the milestone lands.

## Architecture

### The choke points come first

GP register access already funnels through two functions —
`MachineState::read_register` / `write_register`, which take a
`RegisterSlice` and emit the shift/mask merge logic. Flags and XMM halves
do not: they are raw `body.global_get(self.machine.flag(…))` /
`vector_register(…)` calls at roughly fifty sites across `translate.rs`
and `translate/vector.rs`. Milestone 2 routes those through equivalent
choke points (`read_flag`, `write_flag`, `read_vector_half`,
`write_vector_half`) with **no behaviour change**, verified by the
snapshot tests: same input object, byte-identical output.

### Storage resolution, one code path

A per-function layer decides where each cell lives:

```text
MachineState            the globals: names, indices, the convention (unchanged)
FunctionState           per function: MachineState + a promotion map
  read/write_register   emit against local or global per the map
  read/write_flag       likewise
  read/write_vector     likewise
  copy_in(body)         entry: global → local for every promoted cell
  flush(body)           escape: local → global for every dirty cell
  reload(body)          post-call: global → local for every promoted cell
```

`FunctionTranslator` holds a `FunctionState` where it now holds
`&MachineState`. The unpromoted configuration is the **empty promotion
map** — not a second code path. That keeps promotion togglable
(`--no-promote`, for bisecting a miscompile to the pass) at zero
maintenance cost, and it means the refactor milestone and the promotion
milestone are separately verifiable.

Wrappers, thunks and the marshalling code keep talking to `MachineState`
directly. They *are* the seams; globals are their job. Nothing about the
interop mechanism moves: weak globals remain the cross-object convention,
and a transpiled object's imports and exports look exactly as they do
today.

### What gets promoted, and what gets copied when

- **Cells**: the 16 GP registers, the 32 XMM halves, the 5 flags — one
  local each, same shape and width as the global it shadows. At most 53
  locals; in practice the touched set is small.
- **Copy-in** covers every cell the function touches at all (read *or*
  written — a sub-register write reads the old value to merge, so
  written-only cells still need their incoming value). The touched set
  comes from a pre-scan of the lifted instructions; `abi/effects.rs`
  already computes read/write/kill sets per instruction across the whole
  ISA, and the same iced facts cover flags.
- **Flush** covers the statically-written set — a cell never written needs
  no write-back. First version: the function-level written set, flushed at
  every escape. Per-escape dirty tracking is a refinement milestone 4 buys
  only if the benchmark says so.
- **Reload** after a call covers every promoted GP and XMM cell (flags
  exempt, per the narrowing).
- **`rsp` is not special.** It is promoted like the rest; it is simply
  always in the touched and written sets, and the flush before a call is
  what hands the callee a correct stack pointer. The return-address slot
  write in `reserve_return_address` happens against the local, and the
  flush that follows it in `emit_transfer` publishes the decremented
  value. In `emit_return`, the `rsp += 8` happens first, then the flush —
  order matters and gets a mutation (below).

## Milestones

### 1. The benchmark harness

No optimisation work starts before the thing that measures it exists.
A `benches/` harness (criterion, wasmtime embedded — the same runtime the
tests already use) over at least four kernels chosen to stress different
state traffic:

- an integer arithmetic loop (adds, shifts, compares — flag-write heavy);
- a memory-traversal loop (array reduction — bounded by loads either way,
  the kernel promotion should help *least*);
- a scalar float kernel (polynomial evaluation or dot product — XMM
  traffic);
- call-heavy recursion (naive Fibonacci — stresses the flush/reload cost,
  the case promotion could plausibly *hurt*).

Each kernel is compiled three ways: transpiled (current output), and by
clang's own wasm backend as the ceiling; after milestone 3, the promoted
output joins as the third column. Reported measurements: wall time per
kernel and wasm binary size, plus a static count of global accesses in the
emitted code as a diagnostic — stated as such, since a static count says
nothing about execution frequency. No single scalar; the kernels disagree
with each other by design, and the report says what each one does and does
not exercise. Wasmtime is one engine and the numbers are one engine's
numbers; the report says that too.

Kernels are constrained to the implemented instruction set at `-O2`. If a
kernel trips an unimplemented instruction, the precedent from the last two
plans holds: implement it or pick the kernel deliberately, never quietly
weaken the kernel to dodge it.

**Acceptance: baseline numbers for all kernels, transpiled vs.
clang-native, published in the harness output; the harness runs on demand
and is not part of the test suite.**

*Outcome — passed, 2026-08-26.* `benches/kernels.rs` (criterion, wasmtime),
kernels in `tests/corpus/bench_kernels.c`, where the optimisation sweep
covers them across the whole matrix for free. All forty configurations
transpile with no instruction gaps. Baseline medians, transpiled (best of
gcc/clang at `-O2`) against clang-native wasm:

| Kernel | gcc | clang | wasm-native | Best ratio |
|---|---|---|---|---|
| `bench_integer` | 7.51 ms | 6.04 ms | 1.18 ms | 5.1× |
| `bench_memory` | 1.35 ms | 1.76 ms | 0.65 ms | 2.1× |
| `bench_float` | 13.3 ms | 14.4 ms | 4.2 ms | 3.2× |
| `bench_calls` | 2.05 ms | 1.94 ms | 0.42 ms | 4.7× |

The shape matches the prediction that motivated the kernel choice: the
memory-bound kernel is penalised least and the all-register kernel most,
which is the headroom promotion exists to claim. Static diagnostics:
the gcc `-O2` linked module carries 1451 static global accesses in 19 KB;
the clang one 873 in 12 KB; the native module zero in 1.2 KB.

### 2. Choke points and storage resolution

The refactor described above: flag and XMM access routed through
`FunctionState`, GP access moved onto it, promotion map present but empty.

**Acceptance: byte-identical output on the snapshot corpus, full suite
green.**

*Outcome — passed, 2026-08-26.* `FunctionState` lives in `machine.rs` with
the `Storage` resolution enum; the ~75 raw flag and XMM global accesses in
the translator now go through it, and `RegisterSlice::quad` covers the
translator's own full-width stack-pointer bookkeeping. Verified
byte-identical over 198 corpus configurations — which caught something
better than a regression: the byte-identity check found the transpiler was
*already nondeterministic* on `switch_dispatch.c`, because jump tables
lived in a `HashMap` and were emitted in iteration order. Same binary, same
input, different bytes on different runs. Fixed by making
`LiftedFunction::jump_tables` a `BTreeMap` (which also deleted `dump.rs`'s
manual key-sorting workaround); output is now stable across repeated runs
and byte-identical to the pre-refactor baseline everywhere else.

### 3. Promotion

Touched-set copy-in, written-set flush at both escape kinds, reload-all
after calls, flags exempt from cross-call traffic. On by default;
`--no-promote` keeps the empty map.

**Acceptance: the full differential matrix — both compilers, both code
models, all optimisation levels, both control-flow modes — passes with
promotion on, and the interop corpus passes unchanged (the seams see the
same globals they saw before). Benchmark delta over milestone 1 reported
per kernel, including any kernel that got slower.**

*Outcome — passed, 2026-08-26.* Full suite green (80 tests), snapshots
regenerated and reviewed: a promoted body copies in only what it touches,
runs on locals, and flushes only what it wrote. One discipline was added
during design that the plan had missed — see "the cold-path split" note
under milestone 4's heading in design.md when it lands: flags *are*
flushed at tail jumps and copied in at entry, because a conditional tail
jump into a split-off cold section may read the flags its jumper set.
Medians against the milestone-1 baseline, same machine, same day:

| Kernel | gcc −O2 | clang −O2 | wasm-native | Best ratio (was) |
|---|---|---|---|---|
| `bench_integer` | 7.51 → 1.21 ms | 6.04 → 1.18 ms | 1.18 ms | **1.00×** (5.1×) |
| `bench_memory` | 1.35 → 0.68 ms | 1.76 → 0.65 ms | 0.65 ms | **1.00×** (2.1×) |
| `bench_float` | 13.3 → 12.8 ms | 14.4 → **15.3 ms** | 4.01 ms | 3.2× (3.2×) |
| `bench_calls` | 2.05 → 1.01 ms | 1.94 → 1.05 ms | 0.41 ms | 2.5× (4.7×) |

The integer and memory kernels reached parity with clang's own wasm
backend. The call-heavy kernel halved. The float kernel is the honest
number: essentially unmoved, and the clang variant *regressed* 6.6% — the
XMM traffic is evidently not where its time goes, and milestone 4 starts
there. Static global accesses in the linked gcc module fell 1451 → 256,
size 19.1 KB → 15.0 KB.

### 4. Measured refinement

Only what milestone 3's numbers justify, in order of expected value:

- liveness-trimmed reloads (don't reload a register that is dead after the
  call — `infer.rs` already runs backward liveness over this CFG);
- per-escape dirty tracking (flush only what was written since the last
  sync, not the function-level written set);
- anything the numbers demand that this plan did not foresee.

Each refinement is a separate commit with its own before/after benchmark
run. If milestone 3's naive discipline already sits near the clang-native
ceiling on the non-memory kernels, this milestone is a no-op and says so.

**Acceptance: each landed refinement shows a measured improvement on at
least one kernel and a regression on none; each skipped refinement has the
number that justified skipping it.**

*Outcome — both named refinements skipped, with the numbers, 2026-08-26.*

- *Integer and memory*: already at the clang-native ceiling (1.03× and
  1.00×). Nothing for a refinement to claim.
- *Calls* (2.5×): the per-call gap is (1.03 ms − 0.41 ms) / ~393 000 calls
  ≈ 1.6 ns ≈ 4–6 cycles. Reading the emitted call site: five
  flush pairs, five reload pairs, the sentinel store, and three *real*
  callee-saved pushes through linear memory. Liveness could drop at most
  one or two reloads (`rdi` is dead after the call) and dirty tracking at
  most one flush (`rax` between fib's two call sites) — a fifth of the
  global traffic, which is itself under half the per-call gap. Bounded win
  ≈ 5% on the one kernel it touches; the majority of the gap is the guest
  stack's memory traffic, which belongs to the machine model, not to
  promotion.
- *Float* (3.1×, and the honest number of the project): unmoved by
  promotion because its time never was state traffic. clang compiles the
  kernel's range clamp branchlessly — `cmpltsd`/`andpd`/`andnpd`/`orpd` —
  and every one of those translates to masked arithmetic on i64 halves.
  The clang variant's reproducible +6.6% regression is recorded as is;
  the working attribution — unproven — is register pressure from ~35
  always-live locals where the globals were memory-backed. Closing the
  float gap means translating scalar SSE better (a select idiom, f64-typed
  carriers), which is a translation-quality project, not a promotion
  refinement.
- *Measurement caveat, so nobody chases a ghost*: `bench_float/gcc` is
  bimodal **across** processes — 12.8 ms in full-suite runs, 17–21 ms in
  two isolated runs on an idle machine — while tight within any one run.
  One engine, one machine; per-run comparisons are the trustworthy ones.

### 5. The standing proof

The mutation battery, against the full differential matrix:

1. delete the flush before a call;
2. delete the reload after a call;
3. delete one register's entry copy;
4. delete only `rsp` from the flush set;
5. swap the order of `rsp += 8` and the flush in `emit_return`;
6. flush flags at returns (the *inverse* mutation: it must change no
   test's outcome, confirming the tests cannot see the narrowing — and the
   benchmark must show the flag-DSE win it forfeits, confirming the
   narrowing is load-bearing for performance).

Every mutation except 6 must be caught by an existing or newly written
test. The interop mutation lesson applies: an uncaught mutation is either
a missing test to write or an unobservable difference to document, never a
shrug. Then the documentation pass: design.md gains the promotion section
and decision-log entries (including the flags narrowing), this plan gains
its outcome table and moves to `docs/archive/`.

**Acceptance: mutations 1–5 caught, mutation 6's two-sided property
holds, docs updated and plan archived.**

*Outcome — passed, 2026-08-26.*

| Mutation | Result |
|---|---|
| 1. Flush before calls deleted | Caught: 10 differential failures |
| 2. Reload after calls deleted | Caught: 10 differential failures |
| 3. One entry copy deleted | The first form of this mutation removed the register from the promotion *map* — and every test passed, correctly: an unmapped cell falls back to its global by design, so that form is semantics-preserving. The battery proved the fallback property by accident. The corrected form — promote the cell, skip its copy — was caught at once, and in an instructive way: as a hang, because the corpus function looped forever on the zeroed argument and the wasmtime harness has no execution deadline |
| 4. `rsp` dropped from the flush set | Caught: 6 differential failures |
| 5. Flush emitted before the `rsp += 8` | Caught: 4 differential failures — the stack drifts 8 bytes per call |
| 6. Flags flushed at returns (the inverse) | Two-sided property holds. Every behavioural test passes — only the byte-exact snapshots see the change, which is their job. The benchmark shows what the narrowing protects: `bench_calls` +33–45%, and `bench_integer`/gcc **+62%**, because a return flush makes flag locals live out of the loop and every iteration's computation becomes live |

## Prior art

The design above is the standard one in binary translation, which is a
reason to trust it:

- **Remill / McSema** (Trail of Bits) model machine state as one `State`
  struct in LLVM IR and promote with stock SROA + mem2reg — "state as
  memory, standard pass lifts it" is exactly our shape, with the engine's
  SSA construction playing mem2reg's role.
- **rev.ng** (Di Federico, Payer, Agosta — *rev.ng: a unified binary
  analysis framework*, CC 2017) lifts QEMU TCG output with guest registers
  as LLVM globals, promotes them to per-function locals, and lets mem2reg
  run. Structurally identical to this plan.
- **QEMU TCG** is the dynamic version of the escape discipline: guest
  registers live in `CPUState` memory, are cached in host registers within
  a translation block, and are flushed at block exits and helper calls.
  Its lazy condition codes (`cc_op`) and **Valgrind VEX's** flag thunks
  are the explicit-machinery alternative to relying on engine DSE for
  flags — the fallback if milestone 3 shows the engines' DSE is not
  getting the flag win on its own.
- **v86** and **CheerpX**, the two production x86-in-wasm JITs, both keep
  register state in wasm locals and sync memory-backed state at block
  boundaries, and both credit that with their largest speedups.
- **Van Emmerik,** *Static Single Assignment for Decompilation* (PhD
  thesis, 2007), and the UQBT/Boomerang line (Cifuentes) are the academic
  treatment of register and parameter recovery from stripped binaries —
  background for this plan, load-bearing only if we ever build SSA
  in-house, for which **Braun et al.,** *Simple and Efficient Construction
  of Static Single Assignment Form* (CC 2013) is the algorithm of choice.

## Out of scope, and why

- **Typed internal calls** — passing arguments between transpiled
  functions as real wasm parameters using inferred signatures. Deliberately
  excluded: it changes the failure mode. A wrong signature at a module
  seam gets `wasm-ld`'s loud refusal; an under-inferred signature on an
  internal call silently drops an argument. It needs its own safety story
  and its own plan.
- **Stack-slot promotion** — lifting spills and locals out of linear
  memory. Wrong side of the observability line: a stack slot's address can
  escape (the interop corpus passes pointers into frames both ways), so
  this needs escape analysis, a genuinely different and riskier project.
- **Flag computation elision by pattern fusion** (`cmp`+`jcc` as one wasm
  compare-branch). Redundant if engine DSE on flag locals works as
  expected; milestone 3's flag-heavy kernel measures exactly that, and the
  QEMU `cc_op` design is the known fallback if it doesn't.

## Risks

- **Call-heavy code could regress**: reload-all after every call is new
  work the unpromoted code never did. The Fibonacci kernel exists to
  measure exactly this, and milestone 4's liveness trimming is the named
  remedy. If it regresses and the remedy is insufficient, the honest
  outcome is a per-function heuristic, reported as such — not a buried
  number.
- **Code-size growth**: entry copies and escape flushes are new bytes in
  every function. Binary size is a first-class benchmark column so the
  cost is visible, not discovered later.
- **The engines might already optimise globals better than assumed**: in
  which case milestone 3's delta is small, the plan stops after milestone
  3 with the numbers in hand, and design.md records that promotion is not
  where the time goes. A benchmark that can return "don't bother" is the
  point of running it first.
