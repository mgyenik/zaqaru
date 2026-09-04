# Bytecode transpiler: a fast wasm-hosted interpreter for x86-64

## 0. The finding this rests on

The engine interprets x86 directly, and measurement (`docs/performance.md`
§6b, `tools/bytecode-floor/`) put a number on how much of its per-instruction
cost is *x86's* versus *our block-structured design's*: on a realistic mixed
kernel — LCG arithmetic, permission-checked load and store, a data-dependent
unpredictable branch — a minimal register-machine bytecode interpreter runs
the identical work **7.7× faster than our x86-over-`Quick` interpreter, both
under wasmtime**. Per op class the gap is ~9× on arithmetic, 5–6× on checked
memory, ~12× on branches. So the interpreter is not near its floor; the floor
is far below where the block design sits.

Two facts make this the lever, where block-level tier-1 compilation was not:

- **Block-tier-1 lost because compiled x86-in-wasm ran ~1.85× *slower* per
  instruction than the interpreter** — the permission-check helper call, the
  frame, the resolver — with no long runs to amortize it. It fought a mature
  interpreter on its home turf and lost.
- **A bytecode interpreter is ~7× *faster* per instruction than our current
  one, and it can keep control flow internal** the way the tier-1 cluster
  tried to. It gets the "stay in the fast path" benefit the cluster wanted
  *and* the low per-instruction cost the cluster lacked.

The realistic end-to-end multiplier, after a faithful transpiler carries
x86's full weight (widths, addressing, live flags, the computed-goto and
call/ret target translation), is lower than 7.7× — estimate **3–6×** — which
takes Django serving from ~50 MIPS toward ~150–300 and req/s from ~37 into
triple digits. That is the prize, and it is the first lever all session that
is not fighting a fundamental wall.

## 1. The one architectural idea that makes it safe

**The bytecode is an accelerator layered over the existing complete
interpreter, not a replacement for it.** The `Quick` interpreter already runs
Django today, faithfully, including every op, fault, syscall, signal, and
`fork`. The transpiler does not have to reach that coverage to be useful, and
must never regress it:

> Any x86 instruction the transpiler does not (yet) handle **falls back to the
> `Quick` interpreter**. Correctness is guaranteed by the fallback; the
> transpiler only ever *adds speed* on the ops it covers.

This is the whole risk story. Django runs on day one — the transpiler can
cover ten opcodes or two hundred and the container still serves the same
bytes, because the interpreter is underneath. Coverage of the hot ops (the
eval loop's ~30 bytecodes' worth of x86) buys most of the speedup; the long
tail (x87, exotic SSE, weird prefixes) can fall back indefinitely with no
correctness cost, only a speed one on code that is by definition cold.

## 2. Where it sits in the engine

Today: `run` loop → `BlockCache::entry(rip)` decodes a block of `Quick` →
interpret each `Quick` → set `rip` → repeat, re-entering the loop per block.

With the transpiler, the unit is a **trace**: a flat bytecode stream in linear
memory covering one or more blocks connected by *direct* control flow, plus a
global **address cache** mapping a guest address to a `(stream, offset)` so
that an *indirect* transfer stays inside the bytecode interpreter instead of
returning to the Rust loop.

    run(rip):
      loop:
        (stream, off) = address_cache.get(rip)          # hit: stay in bytecode
          on miss:
            block = BlockCache::entry(rip)               # decode as today
            trace = transpile(block)                     # Quick -> bytecode, or None
            if trace: address_cache.insert(rip, trace); (stream,off)=...
            else:     interpret block via Quick; continue   # fallback path
        outcome = bytecode::run(stream, off, tcb, space) # the fast interpreter
        # outcome is: a syscall, a fault, a deferred op, a preemption, or an
        # indirect transfer whose target missed the address cache — each sets
        # rip and returns to this loop, exactly as a block does today.

The bytecode interpreter is the primary execution path; the `Quick`
interpreter is the fallback for un-transpiled ops and cold targets; the
transpiler fills the cache on demand. Nothing else in the kernel changes —
syscalls, faults, signals, preemption, `fork`, invalidation all reach the run
loop the way block execution reaches it now.

## 3. The bytecode

A **register machine** (matches x86 directly; a stack machine would double the
op count — wasmi 2.0 got 2.2× from exactly this switch), a **flat
pre-resolved stream** (direct branches are `pc = offset` inside the loop, the
structural win over per-block re-entry), a **fixed lean encoding** (no
operand-shape enum to match at run time).

### 3.1 State

- **GPRs**: bytecode registers 0–15 map to guest `rax`…`r15`. A `u64` file.
- **Scratch**: registers 16–23, temporaries the transpiler uses for address
  computation and flag materialisation within one guest instruction; never
  live across guest-instruction boundaries.
- **XMM**: a separate 16×128-bit vector file for SSE (§6).
- **`%fs` base, MXCSR, x87**: read from the control block on the rare ops that
  need them (mostly fallback).
- **PC**: the stream offset, a wasm local — the one piece of hot state that
  lives in a native register the whole time.
- Guest **memory** is the engine's `Space`; loads and stores go through it so
  the permission check and fault semantics are the interpreter's, unchanged.

### 3.2 Encoding

One `u64` per op: `[op:8][d:4][a:4][b:4][flags:4][imm:32]`. A two-word `LI64`
carries a full-width constant (addresses, 64-bit immediates) in the following
word. This is the format `tools/bytecode-floor/interp.rs` already validates.
Immediates wider than the `imm` field but ≤32 bits use `imm`; 64-bit use
`LI64`. The `flags` nibble carries per-op modifiers (width class, a
retire-boundary bit — §6.3).

### 3.3 Op set (the shape, not the final table)

- **ALU**: `add sub and or xor mul` in width variants (§5.3), register and
  immediate forms. `shl shr sar rol ror`. `mov`, `movzx`/`movsx` (width-cross),
  `lea`-style address-to-register.
- **Memory**: `load`/`store` in 1/2/4/8-byte widths, address = `base + imm`
  (the common mode) with a preceding address-compute op for
  `base+index*scale+disp` (§5.2). Each carries the permission check and can
  fault (§6.1).
- **Control flow**: `br` (unconditional, internal), `cmp_br` / `test_br` /
  `arith_br` (fused compare-and-branch, §5.4 — the common conditional, no flag
  materialisation), `dispatch` (indirect: read a target guest address from a
  register, resolve through the address cache, §5.5), `call`/`ret` (§5.5).
- **Vector**: v128 `move`, `and`, `or`, `xor`, `cmpeq.i8x16`, `bitmask`, and
  the SSE2 arithmetic Django's glibc touches (§6), mapping to wasm SIMD.
- **Flag materialisation** (rare): `setflag_<cond>` computes one condition into
  a scratch register for `setcc`/`cmov`/`adc` (§5.4).
- **`defer`**: hand this one guest instruction back to the `Quick` interpreter
  (§5.7) — the escape hatch that makes §1 true.
- **`syscall`**, **`retire`** (§6.3), **`exit`** (leave to the run loop with a
  reason and `rip` set).

## 4. The interpreter

### 4.1 Dispatch — the switch-loop, and why not threading

- **The switch-loop.** A Rust `loop { match op { … } }`. Cranelift compiles it
  to a `br_table`. This is what `targum::bytecode` runs, and it is the right
  dispatch for a wasm-hosted interpreter — measurement settled it.
- **Tail-call threaded dispatch was planned here, and is struck.** The plan
  was a distinct `return_call_indirect(handlers[next_op])` per op, for the
  branch-prediction win (~1.3–1.5×) the literature reports over a central
  `br_table`. That literature is native-assembly interpreters (wasmi, Deegen),
  where a threaded tail call compiles to a raw `jmp`. Under wasmtime it does
  not: `tools/thread-dispatch/` measures two interpreters running the identical
  bytecode loop, and the threaded one is **~5.2× slower** — even with the VM
  state register-pinned through the tail calls as arguments, so it is not
  memory traffic. The decomposition (2 B iterations each): a bare `loop`
  ~0.2 ns/op, a direct `return_call` ~1.4 ns, a `return_call_indirect` ~3.0 ns.
  Two costs the sandbox forces and a native `jmp` avoids — the tail call's own
  argument-setup and frame-teardown ABI, and the indirect table dispatch's
  bounds/funcref/signature checks — where a `br_table` is an in-function jump
  that keeps state in registers. So threading is not slow to build, it is slow
  to run, and the `br_table` switch-loop is already the fastest dispatch wasm
  offers. The lever for the covered path is **fewer** dispatches
  (superinstruction fusion, §9.2), not cheaper ones.

### 4.2 The address cache

A direct-mapped array, guest address → `(stream, offset)`, the analogue of the
block cache's recent ring. An indirect transfer (`dispatch`, `ret`, indirect
`call`) reads the target guest address, computes a slot, compares, and on a hit
sets the stream pointer and PC and continues **without leaving the
interpreter**. On a miss it `exit`s to the run loop, which transpiles or
`Quick`-interprets the target, fills the cache, and re-enters. This is what
keeps CPython's computed goto — a `jmp *%rax` per bytecode — inside the fast
path; the cost is one cache probe per dispatch, not a Rust round trip and a
block re-entry.

### 4.3 Memory, faults, preemption, retirement — see §6.

## 5. The transpiler: `Quick` → bytecode

It consumes the pre-decoded `Quick` the block cache already produces (so it
does not re-derive x86; it re-shapes it), one block at a time, emitting a trace
or declining an instruction to `defer`.

### 5.1 Register mapping

Guest GPR *n* → bytecode register *n*, one to one. `%rsp`/`%rbp` are ordinary
registers here (no special casing). Scratch registers 16–23 are allocated
round-robin within a guest instruction and dead at its boundary, so no
liveness analysis across instructions is needed.

### 5.2 Addressing modes

`Quick::address` already carries the decoded `base + index*scale + disp`. The
transpiler lowers it to at most: `lea t0 = index; shl t0, log2(scale);
add t0, base; add t0, disp` — collapsing to `base + disp` (one `load`/`store`
with an immediate) when there is no index, which is the overwhelming common
case in compiled code. `%fs`-relative addressing (`Quick::segmented`, TLS —
glibc `errno`, the stack guard, CPython thread state) adds one `add t0, fs`
using the control block's `%fs` base. `%gs` is refused, loudly, as everywhere.

### 5.3 Widths and sub-register semantics

x86's 8/16/32/64-bit operations and their write-back rules are the fiddly part
that must be exact:

- A 64- or 32-bit write to a GPR is simple; a **32-bit write zero-extends** the
  full register (`mov %eax` clears the top 32 bits) — a dedicated
  `write32_zx` op or a mask in the op's width class.
- An **8- or 16-bit write preserves** the rest of the register (`mov %al` keeps
  the top 56 bits) — a read-modify-write, `write8_keep`/`write16_keep`.
- `%ah`/`%bh`/… high-byte registers: a `high_byte` variant. Rare; may `defer`
  at first.
- ALU and memory ops carry a width class (in the `flags` nibble) that selects
  the truncation. The interpreter masks operands and results to the width, as
  `Width::truncate` does today.

### 5.4 Flags

Flags are the single biggest reducible cost (measured), and the bytecode
handles them by **not representing them at all in the common case**:

- **Dead flags** (the writer's flags are overwritten before read — the analysis
  the engine already runs, `Quick::flags_dead`, extended across `call`/`ret`):
  the transpiler emits no flag work.
- **Compare-then-branch** (`cmp`/`test`/an arithmetic op, then `jcc` — the
  common conditional): **fused** into one `cmp_br`/`test_br`/`arith_br` that
  computes the condition and branches, materialising nothing. This is the
  wasmi op-fusion lesson and it removes the lazy record from the hot path.
- **Flags consumed by `setcc`/`cmov`/`adc`/`sbb`/`pushf`** (rare): the
  transpiler emits an explicit `setflag_<cond>` after the producer, computing
  just the needed flag into a scratch register, eager but only where live. A
  producer feeding `adc` (which reads CF) emits `setflag_cf` before it. No
  lazy record, no per-op flag store.

The correctness obligation: the transpiler's liveness must be conservative —
anything it is unsure reads flags (any `defer`, any op not modelled) keeps the
producer's flags live, so a fused/dropped result is only ever chosen when a
reader is proven absent. This is the `flags_dead` discipline, extended.

### 5.5 Control flow and the target-translation problem

- **Direct `jmp`/`jcc`** whose target is inside the current trace → `br`/
  `*_br` with a stream offset. Targets outside the trace either extend the
  trace (stitch, up to a size cap) or become an `exit` that the run loop
  resolves and the address cache remembers.
- **Computed goto (`jmp *reg`/`jmp *mem`)** — CPython's dispatch — → `dispatch`:
  read the target guest address, resolve through the address cache (§4.2),
  continue in the bytecode. This is the load-bearing op for the eval loop and
  the one place the multiplier is spent (a cache probe per bytecode).
- **`call`** pushes the return guest address and `dispatch`es to the target
  (direct call: a static target, still through the cache so the callee's trace
  is found; indirect call: a runtime target). **`ret`** pops the return address
  and `dispatch`es to it. Because return addresses recur (a function called in
  a loop returns to the same site), the address cache hits, and CPython's
  constant C-function calls stay in the interpreter instead of round-tripping
  the Rust loop — the thing the tier-1 cluster needed and could not afford.

### 5.6 SSE / vector — what Django actually needs

Django's floats are SSE2 (no x87), and glibc's `memcpy`/`memset`/`strlen`/
`strcmp` use SSE (`movaps`, `movdqu`, `pcmpeqb`, `pand`, `pxor`, `pmovmskb`,
`pcmpeqd`, …) — the serving profile put `memcpy` at 0.14%, so SSE is not hot in
*serving*, but it is on the path and must be correct. The engine already has
the SSE→wasm-SIMD lowering built for tier-1 (`Op::VecMov`/`VecCmpEqB`/`VecAnd`/
`VecXor`/`VecMask`, mapping to `v128.load`/`store`/`i8x16.eq`/`v128.and`/
`v128.xor`/`i8x16.bitmask`), against a Core-2 `cpuid` (SSE2 only, no AVX). The
bytecode reuses exactly that:

- Common SSE2 (the ops above plus `paddb/w/d/q`, `psubb/…`, `pcmpgt`, the
  packed moves and shuffles glibc uses) → bytecode vector ops backed by wasm
  `v128`, operating on the XMM file.
- The XMM file is 16×128-bit in the control block, read and written in place as
  `v128` (little-endian, the order a `v128.load` uses), so a scalar SSE op that
  touches only the low 64 bits preserves the high half, faithfully.
- Everything else SSE/AVX/x87/MMX → `defer` to the interpreter's vector unit,
  which already handles the full set. Correct from day one, fast on the subset
  that matters.

Alignment: verify mode settled that the interpreter does not fault a
misaligned `movaps`, so the bytecode's `v128` load/store need not either
(`v128.load` is unaligned-safe in wasm).

### 5.7 The fallback (`defer`)

Any instruction the transpiler does not model — an unmodelled op, an addressing
mode it declines, `x87`, an exotic prefix, a `lock`-prefixed atomic before it
is handled (§6.5), `rep`-string ops before they are — is emitted as `defer`
with the guest instruction's address. `defer` stores the live bytecode
registers back to the control block, `exit`s to the run loop, which runs that
one instruction through `Quick`, then re-enters the trace after it. A trace may
also simply end before an un-transpilable op; either way correctness is the
interpreter's and speed is only forgone locally.

## 6. Faithfulness — the checklist that keeps Django correct

### 6.1 Faults

A guest load/store can fault. The bytecode load/store carries the permission
check; on failure it must report a fault at the **faulting guest instruction's
address**, with the correct access kind, so the kernel's signal machinery
delivers `SIGSEGV`/`si_addr` exactly as today. So the trace carries a mapping
from stream offset → guest address (a side table, or the address is on the op),
and a faulting op `exit`s with that `rip` and the fault. A fault mid-guest-
instruction (a straddling access) must leave the guest register state as the
interpreter would — which is why the transpiler emits the permission check
*before* any state write for that instruction, mirroring the interpreter's
"decline before mutating" rule.

### 6.2 Determinism and `rdtsc`

`rdtsc`/the timebase is a function of the **retired guest-instruction counter**
(deterministic time, replayable — `docs/vm.md`). The bytecode must retire the
same count as the interpreter: exactly one per guest instruction, at its
boundary, regardless of how many bytecode ops it became. Record and replay,
and the differential and verify suites, all depend on this being exact.

### 6.3 Retirement counting

The last bytecode op of each guest instruction carries a **retire bit** (in the
`flags` nibble); the interpreter increments `tcb.retired` on it. One predicted-
taken increment per guest instruction — cheap — and it keeps retirement
guest-accurate for the timebase, the preemption budget, and the profiler.

### 6.4 Preemption

The engine preempts after a quantum of retired instructions. Per-op budget
checks are part of what makes the current loop slow, so the bytecode checks the
budget only at **back-edges and trace exits**, not per op — bounding the
over-run to one loop body, which is within the quantum's slop. Retirement stays
exact (§6.3); only the *check* is coarsened. On budget exhaustion the
interpreter `exit`s with `rip` at the next instruction, exactly where the
interpreter would stop.

### 6.5 Atomics and threads

The container runs one guest thread per process inside one wasm instance (the
engine multiplexes), so within a process there is no true concurrency: a
`lock cmpxchg`/`xadd`/`inc` is semantically its non-locked form. The bytecode
may implement the common atomics as their plain equivalents (correct for the
single-threaded execution the engine provides) or `defer` them at first —
glibc malloc and the pthread fast paths use them, so handling them early is
worth it, but the fallback keeps it optional.

### 6.6 Self-modifying code and invalidation

A trace is transpiled from guest bytes; a write to a page those bytes came from
must drop it, as it drops a block today. The store handler's code-page check
(`note_code_write`, with the clean-page cache) already queues the page; the
drain that clears the block cache also clears any trace and address-cache entry
sourced from it. The dynamic loader writing relocations into code, `dlopen`,
JITs — all reach this path; verify mode exercises it.

### 6.7 Differential correctness

Every op is validated the way the interpreter is: a harness transpiles a real
swept block, runs it through both the bytecode interpreter and `Quick`, and
asserts the register file, flags-observable behaviour, and memory match after
each. Verify mode (re-run each unit interpreted against a journal) extends to
traces. No op ships without passing the differential and the `mixed`/component
kernels. The pre-existing `rol qword,64` flag bug is a known interpreter defect
to fix independently, not a bytecode regression.

## 7. Integration points (no kernel changes)

- **Block cache**: unchanged; it still decodes and byte-keys. The transpiler is
  a consumer of its `Quick`, and the address cache and traces are invalidated
  by the same page-drain.
- **Run loop**: gains the address-cache probe and the transpile-on-miss step;
  the `Quick` interpret path stays as the fallback.
- **Kernel/syscalls/signals/`fork`/record-replay**: untouched — the bytecode
  `exit`s to the same run loop with the same `rip`/reason the block path uses.
- **Verify mode and the differential suite**: extended to traces.
- **Bake-time pre-translation (later)**: hot traces can be transpiled at bake
  and stored in the module, byte-keyed like tier-1, saving first-execution
  transpile cost — an optimisation, not a requirement, and unlike compiled
  tier-1 it costs only the bytecode's size, not a slower execution path.

## 8. What it takes to run Django, concretely

Because of §1, "run Django" does not mean "handle all of x86". It means: cover
the ops the eval loop and its hot callees execute, and let the rest fall back.
In coverage order:

1. **Integer core**: `mov`, `movzx`/`movsx`, `add sub and or xor cmp test`,
   `shl shr sar`, `lea`, `push pop`, `call ret`, `jmp jcc`, the fused
   compare-branches — with 8/16/32/64 widths and `%fs`-relative addressing.
   This alone, with the computed-goto `dispatch` and the address cache, runs
   the eval loop and most of libpython; everything else `defer`s. **This is the
   milestone that makes Django faster.**
2. **The rest of the common ALU**: `inc dec neg not`, `mul imul`, `rol ror`,
   `setcc`, `cmov`, `adc sbb`, `bt/bts`, `movsxd` — removes the `defer`s that
   the eval loop's less-common handlers hit.
3. **SSE2 subset** (§6): glibc's string/memory routines and CPython's float
   ops stop deferring.
4. **Atomics** (§6.5): glibc malloc and pthread fast paths stop deferring.
5. **`div`/`idiv`, `rep`-string ops, the long tail**: handled or left to
   fallback as measurement dictates.

Django serves correctly the instant the run loop's fallback is wired (step 0,
before any op is transpiled); step 1 is where the multiplier appears; steps 2–4
raise the fraction of the eval loop that stays in the fast path toward 100%.

## 9. Performance path, in order

1. **v1 switch-loop, integer core, address cache** — the 3–6× on the eval
   loop, the number that decides everything. Measure it on a real transpiled
   eval-loop block before going further.
2. **Op fusion** — compare-branch (in from the start), plus `load`+op and
   op+`store` superinstructions for the stack-machine patterns CPython emits.
3. **v2 tail-call threaded dispatch** — hand-emitted wasm, the last
   ~1.3–1.5×.
4. **Bake-time pre-translation** of hot traces — startup, and cold-code
   transpile cost.

## 10. Risks and open questions

- **The multiplier on real CPython, not a kernel.** The computed-goto and
  ret address-cache probes are per-dispatch and CPython dispatches constantly;
  if the probe is as expensive as the current per-block re-entry, the win
  shrinks. This is the first thing to measure (§9.1) and the number that
  justifies the rest. The prototype says the probe can be a handful of ops; the
  eval loop will say whether that holds under real dispatch pressure.
- **Trace size vs the address cache.** Small traces mean more cache probes;
  large traces mean more transpile work and more invalidation churn. The eval
  loop's natural trace is one handler; whether to stitch direct-called helpers
  in is an empirical call.
- **Flag liveness across `defer` and indirect edges** must be provably
  conservative — the one place a bug is silent and serves wrong bytes. The
  discipline is "unsure ⇒ live"; it needs the same differential rigour the
  interpreter's flags got.
- **Sub-register width correctness** is fiddly and historically bug-prone
  (the `rol qword,64` defect is a live example in the interpreter itself);
  every width variant needs a differential case.
- **Cranelift on the switch-loop `br_table`.** wasmi found a Rust compiler
  change that collapsed handler paths and cost 50%; our interpreter is our own
  wasm, but the same class of surprise (Cranelift merging or failing to
  register-allocate the dispatch) is possible and must be watched with the
  component kernels.

## 11. First increment

Build, in order, behind the fallback so Django keeps serving throughout:

1. The bytecode module in `targum` — format and switch-loop interpreter, from
   `tools/bytecode-floor/interp.rs`, using `Space` for memory.
2. The address cache and the run-loop integration, with everything deferring at
   first.
3. The transpiler for the §8.1 integer core, with the differential harness that
   transpiles real swept blocks and checks state against `Quick`.
4. Transpile one real `_PyEval_EvalFrameDefault` block and measure the multi-
   plier on it — the go/no-go for the whole system.
