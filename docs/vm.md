# The userspace VM: interpretation as the floor

Status: **proposed** — a design for an alternative execution path, written
after the 2026-08-30 throughput spike said the floor is viable. Nothing in
this document is built except where a measurement is quoted with its date.
Adoption is gated, not assumed: gates G2 and G3 below are the facts this
design stands on that have not been verified yet, and the decision point is
named in the milestones. `container-plan.md` remains the design authority
for the kernel; this document is the design authority for the machine that
executes instructions, and where the two disagree about the seam between
them, this one is wrong until the disagreement is resolved in both.

The one-sentence shape: **virtualize at the syscall boundary, like gVisor —
but the CPU under the kernel is an interpreter compiled to wasm, so the
guest's instructions are data, not something a bake must understand.** The
AOT transpiler survives as an optional acceleration tier, demoted from the
thing correctness rests on to a thing speed benefits from.

## Outline

1. **Why: the case for an interpreter floor** — the three standing problems
   this dissolves, with the evidence: the static-extraction impossibility
   and the roadblock tally, the AOT deal's scope hole, and the
   resume-machinery size and complexity. The spike verdict, recorded.
2. **The shape: three tiers under one kernel** — what virtualizing at the
   syscall boundary means here; kisal is already the sentry; what each tier
   is and what each tier is *for*.
3. **The machine** — thread state, the register file, the guest address
   space as linear memory, the 4 GiB boundary, boot layout, and why
   prelink disappears.
4. **Tier 0, the interpreter** — fetch, the block cache and chaining,
   execution and where the semantics come from, lazy flags, x87 and SSE,
   the retired-instruction counter.
5. **Runtime code, finally legal** — self-modifying code and JITs handled
   by construction: the complete enumeration of writers, the page bitmaps,
   invalidation — and page *permissions*, which restore SIGSEGV, guard
   pages and PROT_NONE, a fidelity class the transpiler documents as
   impossible.
6. **The kernel seam, simplified** — syscalls as function calls; what the
   Blocked return becomes; what dies (the EH shim, the seam thunks, the
   red-zone dance) and what stays (the ll-store lowering, every kisal row).
7. **Threads, preemption, signals** — the run loop; quanta in retired
   instructions; deterministic preemption and what it does for
   record/replay; signal delivery at block boundaries; what is left of M7
   and M10.
8. **Tier 1: hot-block translation at runtime** — the host-instantiate
   contract, reusing the existing emitter and translator semantics, shared
   invalidation, per-host availability, and the profile-guided re-bake
   loop this reopens.
9. **Tier 2: the AOT transpiler as an accelerator** — entry at confident
   function starts, misses falling into the interpreter instead of dying,
   what it still needs (resume, EH), and the severability decision with
   its criteria named.
10. **What every existing subsystem becomes** — the inventory: kisal,
    baker, the x87 crate, the translator, discovery D1–D7, resume, the
    test suites — and the lockstep oracle, the strongest test this project
    could ever run, which only an interpreter makes possible.
11. **The performance model, honest** — measured numbers marked measured,
    literature numbers marked literature, and the workload arithmetic that
    says which images are served by which tier.
12. **Gates and milestones** — G1 (done) through G3, then V1–V5 with
    acceptance criteria and negative controls, and where the adoption
    decision sits relative to the current roadmap.
13. **Risks, boundaries, and open questions.**
14. **Pitfalls index** — seeded now, grown as they are hit.

## 1. Why: the case for an interpreter floor

Three standing problems, each argued from this project's own record rather
than from taste.

**The extraction is impossible, and the roadblocks say so weekly.** The
code-discovery survey settled that no sound algorithm exists for function
recovery on the binaries this project targets, and nobody claims one. The
witness model is the state of the art *for static extraction* — and the
session roadblock log (`~/scratch/zaqaru-roadblocks.md`, 2026-08-30) shows
what state-of-the-art heuristics cost to operate: 10 of its 32 code items
are discovery, extent, or jump-table work, and they are qualitatively
different from the other 22. The kernel and baker items are ordinary bugs,
fixed once against an oracle. The front-end items are **rule churn**: the
jump-table recognizer revised three times in one session, D7's safety
argument false about the very binary it was written for, and the
stated/guessed extent asymmetry — load-bearing a week earlier — killed
outright by one hand-written-assembly library. The churn concentrates
exactly where inference redirects control flow or rewrites bytes, which is
exactly where being wrong is silent. An interpreter decodes at the actual
program counter at run time: ground truth, no inference, no witnesses, no
recognizer, and that entire class of problem — and of future roadblock —
does not exist.

**The AOT deal permanently excludes half the container ecosystem.** A bake
can only translate code that exists at bake time, so `mmap(PROT_EXEC)` of
anything else is a refusal by design. That is a coherent deal — and it
means Node, the JVM, .NET, PyPy, LuaJIT, and every other JITted runtime can
never run, which is not a tail of the container population but a large
fraction of its center. "The target is any binary" was always going to
collide with this. An interpreter executes whatever bytes are at the PC,
including bytes the guest wrote a microsecond ago; the deal is repealed
rather than renegotiated.

**The machinery for suspension is the most intricate thing in the system,
and the size problem is its bill.** Resume bodies double the code section
(measured 2026-08-30: 211 MB → 317 MB, boot 12.2 s → 20.5 s wall on the
CPython container); the resume-ID encoding, the EH shim, the throw/catch
discipline, the shadow-stack restoration, the two-bodies trap policy, and
the chain walks are the subtlest code in the tree, and the one open
correctness bug in the roadblock log (a chain walk reaching an address in
Python's own data) lives there. In a VM loop, machine state is a struct;
a blocked thread is a struct nobody is advancing; suspension, preemption,
signals and setjmp are all "the loop writes fields." The entire apparatus
is unnecessary in the interpreter world — not replaced, *absent*.

**And the floor is fast enough, measured.** The 2026-08-30 spike
(`tools/interp-spike`, recorded in section 11): a straightforward
Rust interpreter — iced decode, generic operand access, eager three-flag
ALU, a preemption counter paid per instruction — retires a 7-instruction
hot loop at **221.7 MIPS native / ~125–135 MIPS under wasmtime** with a
per-block decode cache, and 77.5 / ~61 MIPS re-decoding every instruction.
The wasm tax on interpretation is 1.3–1.7×, not the feared 5–10×. The
engine module was 591 KB and compiled in ~0.1 s — against the 317 MB and
20 s of the artifact that opened this discussion. Discounts for real
dispatch breadth are applied in section 11; even discounted, the floor
carries "an arbitrary image boots and works."

## 2. The shape: three tiers under one kernel

gVisor's sentry intercepts a process's syscalls and implements Linux in
userspace, punting a narrow set of operations to the host. This project
already built that half: **kisal is the sentry.** The syscall surface, the
VFS and overlay, the VMA tree, the image format, the `/iso` capability
mounts, the loud-error policy — none of it knows how instructions execute.
What this design changes is only the CPU under it. v86 is the wrong
comparison (it virtualizes hardware — DMA, PICs, boot sectors — and is
32-bit besides); the comparison is CheerpX, which virtualizes exactly this
boundary, and whose speed comes from a tiering structure this design
adopts deliberately:

- **Tier 0, the interpreter — the floor.** Always present, always
  correct, executes anything: stripped binaries, hand-written assembly,
  runtime-generated code, code reached through pointers no analysis could
  see. Everything in sections 3–7. Correctness rests here and only here.
- **Tier 1, runtime block translation — the accelerator.** Hot blocks,
  discovered by execution rather than analysis, translated to wasm at run
  time and instantiated by the host into the same memory and table.
  Optional per host; section 8. Speed rests here.
- **Tier 2, the existing AOT transpiler — the precompiler.** Whole-function
  translation at bake where discovery is confident, entered at known
  entries, *missing into tier 0 instead of dying*. Optional per image;
  section 9. Its status changes from load-bearing to beneficial, which is
  the D6 guarantee inversion carried to its logical end: the saturated
  tier was the floor discovery was groping toward, and an interpreter is
  that floor made total, simpler, and smaller.

The deliverable stays one file. A bake links the prebuilt engine (the
interpreter plus kisal, a staticlib) with the baked image object — no
translation on the critical path, so a bake is image assembly plus a link:
seconds today, sub-second in reach. The host contract is unchanged: the
two ll-store imports, and (for tier 1 only) one optional instantiate
import. Tier 0 uses **no wasm exceptions and no tail calls**, which widens
engine compatibility relative to the transpiler's requirements; tiers
reintroduce their own requirements and say so.

## 3. The machine

**Thread state is a struct, and that one fact is most of the design.** Per
thread (the TCB, owned by kisal as M7 always intended):

- `regs: [u64; 16]` — indexed by the encoding number, because an
  interpreter's operand access is indexed and wasm globals cannot be. The
  spike's numbers already include this cost (a Rust array on wasm32 *is*
  linear memory).
- `rip: u64`.
- The lazy-flag record: `{ rule, width, left, right, result }` plus the
  materialized bits it does not cover (DF, and the sticky ones) — section
  4 for the model.
- `fs_base: u64`; `%gs` remains a loud error, same policy, now enforced at
  one decode site instead of several translation sites.
- `xmm: [[u64; 2]; 16]`, widened to 32 halves exactly as the machine image
  does today.
- The x87/MMX state: the `x87` crate's `X87State`, per-thread. The crate
  is already an interpreter; its FFI-static arrangement gains a
  per-thread pointer, which is the `x87_save`/`x87_load` integration M7
  named, arriving in a simpler form.
- Signal state (mask, pending set, altstack), `clear_child_tid`, tid —
  unchanged from the M7/M10 designs.

A context switch is a pointer swap. A snapshot is a memcpy. The 412-byte
machine-image wire format survives as the *interchange* format with tier 2
and with record/replay, not as something a seam has to marshal through
globals.

**The guest address space is linear memory, identically mapped** — guest
virtual address equals linear offset, exactly today's arrangement, with
the same consequences: `AT_PHDR` is real, `/proc/self/maps` is honest, a
program reading its own headers sees the truth. The interpreter's loads
and stores are wasm loads and stores at the guest address. wasm32 caps the
space at 4 GiB; kisal's layout already keeps every allocation under it,
and section 5's access checks decide what a wild address does (the answer
is now *SIGSEGV*, not silence). memory64 is the named escape hatch if a
workload genuinely needs the space, at a measured bounds-check cost, not
before.

**Boot layout: the same carve, minus prelink.** The engine's own data
(Rust statics, kisal's heap, the image blob) is placed by `wasm-ld` above
the highest address any image ELF states — `baker::layout` unchanged, same
`--global-base` mechanism, same check against `__global_base` at boot.
What disappears is everything prelink existed for: no bases assigned at
bake, no `EXEC_TRANSPILED` flag, no modules region in the index, no forced
`MAP_FIXED` placement — `ld.so` asks `mmap` for "anywhere" and kisal
answers from the arena like a kernel, because code is data and data can go
anywhere. (Deterministic-seeded ASLR becomes *possible*; it stays off,
because determinism is the property record/replay pays for.) The guest
stack is guest memory; the interpreter never executes on it, so the red
zone is trivially safe and the shadow-stack discipline has nothing to
protect against.

## 4. Tier 0, the interpreter

**Fetch and the block cache.** A block is a decoded run from a guest PC to
the first control transfer, capped (64 instructions) so pathological
straight-line code cannot make unbounded entries. v0 representation: the
iced `Instruction` structs themselves (40 bytes each — compaction into a
micro-op form is measured work for later, not assumed work for now), in a
`Vec` per block, with a map from entry PC to block. Two facts from the
spike shape the loop:

- The cache is mandatory: decode-every-time costs 2× (61 vs 125+ MIPS in
  wasm). The map lookup is *not* paid per instruction — only per block
  transition, and
- **chaining removes most of those too**: a block records where its taken
  and fall-through successors are once it has seen them, so steady-state
  execution walks block-to-block through direct references. The map is
  for cold entries. (The spike's cached variant paid the map only on
  taken branches and that sufficed for 125 MIPS; chaining is the next
  step, taken when measured, per the pitfalls entry on hot-loop maps.)

Every cached block is registered against the 4 KiB pages its bytes came
from — the hook section 5's invalidation hangs on. A block is entered only
through its entry PC; a jump into the middle of a cached block simply
creates a second, overlapping block, which is correct by construction and
needs no splitting logic (blocks are a cache, not a truth claim — the
contrast with the transpiler's pieces is the whole point).

**Execution: a match on mnemonics, with the semantics this project already
owns.** The interpreter's per-instruction semantics are the same facts the
translator encodes — which flags an operation writes and by what rule,
`dec` preserving CF, `xchg`'s both-reads-before-either-write, the
`r10`-for-`rcx` twist at syscalls, the segment-override-on-`lea` rule —
ported as direct Rust instead of emitted wasm, and checked by the same
differential corpus that hardened them the first time. The breadth grind
is the same campaign shape as SSE and x87: **an unimplemented mnemonic is
a loud error naming itself**, and the worklist is driven by real programs.
Two subsystems port as wholes: the `x87` crate links in as-is (it is
already the interpreter for its domain, oracle and all), and
`translate/vector.rs`'s pair-arithmetic SSE semantics transcribe
mechanically.

**Flags are lazy, by the translator's own model.** The record stores the
last flag-writing operation (rule, operands, result); consumers evaluate
what they need from it — a `jne` asks one question of the record, and only
`pushf`/`lahf`-class readers force full materialization. This is the
design the translator already validated; the interpreter inherits it
rather than re-deriving eager flags (the spike's eager three-flag model
was the conservative choice for measurement, not the design).

**One counter, three jobs.** Every retired instruction increments a
counter the spike already priced. It is the preemption quantum (section
7), the deterministic time base (`rdtsc` answers a function of it, making
the timestamp deterministic and replayable), and the profiling signal tier
1 promotes blocks on. One mechanism, no polls emitted into anything.

**Interpreter-only capabilities fall out for free** and are recorded here
so they are not rediscovered as surprises: single-stepping is the loop's
natural gait, so a gdb-stub over `/iso` is future work of modest size;
`int3` can be a real breakpoint; and watchpoints are a bitmap check the
memory path already pays (section 5).

## 5. Runtime code, finally legal

The correctness obligation is one sentence: **the block cache must never
execute stale bytes.** What makes it dischargeable here, where the AOT
world had to forbid the situation outright, is that the set of writers to
guest memory is closed and every one of them passes a hook:

1. **Guest stores** go through the interpreter's store path — one place.
2. **Kernel writes** (a `read(2)` landing in guest memory, `mmap`'s
   eager copy, the sigframe builder) go through kisal's guest-memory
   helpers — one place, hardened twice by reviews already.
3. **Mapping changes** (`mmap` over existing pages, `munmap`,
   `mprotect`, `MADV_DONTNEED`'s zeroing) are kisal rows — enumerable.
4. There is no fourth writer. No DMA, no other core, and the host only
   writes through kisal's copies of ll-store results.

The mechanism: a bitmap, one bit per 4 KiB page of linear memory (128 KiB
covers the full 4 GiB), set when any cached block includes bytes from that
page. The store path tests the bit — a shift, a load, a test, a branch —
and on a hit takes the slow path: invalidate every block registered to the
page, unchain their predecessors, clear the bit when the register empties.
The tax lands only on stores, only costs a few operations, and is paid by
an engine already spending tens of operations per instruction; section 11
carries the measurement obligation. Real x86's coherence rules are weaker
than store-visible-immediately; this mechanism is stronger than the
architecture requires, which is the right side to miss on.

The known hard case is named now rather than met as a surprise: **a JIT
that interleaves writing and executing the same page** thrashes
page-granular invalidation. The literature's mitigations (sub-page ranges,
write-protect flipping, per-block byte checksums) are tier-1-era
hardening; v0's answer is correct-and-slower, which is the failure
direction this project accepts.

**Page permissions come along almost free, and they are a fidelity class
the current design documents as impossible.** The same bitmap structure,
twice more — a readable bitmap and a writable bitmap maintained by the
`mmap`/`mprotect`/`munmap` rows — checked on the interpreter's load and
store paths, turns a wild access into what Linux makes it: **a real
SIGSEGV delivered to the guest**, catchable, with a faithful siginfo.
`container-plan.md`'s "hardware signals cannot exist" divergence — null
derefs reading garbage, guard pages unenforceable, stack overflow
corrupting silently — inverts: PROT_NONE means something, `sigaltstack`
has a reason to exist, a crashing guest dies the way it dies on Linux.
Execution permission falls out of the same check at block-decode time
(fetch from a non-executable page is SIGSEGV, which no AOT design can
say). The cost is two more bit-tests on the memory path; whether the
default is on is a measurement question, but the design carries it from
day one because retrofitting per-access checks is a rewrite and adding
them now is a branch.

`mmap(PROT_EXEC)` of anything — a dlopen'd wheel installed by pip at run
time, a JIT's fresh page, bytes decompressed into memory and jumped to —
requires nothing from anyone: the pages get permission bits, the first
fetch decodes them, the cache serves them until a writer invalidates. The
AOT deal's clause about runtime-arriving code is repealed, and with it the
`EXEC_TRANSPILED` refusal and the entire question of what the bake could
have known.

## 6. The kernel seam, simplified

Today a `syscall` instruction is a rewritten call through a generated
thunk that marshals six argument registers out of wasm globals, hands the
kernel a stack that must dodge the guest's red zone, receives either a
result or a leave sentinel, and turns the sentinel into a wasm throw that
a catch above reinterprets. Every clause in that sentence was a defect
found by a review at least once.

In the VM, the interpreter's loop reaches a `syscall` and calls
`kernel.dispatch(&mut tcb)` — a Rust function taking the thread state by
reference. The marshaling is field access. The red zone is untouched
because nothing runs on the guest stack. The outcome enum is consumed
where it is produced:

- `Done(rax)` — write it back, and set `rcx` and `r11` *faithfully* (the
  return RIP and rflags, as hardware does) rather than the transpiler's
  conformant-but-invented zeros: a small fidelity gain that costs
  nothing and removes a documented divergence from the strace-diff.
- `Blocked { .. }` — the loop parks the thread and picks another. No
  unwind exists because no wasm frames hold guest state.
- `Exit(status)` — thread teardown; process exit when it is the last.

What dies with the old seam, in tier 0: the generated syscall thunk, the
EH tag and shim pair, `x86_yield_slot`, `install_continuation`, the
shadow-stack save/restore around the kernel, the leave-sentinel agreement
test, and the throw-never-crosses-Rust-frames rule — there are no throws.
What stays untouched: **every kisal syscall row**, the VFS, the overlay,
the VMA tree, the fd table, the loud-error policy, and the ll-store
lowering — the two host imports and `cabi_realloc` remain the entire host
boundary, which is what keeps the browser host exactly as reachable as it
is today. The `Blocked` protocol M1 built "as types with one variant
used" turns out to be the durable interface; the EH transport around it
was tier-2 scaffolding.

## 7. Threads, preemption, signals

The run loop, in the shape a junior dev should recognize from any VM:

```
loop {
    let thread = scheduler.pick()?;           // or the single kernel wait
    let mut quantum = QUANTUM;                // retired instructions
    while quantum > 0 {
        if thread.pending_unblocked_signal() { deliver(thread); }
        let block = cache.entry(thread.rip);  // decode on miss
        quantum -= execute(block, thread);    // may hit `syscall` inside
        match outcome {
            Ran            => continue,       // chained to the next block
            Blocked        => break,          // park; scheduler owns it
            Exited         => break,
            Fault(signal)  => queue(signal),  // section 5's SIGSEGV lands here
        }
    }
}
```

**Preemption is a counter comparison, and it is deterministic.** The
quantum is denominated in retired instructions, so scheduling decisions
are a pure function of execution — the same container with the same
`/iso` tape produces the same interleaving, *including under preemption*.
The current design is deterministic only because it never preempts
(switches only at syscalls); this one is deterministic *and* preemptive,
which upgrades record/replay from a property of cooperative workloads to
a property of the system. Two compute-bound threads make progress against
each other — a capability the current design defers to phase two and this
one gets as a side effect of the loop owning control.

**What is left of M7 is its kernel half, which was always the good half.**
The run queue, futex buckets with bitset masking and requeue, the timer
heap, `clone3(CLONE_THREAD)` semantics, `clear_child_tid` — all unchanged,
all still host-testable native Rust. What M7 loses is everything that was
hard: no fabricated resume chains on new stacks (a new thread is a TCB
with `rip` and `rsp` set), no first-genuine-unwind milestone risk, no
sustained-suspension dispatcher stress — a suspended thread is a struct
the loop is not advancing. `fork` stays out of scope exactly as before,
but the door moves: with a sub-megabyte engine and the image sharable
between instances, an instance-per-process router stops being absurd on
size grounds; that is an observation for phase two, not a plan.

**Signals collapse into the loop.** Delivery points are block boundaries
and syscall completion — strictly finer than Linux's
return-to-userspace precision, so nothing observable is lost. Delivery
builds the `rt_sigframe` on the guest stack (or altstack) from the TCB —
the ucontext is a *copy of the actual register file*, faithful by
construction rather than by careful marshaling — sets `rip` to the
handler, and continues interpreting; `sigreturn` restores from the frame.
No reserved table slot, no resume-body signature, no splice-vs-call rule,
no chain surgery: M10's design reduces to its dispositions table, its
routing rules, and `EINTR`/`SA_RESTART` ordering — the parts that were
Linux semantics rather than machinery. A handler that blocks needs
nothing special: it is a thread in a handler frame, parked like any
other. `siglongjmp` out of a handler is register writes. And section 5's
fault path gives SIGSEGV/SIGBUS delivery for real, which M10 could not
offer at any price.

## 8. Tier 1: hot-block translation at runtime

The floor is 40–130 MIPS; native is tens of thousands. Tier 1 is how VMs
of this shape close most of that gap, and it is the *engine's* code
generation, not the guest's — the guest still cannot JIT x86 except by
having it interpreted; the engine JITs *wasm*, through the front door
every host provides.

**The contract is one optional import**:

```
install(bytes_ptr: i32, bytes_len: i32) -> i32   // base table index, or -1
```

The engine hands the host a complete wasm module (built in guest memory
by the *existing emitter crate*, which is pure Rust and compiles to
wasm32 unchanged); the host instantiates it sharing the engine's memory
and — via an imported table — its indirect-call space, and answers with
where the module's functions landed. In a browser that is
`WebAssembly.instantiate` in ten lines of glue; under wasmtime it is the
embedder API; a host without the import simply leaves tier 1 off and the
container runs on the floor. Validation is the engine's own: a malformed
module fails instantiation and the host answers -1, which the engine
treats as "stay interpreted" — a loud log line, never a correctness
event.

**What a compiled block is.** Granularity is the superblock/trace, not
the function: entered at one guest PC, exiting to the loop with the next
PC. Signature `(tcb: i32) -> i64`, body: load the registers the trace
touches into locals (the promotion machinery's register-liveness
knowledge, reapplied), run the trace's translated instructions — the
semantics are `translate.rs`'s, entered from a second consumer — store
back, return the successor PC or a sentinel (`SYSCALL`, `EXIT`) that
tells the loop to act. Guest calls and returns inside a trace are just PC
changes, so **there is no wasm-stack entanglement, no resume machinery,
and no EH** in tier 1 — the properties that made the function-granular
transpiler need them do not arise at trace granularity. Hot-block
selection is the retired counter crossing a threshold per block entry;
compilation is batched (many hot blocks per `install`) to amortize
instantiation.

**Invalidation is shared, not duplicated.** Compiled traces register
against the same page structures as decoded blocks; a write to their
pages drops them (the loop checks a generation stamp before entering a
compiled trace — one compare — because the host cannot cheaply unmap a
table entry). Same closed set of writers, same hook points, one
invalidation story for both caches.

**And tier 1 reopens the door the user asked about in August: the
profile-guided re-bake.** The hot-block set, with its *observed* entry
points and boundaries, is ground-truth discovery — dynamic evidence of
exactly the kind the code-discovery doc said could be consumed statically.
A bake that accepts a recorded profile can precompile those traces into
the image (tier 2's machinery, fed by evidence instead of witnesses), so
a fleet's steady-state images start hot. Optional, additive, and finally
resting on execution rather than inference.

## 9. Tier 2: the AOT transpiler as an accelerator

Tier 2 is today's pipeline, kept, with its meaning changed by one edit:
**an exec-map miss enters the interpreter at that address instead of
calling `kisal_no_function_at`.** Every failure mode in the discovery
record — the missed applet, the unrecovered table, the guessed extent, the
sibling-piece arm — becomes a transition to tier 0 at the faulting PC,
priced in speed. The witnesses stop deciding whether a container lives
and start deciding how fast it runs, which is what heuristics are fit
for. D6's saturated tier is cancelled outright — the interpreter is that
design's guarantee, delivered smaller (no 150k trampolines, no per-region
dispatchers) and total (mid-instruction targets included, since the
interpreter will happily decode from any byte the guest jumps to, with
the same loud possibilities Linux itself has).

What tier 2 still carries, honestly: transitions between translated
frames and the VM. Entering translated code copies the TCB into the
globals through the existing 412-byte machine image; leaving copies back.
Cheap at function granularity. But **blocking inside translated frames
still needs the resume machinery** — the wasm stack holds guest state
there, and unwinding it is what resume is for — so a build that includes
tier 2 keeps resume bodies, the EH shim, and their size bill *for the
functions it precompiles*, which composes naturally with the
suspension-analysis work (functions that cannot suspend need only one
body; the measurement of 2026-08-30 stands: 34.4% of text bytes).

**The severability decision, named with its criteria rather than left to
drift.** Tier 2 exists because it is built and it is fast. It should be
dropped if and when both of these are measured true: (a) tier 1 reaches
within a stated factor (proposed: 2×) of tier 2 on the M11 benchmark set,
and (b) the maintenance cost of the second engine — every semantics
change made twice, the divergence test surface of section 10, the resume
machinery's continued upkeep — exceeds what the remaining speed buys.
Until both are measured, tier 2 is kept and the question is left open on
purpose; deciding it today would be deciding from taste.

## 10. What every existing subsystem becomes

The inventory, so the scale of reuse is a table rather than an
impression:

| subsystem | in the VM design |
|---|---|
| **kisal** — every syscall row, VFS, overlay, VMA tree, fd table, futex, timers, image reader | **unchanged** — the largest asset carries over whole |
| the seam (`src/seam.rs`, EH shim, machine-image thunks) | tier 2 only; tier 0 calls the kernel as a function |
| **baker** — tree, tar, layers, index, xattrs, argv/envp regions, layout | unchanged; translation leaves the critical path, bake becomes assembly + link |
| prelink (`DYNAMIC_BASE`, modules region, `EXEC_TRANSPILED`, forced placement) | not needed by tier 0/1; retained only where tier 2 is baked in |
| **x87 crate** | linked into the interpreter as-is; per-thread state via its existing save/load images |
| `translate.rs` / `vector.rs` semantics | the source of truth ported into interpreter arms; entered directly by tier 1's trace compiler |
| the emitter | tier 1's backend, compiled to wasm32 inside the engine |
| discovery, witnesses, jump tables, D1–D7 (`discover.rs`, `frontend.rs`, `jump_table.rs`) | tier 2 input only; correctness-critical nowhere; D6 cancelled, superseded by the floor |
| resume machinery, resume IDs, dispatcher bodies | tier 2 only; absent from tiers 0/1 |
| runner | unchanged imports; gains the optional `install` |
| differential corpus + strace-diff | **the primary suite**, rerun against the interpreter unchanged — the oracle does not care who executed |

**And one genuinely new test capability, which may be the strongest
argument nobody has made yet: the lockstep oracle.** An interpreter can be
single-stepped; so can a native process, under ptrace. A harness that
runs the same corpus binary both ways and compares the *entire register
file after every instruction* — flags included, x87 stack included — is
the maximal-resolution differential this project could ever run, and it
is structurally impossible for the transpiler (whose observable
granularity is a whole run). Every semantics arm the interpreter grows
gets checked at retirement granularity against real silicon, the same
epistemics as the x87 crate's hardware oracle, generalized to the whole
ISA. The x87 oracle found seven undocumented behaviors in its first run;
this one inherits that track record's method. Native-only, like the x87
oracle, and worth building in V1 rather than later — it converts the
breadth grind from "implement, run corpus, debug a divergence three
layers downstream" into "implement, lockstep names the first wrong
instruction."

## 11. The performance model, honest

Three grades of number, labeled: **measured** (a date and a tool),
**discounted** (measured, then reduced for a named reason), and
**literature** (other systems' published shape, used only to size
expectations).

**Measured, 2026-08-30** (`tools/interp-spike`; 7-instruction guest
loop — ALU, load, store, flag write, taken branch — 140M instructions,
identical checksums native/wasm):

| variant | native | wasmtime |
|---|---|---|
| decode every instruction | 77.5 MIPS | ~61 MIPS |
| per-block decode cache | 221.7 MIPS | ~125–135 MIPS |

Engine module 591 KB; engine compile ~0.1 s. The per-instruction
preemption counter and the memory-indirection cost of a register file in
linear memory are *included* in these numbers.

**Discounted:** the spike dispatches over eight mnemonics with three
eager flags and friendly operands. Full dispatch breadth (hundreds of
arms), lazy-flag bookkeeping, real operand-form variety, and the page
bitmaps plausibly cost 1.5–3×. Working estimate for tier 0 at real
breadth: **40–80 MIPS in wasm**. Gate G2 exists to replace this estimate
with a measurement; nothing downstream may quote the estimate as a fact.

**The workload arithmetic**, at 50 MIPS for conservatism:

| workload | rough instruction budget | tier 0 outcome |
|---|---|---|
| syscall-dominated (the design's 80% case) | kernel-bound | barely degraded — kisal, not the CPU, is the cost |
| CPython start to `print` | ~10⁸ | ~2 s — usable floor |
| a Flask request | ~10⁷ | ~200 ms — usable floor, not a product number |
| CPU-bound Python (pystone-class) | 10⁹/s native appetite | 100–300× off native — needs tier 1 |
| BLAS / codec kernels | vector-dense | disqualified on the floor; tier 1 or tier 2 territory |

**Literature:** CheerpX-class systems (interpreter + runtime wasm
translation, the tier 0+1 shape) land single-digit-multiples off native
on integer code; qemu-TCG-class block translation historically 5–20×.
That is the band tier 1 is expected to reach, and no number in it may be
promised until V5 measures ours.

The boot story needs no model: the engine compiles in ~0.1 s wherever it
runs, the image is data, and the 20-second, 383-CPU-second boots of the
translated CPython container are an artifact of the design this document
is the alternative to.

## 12. Gates and milestones

Gates first, M0-style — hours each, a recorded verdict, a named reroute.

**G1 — the floor is fast enough. DONE, 2026-08-30**, verdict in section
11: interpretation in wasm pays 1.3–1.7× over native and the cached
variant clears 125 MIPS on friendly code. The reroute ("if under ~20
MIPS, tier 1 is load-bearing from day one") was not needed.

**G2 — breadth does not collapse dispatch.** Extend the spike's engine to
the real corpus: the integer/branch/flags surface the differential corpus
already exercises (~a hundred-plus mnemonics), lazy flags, the page
bitmaps on. Run the corpus programs interpreted; measure MIPS on their
mix. Verdict line: ≥ 25 MIPS in wasm keeps tier 0 self-sufficient for the
floor; below that, tier 1 moves from V5 to V3 and the milestones reorder.
This gate is where the section-11 discount stops being an estimate.

**G3 — the tier-1 contract exists on both hosts.** A hand-written wasm
module instantiated at run time by (a) a wasmtime embedder and (b) a
browser page, importing the engine's memory and table, called through
`call_indirect`, writing guest-visible state. Hours. Reroute if a host
refuses: tier 1 is unavailable there and the floor carries it — recorded,
not fatal.

Then the milestones. Each ships with its acceptance list; the standing
suite grows, never shrinks.

**V1 — the engine, against the corpus.** The interpreter crate (tier 0
core: block cache, lazy flags, loud-error dispatch), the x87 crate linked
in, and the **lockstep oracle** from section 10. Acceptance: every
existing corpus program runs interpreted with output byte-identical to
native; the lockstep harness passes over the corpus and *fails when a
semantics arm is deliberately broken* (the negative control that proves
the oracle sees); an unimplemented mnemonic names itself.

**V2 — the boot ladder, again, on the floor.** kisal linked under the
interpreter with the direct-call seam; the bake's engine+image link path.
The ladder is the one M6 climbed: static hello → stripped busybox applets
→ dynamic hello with `ld.so` interpreted and **no prelink** → CPython
`print("hello")`. Acceptance: each rung byte-identical to native, the
CPython rung's syscall trace equal to the transpiler path's trace modulo
documented divergences — and the `rcx`/`r11` fidelity fix means the
divergence list gets *shorter*, which the diff must show. **This is the
adoption decision point**: V2 green means the VM path has reproduced the
project's checkpoint on a fraction of the machinery, and the roadmap
choice (which path carries M7–M11) is made here with both alternatives
running.

**V3 — threads, preemption, signals.** M7's kernel half on the run loop;
M10's dispositions and delivery through the loop's frame builder; the
fault path delivering SIGSEGV. Acceptance: the planned pthread corpus;
CPython `threading`; **two compute-bound threads interleaving under the
quantum** (the capability the transpiler defers — demonstrated, and
deterministically: two runs, identical interleaving, bit-identical
output); a null-deref corpus program dying with the same signal, status,
and (given a handler) the same handler-observed `si_addr` as native.

**V4 — runtime code.** The invalidation machinery under a hand-written
JIT corpus program (emit a function into an anonymous executable page,
call it, rewrite it, call again — the stale-execution negative control:
with invalidation disabled the test must observably fail); then the
trophy: a real JITted-runtime image (LuaJIT is the smallest honest one)
executes correctly. The AOT deal's repeal, demonstrated rather than
claimed.

**V5 — tier 1, behind G3.** Hot-block selection, batched trace
compilation through the existing emitter, the `install` import on both
hosts, shared invalidation with the generation stamp. Acceptance: the
whole differential suite green with tier 1 *forced on* for every block
(correctness under maximum compilation); the M11-style benchmark set
showing the speedup, recorded against section 11's literature band; a
page write invalidating a compiled trace mid-run (negative control).

Sequencing against the standing roadmap: nothing here blocks the current
path — the transpiler keeps running CPython today, and V1–V2 are buildable
beside it, mostly from parts both paths share. The plan's M7–M11 are
deliberately *not* restated here; whichever path wins V2's decision
carries them, and their kisal halves are identical either way.

## 13. Risks, boundaries, and open questions

- **The 4 GiB ceiling.** wasm32 linear memory caps the guest address
  space. Today's containers live under it by construction; a workload
  that genuinely wants more (a JVM heap, a mapped dataset) hits a wall no
  tier fixes. memory64 is the escape hatch — engines ship it — at a
  bounds-check cost that would need its own gate. Boundary, stated: v1 of
  this design is a 4 GiB machine.
- **G2 is the load-bearing unknown.** Every number downstream of section
  11's discount assumes dispatch at breadth degrades gracefully. If a
  real-mix corpus lands under 25 MIPS, the design does not fall — tier 1
  moves forward in the schedule — but the "floor alone suffices"
  sentence stops being true and the doc gets corrected.
- **JIT page thrash** (section 5) is the known worst case for
  invalidation; v0 is correct-and-slow there, and the hardening has
  prior art but no design here yet.
- **Semantic breadth is the same grind as ever.** An interpreter does not
  shrink the x86 ISA; it changes the failure mode (loud, runtime-driven)
  and the debugging instrument (lockstep names the instruction). The
  campaign cost is real and the corpus/oracle infrastructure is the
  mitigation, not a waiver.
- **Two engines means divergence risk while tier 2 lives.** The same
  instruction must mean the same thing interpreted and translated. The
  mitigation is mechanical: the differential suite runs the corpus
  through both paths and compares them *to each other*, not only to
  native — a divergence between tiers is a failure even when both match
  native on the happy path. This test class exists the day tier 2 first
  coexists with the interpreter.
- **Tier 1's host contract is not uniform.** A static-wasm-only host
  (some serverless runtimes) gets the floor only; the design degrades
  rather than fails, but the performance promise is host-conditional and
  every published number must name its host.
- **Determinism must be defended, not assumed.** The quantum makes
  scheduling deterministic only if retired counts are — which bans
  host-observable time from leaking into the loop's decisions anywhere
  outside `/iso`. This is the existing nondeterminism-inventory
  discipline extended to one new consumer.
- **Open: what the engine is called.** This document says "the engine"
  and "the VM"; a crate needs a name (`vm/` is the placeholder). Naming
  is deliberately not decided here.
- **Open: the wasm-EH dependency in mixed builds.** Tier 0/1 need no EH;
  a tier-2-carrying build keeps it. Whether the engine ships one build or
  two (a floor-only engine with wider host reach, a full engine for
  tier-2 images) is a packaging question V2 can answer with real sizes.

## 14. Pitfalls index

Seeded from the design work and the spike; grown as they are earned.

1. **Do not put a map lookup in the per-instruction path.** The spike's
   cached variant pays the map only on taken branches; chaining exists to
   remove even those. A HashMap probe per retired instruction is the
   first way this engine gets slow.
2. **Register a block against *every* page its bytes touch, not its
   first.** A block spanning a page boundary that is registered once is a
   stale-execution bug that manifests only when a JIT writes the second
   page — the silent class.
3. **The kernel writes guest memory too.** `read(2)`, `mmap`'s copies,
   sigframes: if a kisal helper gains a new write path that skips the
   invalidation hook, SMC correctness silently dies. The hook belongs in
   the single guest-memory write choke point, and a new writer bypassing
   it should be structurally hard, not merely discouraged.
4. **Deliver signals at block boundaries only.** Mid-block delivery
   observes a half-executed block's state; the lazy-flag record and
   partially-advanced `rip` are not a consistent machine. The TCB is
   consistent exactly at retirement boundaries.
5. **`rcx`/`r11` at syscalls are faithful here — keep them so.** The
   transpiler zeroes them (conformant); the interpreter sets the real
   values. Do not "simplify" to zeros: the strace-diff's divergence list
   shrank because of this, and tests pin it.
6. **Tier transitions go through the machine image, both directions,
   always.** Tier-2 code with state in globals and the VM with state in
   the TCB will disagree silently if any transition path skips the
   412-byte copy. One function pair, no exceptions.
7. **The x87 crate's state is per-thread now.** Its static-singleton FFI
   arrangement was correct under one interpreter thread of execution;
   wiring it into the TCB without the save/load discipline reintroduces
   the cross-thread x87 corruption M7 was designed to avoid.
8. **An overlapping block is fine; a *merged* one is not.** Two entries
   into the same bytes are two cache entries. Trying to share suffixes
   between blocks rebuilds the transpiler's splitting problem inside the
   cache for a memory saving nobody measured.
9. **Do not let tier 1 compile through a `syscall`.** A trace ends at
   the instruction before it; the loop owns syscalls. A compiled trace
   that calls the kernel inline has recreated the seam this design
   deleted, wasm frames and all.
10. **The interpreter's Rust stack is not the guest's.** No guest value
    lives on it; nothing about guest `%rsp` constrains it. The moment
    those mix — a helper "borrowing" guest stack for scratch — the red
    zone class returns from the dead.

