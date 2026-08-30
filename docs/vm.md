# The userspace VM: interpretation as the floor

Status: **in progress** — a design for an alternative execution path,
written after the 2026-08-30 throughput spike said the floor is viable, and
now partly built. **V1's engine and its oracle exist** (`targum/`, 2026-08-30);
everything from V2 on does not, and section 12 says exactly which is which.
Adoption is still gated, not assumed: G2 and G3 are unanswered, no MIPS
number for the real dispatch breadth has been measured, and the decision
point is where it always was. `container-plan.md` remains the design authority
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
15. **Open questions from the build** — the decisions V1 raised and the
    seam waits on: how the kernel reaches thread state and guest memory,
    what the address-space default costs, the fidelity choices already
    made that want confirming, and where V1 actually ends. Thirteen
    questions, each now answered in place.

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
the loop is not advancing. `fork` is section 7a, and it is
*cheaper* here rather than out of scope — this sentence used to defer it
and the deferral was read as a policy, which it was never entitled to be.

### 7a. Fork, which the interpreter makes cheaper rather than harder

`container-plan.md` specifies fork in detail and treats it as critical:
`/iso/proc/spawn`, a host-side router, type-graduated fd hoisting, the
`vfork`/`posix_spawn` fast path. **None of that analysis is superseded
here.** It is about POSIX — that a pipe's ring is genuinely shared, that
`{ read a; cat; } < file` shares one offset cell, that a prefork server's
workers all `accept` one listener — and POSIX does not change because the
CPU did. The hoisting design, its type grading, and its ordering rule
(hoist, *then* snapshot) carry over unchanged.

What changes is the price, and it changes in this design's favour.

**The structural constraint is the same, for the same reason.** A guest
address is a linear-memory offset, so two live processes cannot share one
memory: the child needs the parent's addresses. Process is still an
instance; a thread is still a control block inside one. The kernel is
still replicated per process, and that is still a feature at fork time —
the fd table, the VMA tree and the signal dispositions are copied with
everything else, so inheritance is POSIX-correct by construction.

**What the interpreter deletes is the resume half.** Under the transpiler
a fork is a snapshot *plus a way back into the frames it was taken from*:
resume IDs threaded through the guest stack, resume bodies for every
function, a driver that walks the chain re-entering each frame at its
post-call block. `tests/fork_resume.rs` exists to prove that walk
reconstructs the parent's stack without re-executing it, and the resume
bodies are the 211 MB → 317 MB bill measured on 2026-08-30.

Here there is nothing to reconstruct. The child's machine state is a
`Tcb`; its address space is bytes. A fork is:

1. hoist whatever is genuinely shared (the AOT design's analysis, verbatim);
2. copy the parent's mapped pages and clone the kernel's structures;
3. in the child, `%rax = 0`, and `%rip` is already past the `syscall`.

The child resumes by *being interpreted*, which is the only thing the
loop ever does. No resume IDs, no resume bodies, no chain walk, no
shadow-stack restoration — and therefore no doubled code section, because
there is no code section. The snapshot is a `memcpy` and the entry is a
field assignment, which is the same sentence section 3 makes about
context switches, applied to a second process.

**And the module is shared.** Under the transpiler each process's module
*is* its program, so a second process means instantiating a second
enormous module. Here every process runs the same engine + image module,
compiled once by the host and instantiated many times. Instance-per-process
stops being a size argument at all.

**Two ways to make a process's memory current, one kernel above them.**
The switch is "put this process's linear memory at the guest's addresses",
and the two worlds answer it differently: in the module the instance *is*
the memory, so the router switches by calling into a different instance;
natively each process's memory is a `memfd` and the switch is one
`MAP_FIXED` mapping of it over the guest range — a page-table swap, which
is what a real kernel does at the same moment. Everything above that seam
— pid allocation, `wait4`, `SIGCHLD` routing, the fd hoisting — is
ordinary Rust and is tested natively, which is what makes the native
answer worth having rather than a stand-in.

**Built, as of 2026-08-30.** `kisal/src/system.rs` is the process table:
`fork`/`vfork`/`clone`-with-process-flags duplicate the address space
(`copy_file_range` between two `memfd`s) and the kernel's structures field
by field; the child returns zero from a `%rip` already past the `syscall`.
`execve` replaces an address space in place, keeping the descriptors, the
working directory and the ignored dispositions, and resetting the caught
ones. `wait4` parks and is completed *after* the switch back to the
waiter, because a process's bytes exist at the guest's addresses only
while it is current. `SIGCHLD` is raised at the parent through the same
disposition check a `kill` goes through, and an exiting process's children
are reparented to the container's first process, which is what `init` is.
Processes are scheduled round-robin on the same retired-instruction
quantum the threads inside them use, so the whole interleaving — across
processes as well as threads — stays a pure function of execution.
`kisal/tests/interpreted.rs` runs a guest through each of these against a
native run of the same bytes, and against the container's own rule where
the host is not the oracle.

**And the fd hoisting is built, structurally.** Until pipes, every
descriptor a container could open was safe to *copy* into a child: an image
file is read-only and a console has no offset the two processes fight over.
A pipe is the first that is neither, and it is what the plan's hoisting
analysis is about.

What the interpreter changes here too is where the shared thing lives.
`kisal/src/pipe.rs` holds every ring in one arena that the whole process
tree shares — an `Rc` on the kernel, cloned by `fork` and kept by `execve`
— and a descriptor holds an *index* into it. So the descriptor table is
copied (the numbers, the flags, the close-on-exec bits: all per-process,
all POSIX-correct by construction) and the buffer is shared, which is
exactly the split hoisting exists to produce. The plan's ordering rule —
hoist *before* the snapshot — is satisfied by there being nothing to
snapshot: the bytes were never in either address space.

The accounting is where it can go wrong, and it goes wrong by hanging
rather than by being wrong: a reader sees end-of-file when the last
*writer* descriptor closes and a writer gets `EPIPE` when the last reader
does, so a fork raises both counts once per copied descriptor and every
`dup`, `dup2`, `dup3`, `close` and `execve` moves them. Rather than five
call sites each remembering the right direction, the table is censused
before and after and the difference applied.

A transfer that cannot finish parks *as a record* on the thread rather
than as a syscall to re-run, and `write` is why: a write of more than the
64 KiB a pipe holds moves in pieces, and POSIX says the caller sees one
count at the end. It is completed on the parked process's own turn, never
on the turn of whoever woke it, for the same reason `wait4` is — a guest
address means this process's bytes only while this process is current.
Which also means "no thread here can run" stopped being a deadlock: it is
a question about the container, so `Process` reports `Idle` and only
`System`, which can see every process, calls it.

**`poll` and `epoll` are built on the same idea**, and the idea is forced
by the same rule everything else here is: readiness is asked *while
choosing which process to run*, so it must be answerable without touching
any process's memory. `epoll` already keeps its set in the kernel. `poll`
is handed one on every call, in the caller's own address space — so
parking a `poll` copies its set into a kernel one that lives for exactly
that call. Which makes a `poll` an `epoll` set with a short life, and
makes the readiness path one function instead of two.

That rule is not theoretical. Written the other way — re-reading the
caller's `pollfd` array to decide whether to wake it — the array is read
while the *forked child's* memory is at the guest's addresses, and a
forked child has a stack at the same address. It does not fault. It
answers with somebody else's bytes and the wait never wakes.

A wait with no timeout costs nothing: the thread is not runnable and its
process is scheduled again exactly when the answer changes. A wait *with*
a timeout spins, because there is nothing to sleep on — the host boundary
is two `ll-store` imports and neither of them waits.

**And a process that ends lets go of everything.** Its descriptors and its
address space, before there is a zombie: a zombie is a status and a
process id so a parent can ask what happened, and nothing else. Keeping
the descriptors keeps a pipe's writer count standing, and the parent's
`poll` on the other end waits forever for an end-of-file that already
happened — which is exactly how `subprocess.run` hung, and exactly what
the stall report (`System::stall`, printed whenever a container
deadlocks) said in one line.

**Verified in the blob:** `subprocess.run([sys.executable, "-c",
"print(6*7)"], capture_output=True)` and `subprocess.check_output`, inside
a 124 MB `.wasm` — a second CPython forked, `execve`d, captured through a
pipe, waited for, and reaped.

What is left is sockets, which are the next thing whose state two
processes share, and which hoist the way a pipe does because the arena is
already that shape.

The `vfork`/`posix_spawn` fast path is unchanged and is still the case
that matters: no snapshot at all, the child instantiated fresh from the
image with fd dispositions applied. The AOT design's note that this is
what real code overwhelmingly does — the traced application's only
fork-shaped call was a `CLONE_VFORK` — is if anything stronger here,
because instantiating fresh is now instantiating the same module again.

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

**G2 — breadth does not collapse dispatch. ANSWERED, 2026-08-30: 29 MIPS
in wasm**, measured on CPython importing numpy — 3.04 G instructions at
full dispatch breadth, lazy flags and page bitmaps on. The verdict line
asked for ≥ 25 MIPS and the reroute is not needed; tier 1 stays at V5.
The original text follows.

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

**V1 — the engine, against the corpus. PARTLY BUILT, 2026-08-30.** The
interpreter crate (tier 0 core: block cache, lazy flags, loud-error
dispatch), the x87 crate linked in, and the **lockstep oracle** from
section 10. Acceptance: every existing corpus *function* runs interpreted
under the lockstep oracle with the register file identical to native at
every retirement; the harness *fails when a semantics arm is deliberately
broken* (the negative control that proves the oracle sees); an
unimplemented mnemonic names itself. (This line originally said "corpus
program … output byte-identical to native" — corrected per Q10 in section
15: running a *program* needs the seam, which is V2's first item, so the
ladder is V2's, and the oracle is the stronger check.)

What exists, in `targum/`: the thread control block, the lazy-flag record, the
address space with its page permissions and its code bitmap, the block
cache with page-granular invalidation, and the interpreter — the integer
core, the SSE surface transcribed from `translate/vector.rs`, and the x87
reached by calling the crate directly. It builds for wasm32 and for the
host from one source with no second address model. An unimplemented
mnemonic names itself, with its exact encoding.

**The oracle exists and works, which is the part worth reporting.** It runs
a corpus probe in the interpreter and in a `ptrace`d process at once and
compares the general-purpose registers, the defined flags, the sixteen XMM
registers and all eighty bits of every x87 stack register after *every*
retired instruction. Its negative control passes: a deliberately
desynchronised machine is caught. Its coverage is asserted rather than
hoped for — 73 integer, 99 vector and 70 x87 mnemonics were put through
both machines, and the corpus fails if a probe's point gets folded away.
In its first hours it found five real defects, four of them in code that
had already passed a hardware oracle of its own: `inc` reading a stale
carry out of the lazy record, `fstp m80` leaking the previous operation's
C1, `fninit` erasing register data that hardware only marks unreachable,
the NaN tie-break on equal significands, and the denormal-operand flag not
being suppressed on the NaN path. See the pitfalls index.

**The seam is half built (2026-08-30).** kisal owns the page table, every
kernel access to guest memory goes through it, the mapping rows write it,
and the `PROT_EXEC` refusal — the AOT deal's clause about runtime-arriving
code — is now gated to the ahead-of-time world, which is the repeal
section 5 promised, expressed as a deletion. See Q1, Q3 and Q4 in
section 15.

**And a real program runs (2026-08-30).** `kisal::run::Process` is the
loop — fetch a quantum, serve what stopped it, go round — and six static
glibc programs boot and run to completion under it, each compared against
the *same binary* run natively rather than against a constant:
`write`/`exit`; `printf`, which is format parsing over a buffered stream
with an `ifunc`-selected `memcpy`; `malloc`/`free`/`calloc` across `brk`
and a megabyte through `mmap`; the string routines, which are the
hand-written assembly the discovery front end has the hardest time with
and which the interpreter never has to find; double arithmetic with
`sqrt` and `%f`; and `strtold`/`%.21Lg`, which is glibc's own
extended-precision path — `fprem`-driven scaling and control-word
manipulation — through the x87 crate.

**The whole breadth bill for those six programs was one instruction**:
`rdssp`, the shadow-stack probe, which on a processor without CET does
nothing and leaves its destination alone — exactly the answer glibc reads
as "no shadow stack". The loud error named it, with its encoding, at the
first program that reached it.

**V2's ladder is climbed, and the deliverable exists (2026-08-30).**

Dynamic loading works, with prelink absent rather than disabled: a
position-independent file arrives with no base, the *address space*
finds room for it as a `PROT_NONE` reservation and the segments are
mapped over it — which is what a kernel does and what the prelink design
existed to avoid having to do. `ld.so` runs interpreted, maps `libc` for
itself, and writes relocations into pages it is about to execute. So does
`dlopen`: a shared object nobody named at link time, opened by path,
relocated into a fresh mapping and called. That is the AOT deal's clause
about runtime-arriving code, repealed and demonstrated rather than
claimed — and expressed in the code as a *deletion*, the `PROT_EXEC`
refusal now gated to the ahead-of-time world.

And the artifact: **engine + image, linked into one `.wasm`, carrying a
program the bake never read.** `examples/bake-vm.rs` is the whole bake —
a directory becomes an image, the image becomes one object, and the
object is linked with three staticlibs that are identical in every
container.

| | AOT CPython container | interpreted |
|---|---|---|
| module | 211 MB (317 MB with resume) | **121 MB**, of which 119 MB is the filesystem |
| the engine itself | — | **2.2 MB**, the same in every container |
| bake | minutes of translation | **2.1 s** (0.2 s image, 0.13 s link) |
| host compile | 12.2 s (20.5 s with resume) | **0.24 s** |

**The measurements, 2026-08-30.** A static glibc `hello`: 3.0 MB module,
compiled in 0.07 s, run in 0.01 s. CPython 3.12 printing a string: 51.1 M
instructions at **55.7 MIPS** native. CPython importing **numpy 1.26.4**
and doing array arithmetic through BLAS — `arange`, `reshape`, `sum`,
`mean`, a matrix multiply — 3.04 G instructions at **50.7 MIPS** native
and **~29 MIPS under wasmtime**, 105 s wall.

That last number is the one section 11 owed. **G2's verdict line was
"≥ 25 MIPS in wasm keeps tier 0 self-sufficient for the floor", and the
measurement is 29 MIPS — on a real workload at full dispatch breadth,
with lazy flags and the page bitmaps on**, not on the spike's eight
mnemonics. The wasm tax measures 1.7×, at the top of the 1.3–1.7× band
G1 predicted. The section-11 discount can stop being an estimate: the
working estimate was 40–80 MIPS and the answer is 50 native, 29 in wasm.

**V3 is built (2026-08-30): threads, preemption, signals.**

A thread is a control block with `%rsp` and `%rip` set, and a context
switch is choosing a different index — which is the sentence this design
made about threads, now a `clone3` row. `futex` has both halves, `exit`
ends a thread where `exit_group` ends the process, and the word
`set_tid_address` named is cleared and woken on the way out, which is the
whole of how `pthread_join` returns. `fork` is refused by name rather than
deferred: a second address space is a thing this machine does not have.

Preemption is the quantum, and it is what section 7 promised. Two threads
spinning on words the other writes finish, which a scheduler that switches
only at syscalls cannot manage; and the same container racing on an
unlocked counter produces the *same total twice*, which no real machine
can offer, because there the switch is a wall clock. Deterministic and
preemptive at once.

Signals collapse into the loop exactly as section 7 said they would.
Delivery is at block boundaries — strictly finer than Linux's
return-to-userspace precision — and the frame is Linux's `rt_sigframe`
byte for byte, because the guest reads it: `sigreturn` is glibc's own code
reading back what the kernel wrote. What is left of M10 is its
dispositions table and its routing rules. There is no reserved table slot,
no resume-body signature, no splice-versus-call rule and no chain surgery,
because a handler is a program counter.

**And a fault is a signal a handler can catch**, which
`container-plan.md` documents as impossible on the other path. A null
dereference there reads whatever is at address zero and carries on. Here
the address space refused the access, the loop turns the refusal into
`SIGSEGV` with a faithful `si_addr` and an `si_code` that says which kind
of refusal it was, and the program counter stays on the faulting
instruction so a handler that fixes the mapping and returns re-runs it.
A stack overflow is caught on the alternate stack — the case
`sigaltstack` exists for, and one that could not arise at all until the
address space had page permissions.

CPython runs four threads under a lock and gets the right total, and
`signal.signal` with `os.kill` reaches its handler.

What does *not* exist yet, and so is not claimed: tier 1, the timed futex
wait (a timeout needs a clock to expire against, and it is a named fault),
and the strace diff against a native run that V2's acceptance asks for —
the trace is wired through the loop, but nothing has diffed it. G3 is
untouched.

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
- **Determinism must be defended, not assumed — and it must be
  tier-invariant.** The quantum makes scheduling deterministic only if
  retired counts are — which bans host-observable time from leaking into
  the loop's decisions anywhere outside `/iso`. And it makes the schedule
  reproducible across *tiering states* only if every tier preempts at the
  same points: a compiled trace that checks the quantum only at its end
  lets a quantum expire mid-trace and switch threads at a different
  retirement point than the interpreter would — same tape, different
  interleaving, record/replay broken the first time a warmed run tiers
  differently from a cold one. The rule: **preemption points are per
  source block in every tier**, and a compiled block's retired-count
  contribution is a compile-time constant it charges as it goes — one
  decrement-and-branch per block, the cost the transpiled-path poll
  design already priced. The test: same tape, tier 1 off and forced on,
  bit-identical interleaving. (This is also what makes Q7's `rdtsc`
  answer free — section 15.)
- ~~Open: what the engine is called.~~ **Resolved, 2026-08-30:
  `targumannu`, crate directory `targum/`** — see Q13 in section 15 for
  the reasoning.
- ~~Open: the wasm-EH dependency in mixed builds.~~ **Resolved,
  2026-08-30: one build, floor-only, no EH.** What settles it is Q2's
  answer in section 15 — translations are caches: tier-2 output arrives
  as an install-contract companion, and the install contract already
  forbids EH (section 8), so no engine build ever carries it. The
  consequence is binding on tier 2: a companion cannot ship today's
  structured-body-plus-resume shape, whose blocking needs a throw the
  engine no longer catches; it ships loop-compatible bodies — enterable
  at post-call blocks, yielding at suspension points — which is the
  dispatcher/resume shape the transpiler already emits, re-pointed at
  the loop instead of at a catch. The welded single-file container with
  EH remains what the standing AOT path builds; it is not this design's
  mixed build.

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
11. **A flag that is *preserved* must be read from the record being
    replaced, not from the materialized bits.** `inc` and `dec` keep the
    carry; the carry they keep is the one the lazy record would have
    computed, and the bits left over from some earlier materialization are
    only accidentally the same. Getting this wrong is correct whenever
    nothing lazy happened in between, which is most of the time. The
    lockstep oracle found it at the ninth instruction of the first probe
    it ever ran.
12. **An undefined flag stays in the register.** Masking a comparison at
    the instruction that left a flag undefined is not enough — the bit
    persists, and the divergence surfaces at the next instruction that
    writes no flags at all and therefore looks innocent. The mask has to
    be carried until something defines the bit again.
13. **A single-stepped machine has its trap flag set, and the guest can
    see it.** `ptrace` hides it from the register block but not from the
    guest's own `pushf`, so an oracle that does not carry the bit on both
    sides reports a divergence that is the debugger's, not the engine's.
14. **A per-operation oracle and a per-sequence oracle find different
    bugs.** The x87 crate's hardware oracle drives each operation from a
    fresh state, so every defect that is about what an operation *leaves
    behind* for the next one is invisible to it: a condition code not
    written, a flag not suppressed, register data erased where hardware
    only marks it empty. Three of the four x87 defects found on 2026-08-30
    were of exactly that shape. Neither oracle replaces the other.


## 15. Open questions from the build

Section 13 lists the risks the *design* carries. These are the questions
building V1 raised — decisions that have to be made before the seam is
written, and choices already made that want confirming rather than
discovering later. Each says what turns on it. **All thirteen are now
answered (2026-08-30).** The original recommendations are kept where the
answer says more than they did, so what was proposed and what was decided
stay separately readable, and an answer that changes text elsewhere in
this document says where.

### The seam

**Q1. How does the kernel reach thread state?** This is the decision the
rest of the seam waits on. `Kernel<S, M>` owns its `M: Machine` by value
and lives in a static; `Machine`'s accessors are undefined symbols the
seam object resolves to wasm globals. The interpreter's state is a `Tcb`
owned by the engine, and section 6 says the call is
`kernel.dispatch(&mut tcb)`. Three arrangements:

1. **Pass it.** `dispatch` takes `&mut Tcb`, and `Machine` becomes a trait
   whose methods take one. Cleanest, matches section 6 — and touches every
   kisal call site, including the AOT path, which has no `Tcb` to pass.
2. **Point at it.** The `Machine` impl holds a raw pointer to the current
   `Tcb`, which the loop swaps before each dispatch. kisal's signatures do
   not move at all. It reintroduces exactly the aliasing question the
   `BORROWED` flag exists to turn into a panic.
3. **Own it.** The `Tcb` lives in the kernel — which section 3 already says
   it should ("the TCB, owned by kisal as M7 always intended") — and the
   engine borrows it for the duration of a quantum.

Recommendation: **(3)**, because the design already says the kernel owns
the control block and because it is the arrangement that survives more than
one thread without a second mechanism. (2) is the tempting one and should
be refused: a raw pointer swapped by the loop is a correct-today,
silent-tomorrow arrangement of precisely the kind this project keeps paying
for.

**Answered: (3), taken all the way.** The engine is already halfway
there — its own header says the loop "stops and hands the caller a
`&mut Tcb`", so the dispatch inversion exists and only ownership is
misplaced. The concrete shape: the kernel owns *all* machine state — a
process is `{ space, cache, threads: Vec<Tcb> }` in kisal — and the
engine becomes pure mechanism:
`Engine::run(&mut Tcb, &mut Space, &mut BlockCache, quantum)`, shedding
its `tcb` field. That dissolves the (1)-versus-(3) distinction — the
kernel owns *and* the seam call passes `&mut`, both at once,
borrow-checked — and it settles `Space`'s ownership in the same move,
which Q3 needs anyway: the `mmap` rows and the kernel's guest-memory
writes want `&mut Space` from the same owner the scheduler is. The
`Machine` trait's methods get their VM implementation over the kernel's
own state; the AOT accessor implementation is untouched. (2) is refused,
for the recommendation's reason.

**Built, 2026-08-30.** `Engine` holds nothing: `Engine::run(&mut Tcb,
&mut Space, &mut BlockCache, quantum)`, and the kernel owns the pieces —
`Kernel` has a `pages` field, which is the page table. kisal now depends
on targum, which is the direction the ownership decides. The lockstep
harness plays the kernel's part and owns the three itself, which is a
small piece of evidence that the split is the natural one.

**Q2. Does tier 2 keep the globals, or does everything move to the TCB?**
Downstream of Q1. If the transpiler survives, there are two
representations of machine state and the 412-byte image is the bridge
(pitfall 6). The question is whether `Machine` gets implemented twice — the
globals for tier 2, the `Tcb` for tiers 0 and 1 — or whether tier 2 also
reads a `Tcb` with the globals demoted to a per-call cache. Two impls is
less work now and one more place for the two engines to drift. Not
recommended either way here: it is a question about how long tier 2 lives,
which section 9 deliberately leaves open.

**Answered: the TCB is the only home in the VM build, and the two-impl
question is moot.** What settles it is a delivery decision taken since
this question was written (2026-08-30): **translations are caches, not
artifact content.** The canonical artifact is engine + image; all
acceleration — runtime tier 1 and baked tier-2 output alike — arrives as
a companion module through the one `install` door, per host, never
welded into the deliverable. So the register globals are a convention of
the *AOT deliverable* only: the VM build never links the seam accessor
object and never defines them, and `Machine` is implemented once per
world, not twice in one. A companion body's storage class is "locals
inside, TCB at the seams" — the promotion machinery re-pointed from
`global.get`/`global.set` to loads and stores at `tcb_ptr + offset`, a
mechanical third storage class beside the two that exist. Pitfall 6's
412-byte bridge governs only an artifact where both representations
coexist, and under the caches model none does. This same decision
resolves section 13's EH question; the resolution is recorded there.

**Q3. How does kisal reach guest memory?** `kisal/src/memory.rs` turns a
guest address into a pointer and dereferences it. Under the VM that is
unsound in two directions: a `read(2)` landing on a page some cached block
was decoded from has to hit the invalidation hook, and a write to a page
the guest mapped read-only has to fault rather than succeed. Every kernel
write has to pass through `Space`. The question is the mechanics, because
`GuestMemory` is `Copy`, is constructed ad hoc in `file.rs` and `write.rs`,
and threading a `&mut Space` through all of that is a wide change:

1. Thread the borrow, and let the compiler enumerate the sites.
2. Make `Space` a process-global the helpers reach, matching kisal's
   existing one-instance-one-actor arrangement.

Recommendation: **(1)**, on the strength of pitfall 3 — "a new writer
bypassing it should be structurally hard, not merely discouraged". A global
makes the hook available; a borrow makes it unavoidable. The width of the
change is the price of that, and it is a one-time price.

**Answered: (1) — and further: `GuestMemory` is retired in the VM
build.** `space.rs` already states the invariant — "nothing outside this
module turns a guest address into a pointer" — and `kisal/src/memory.rs`
is precisely the second door that sentence forbids. Rows take
`&mut Space`, which is natural under Q1's answer because it comes from
the same owner, and the one genuinely new decision is the fault mapping:
the same `Fault` serves both consumers — at a syscall row it becomes
`Errno::Fault`, an `EFAULT` to one call, exactly what `GuestMemory`
answers today; at the loop it becomes a SIGSEGV. The null-page rule
folds in for free: page zero is never mapped, so the special case
becomes the general one. The width of the change is the price of the
closed writer set, paid once.

**Built, 2026-08-30**, and it turned out to have a second half nobody had
named. `GuestMemory` is retired: what is left is an adapter with no way to
reach memory the address space does not, split in two along the line that
matters — `GuestReader` for rows that only read, `GuestMemory` for rows
that write. The split is not tidiness; it means "which rows write guest
memory" is a question `grep` answers.

The second half: **there are two kinds of kernel write, and Linux
distinguishes them too.** A write *on the guest's behalf* — `read(2)`
filling a user buffer, a signal frame — is a user access and answers to
the guest's protections, so a read-only destination is `EFAULT`.
*Populating* memory the kernel is in the middle of handing over is not a
user access at all: zero-filling a fresh mapping, copying a file's bytes
into a mapping the guest asked to be read-only, loading a program's text
into a read-execute segment. A real kernel does those through the direct
map, where the process's page table has no say. Conflating them makes
`mmap` of a read-only file fail with `EFAULT` — which is how this was
found, eight tests at once. So `Space::place`/`place_fill` are the
kernel's own write: they check that the range is inside linear memory,
they skip the permission test, and they **still take the invalidation
hook**, which is the whole reason they live in the address space instead
of being a raw pointer at three call sites.

The mapping rows now write both structures: the VMA tree says what a
mapping *is*, the page table is what every access tests, and `mmap`,
`mprotect` and `munmap` update them together. `munmap` drops the decoded
blocks with them, which is what makes unmap-then-remap safe.

One thing the rewire had to give the native tests: they used to hand the
kernel the host address of a stack array. The page table refuses that
correctly — a host pointer is nowhere near the four gigabytes the machine
has — so a test that hands over a pointer now allocates it low, where a
guest's memory really is (`GuestBytes`, `GuestBuffer`, and the two test
arenas). 178 kisal tests pass over the new path.

**Q4. What does `Machine::grow` do natively?** Inside the module it is
`memory.grow`. Natively the arena is a reservation, so growth is commitment
rather than allocation. Reserve the whole four gigabytes up front with
`MAP_NORESERVE` and let `grow` only move the limit, or commit in steps?
Recommendation: **reserve up front** — it makes the native and module paths
differ in one line instead of in a policy, and an untouched reservation
costs nothing but address space this process has in abundance.

**Answered: reserve up front — `PROT_NONE`, committed on `grow`.** The
whole four gigabytes reserved at start, with `set_limit` committing
`[0, limit)` to readable-writable. Host page protection does not mirror
guest protections — the bitmaps are the guest's truth — but keeping the
*uncommitted* tail `PROT_NONE` means an engine defect that touches past
`limit` natively faults instead of working on the host and trapping in
the module, and it makes the identity `Space::pointer` sound for every
address the harness can ever hand it.

**Built, 2026-08-30.** The reservation is process-wide and taken once;
arenas are commits inside it rather than mappings beside it, and dropping
one puts the range back to `PROT_NONE` so a pointer that outlived it
faults instead of finding the next test's memory. `Space::set_limit` now
refuses a limit past the ceiling, which caught a test double whose
"unbounded" default would have asked the page table for a bitmap covering
half a petabyte.

### The address space

**Q5. Is "everything is a mapping" the right default?** `Space` starts with
nothing mapped and every access faulting, which is what makes a null
dereference a real `SIGSEGV` (section 5). The consequence is that the boot
path must map every region before anything runs, and that the engine's own
data — kisal's heap, the image blob, Rust statics — is *not* mapped from
the guest's point of view, so a wild guest pointer into it faults. That is
the correct answer, and it means kisal's writes to its own heap must not go
through `Space` while its writes to guest memory must. Worth confirming
that split is wanted before it is built into a hundred call sites.

**Answered: confirmed — and the split has a name, which is the reason to
want it.** This is guest/kernel isolation, the thing `container-plan.md`
states the AOT design cannot have ("no isolation between guest and
kernel … everything inside is one trust domain", named once and
accepted). The nothing-mapped default reverses that cost: kisal's heap,
the image blob and the Rust statics are absent from the guest's page
maps, so a wild guest pointer into kernel state *faults* instead of
corrupting the kernel silently. The split — kernel writes to its own
heap bypass `Space`, writes to guest memory cannot — is enforced by
Q3's types, and it is one of the strongest upgrades the VM buys. Build
it into the hundred call sites.

**Q6. Four bitmaps, or one packed one?** Readable, writable, executable and
code are four bits per 4 KiB page — 512 KiB in total for a full four
gigabytes, grown as linear memory grows. Cheap enough not to think about,
and the four separate maps keep the hot path a single load and test. A
packed two-bits-per-page alternative halves it at the cost of a shift on
every access. Recommendation: **leave it**, and revisit only if a
measurement says the working set matters.

There is a consequence worth writing down now: the maps are dense and
indexed by absolute page number, so the native lockstep harness is bounded
to the low four gigabytes. That is free today, because the harness moves
`%rsp` into the corpus program's own static stack and every address the
compared region touches is in its image. It becomes a wall the day the
oracle wants to compare a position-independent binary at the address the
kernel actually loaded it at, and the fix then is a sparse map for the
native build only.

**Answered: leave it, as recommended.** 512 KiB dense is noise and the
single load-and-test on the hot path is not. Q4's up-front reservation
keeps the low-address arrangement coherent, and the recorded wall — a
position-independent binary compared at the address the kernel really
loaded it — has its fix bounded and native-only when the day comes.

### Fidelity choices already made

These are built. Each is a defensible reading of the design that somebody
else might have read differently, so they are named rather than left to be
discovered by a divergence.

**Q7. `rdtsc` answers `retired × step`.** Section 4 says the counter is
"the deterministic time base (`rdtsc` answers a function of it)", so the
interpreter multiplies the retired count by the transpiler's odd step. The
transpiler instead advances a cell by that step *per read*. Both are
deterministic; they are not the same sequence, and the tier-divergence test
section 13 asks for would flag it on the first program that reads the
counter twice. One of them has to become the other. Recommendation: **the
interpreter's**, because a counter derived from execution is monotone
across threads for free, where a read-counting cell has to be told about
them — but this is a change to the transpiler and so is not one to make
quietly.

**Answered: the interpreter's semantics are the specification — and the
transpiler change rides work it owes anyway.** The piece that makes this
no longer a quiet unilateral edit: tier-invariant determinism (section
13's entry, now stated there as a rule) requires compiled code to charge
retired counts per source block — otherwise a quantum expires at
different retirement points warmed versus cold, and record/replay breaks
across tiering states. Once compiled code carries that per-block counter
for preemption, `rdtsc` as a function of retired count falls out of the
same counter in both engines. Until a build mixes engines, the AOT
deliverable's per-read cell stays and no divergence is observable — the
cross-engine test only exists where both engines coexist, which is
exactly when the counter arrives.

**Q8. Undefined flags are left unspecified, and the oracle masks them.**
The architecture says nothing about them and the two vendors differ, so
pinning them would be pinning *this* machine. The oracle carries a poison
mask (pitfall 12) rather than comparing them. The open half: a guest that
depends on the AMD behaviour would diverge from us and from an Intel host
alike, and we would not find out. Is that acceptable, or does the engine
eventually want to match measured hardware for the undefined bits, and
whose? No recommendation — it is a question about what "faithful" is for.

**Answered: unspecified, deterministic, masked — permanently, with a
named trigger.** "Faithful" here is faithful to the *architecture*, and
the architecture says nothing; the house precedent is the x87
transcendentals, which deliberately match neither vendor because there
is no "the hardware" to match, and measure instead of pin. A guest
branching on an AMD-undefined bit is broken on half the real machines it
could run on — matching a die would pin this engine to a vendor forever
in exchange for conformance nothing conforming needs. Two obligations
survive: the engine's actual values stay deterministic (they are — the
lazy record computes them), and the poison mask stays scrupulous
(pitfall 12). The trigger for reopening is the standard one: a real
binary observed depending on an undefined bit — then both vendors get
measured and the decision is made from a case.

**Q9. A repeated string instruction retires one iteration at a time.**
`rep movsb` leaves `%rip` where it is until the count runs out, so a
preemption or a signal can land inside a `memcpy` exactly as it does on
hardware, and single-stepping matches `PTRACE_SINGLESTEP` iteration for
iteration. The price is a dispatch per iteration rather than per
instruction, which on a large copy is the difference between one loop and
one loop plus a block-cache position check. Recommendation: **keep it** —
the alternative makes a signal undeliverable inside a gigabyte copy, which
is a real program's real behaviour — but the price is real and belongs in
the G2 measurement rather than being discovered inside it.

**Answered: keep it, as recommended — and answering it found a defect
next to it.** The run loop's staying-put arm (`rip` unchanged, the
repeated-instruction case) `continue`s past the dirty-code check at the
bottom of the inner loop. A `rep movsb` whose destination overwrites
*its own instruction bytes* therefore keeps executing the cached decode
for the rest of the count — the exact stale execution `space.rs`'s
header promises impossible, where hardware executes the new bytes at the
next iteration boundary. The fix is one line — the staying-put path
checks `has_dirty_code()` too and breaks to the drain, whose re-fetch
decodes the freshly written bytes at `rip` — plus the SMC corpus case
that pins it: a rep store landing on its own page. Named here rather
than fixed silently; open until the fix and its test land.

**Fixed, 2026-08-30.** The check moved above the program-counter arms, so
it runs whatever `%rip` did. Pinned by a unit test whose guest turns its
own `rep stosb` into a `ret` with the first byte it stores: correct
behaviour is that exactly one iteration retires and the re-fetch runs the
`ret`. The test was checked against the defect — with the check back in
its old place the count runs to zero, on a hundred iterations of bytes
that no longer exist.

### Scope and sequencing

**Q10. What is a "corpus program" in V1's acceptance?** V1 asks that "every
existing corpus program runs interpreted with output byte-identical to
native". The lockstep oracle runs corpus *functions*, entered on a
synthetic frame with the harness choosing the registers; running a
*program* needs the seam, which is V2's first item. So either V1 closes on
the oracle plus the function corpus and the ladder is entirely V2's, or V1
does not close until the seam exists and the boundary between the two
milestones moves. Recommendation: **the former**, with the acceptance line
in section 12 corrected to say "function" — the oracle is a stronger check
than the output comparison it replaces, and pretending the milestone is
still open is worse than moving the line and saying so.

**Answered: the former.** V1 closes on the oracle plus the function
corpus; the ladder is V2's entirely. Section 12's acceptance line is
corrected in this edit, with the correction recorded there. The oracle
compares the whole register file at every retirement, which is strictly
stronger evidence than the output comparison the old line asked for —
holding the milestone open on the weaker phrasing would teach the wrong
lesson.

**Q11. When is block chaining built?** Section 4 says "taken when
measured", and the cache today probes a map once per block transition,
never per instruction (pitfall 1 is satisfied). The measurement that would
justify chaining is G2 — which needs the seam, which is V2. So chaining
cannot honestly be decided before V2 even though it is a tier-0 concern.
Recommendation: **leave it unbuilt** and let G2 say.

**Answered: leave it unbuilt; G2 decides.** Pitfall 1 is satisfied as
built — the map is probed once per block transition, never per
instruction. One thing can run before the seam exists: a **G2a** on the
original spike, adding the lazy-flag record and the bitmap tests — the
two known per-instruction taxes — and re-measuring. Hours, and it
brackets section 11's discount without touching the chaining question,
which stays behind real G2.

**Q12. MMX and `fxsave`.** Both are on the x87 crate's own roadmap as "not
yet", and both are reachable from a real libc — `fxsave` in particular sits
on the signal-frame path. Do they get built during V2's breadth grind, or
does the engine refuse them loudly and the ladder route around until
something actually needs them? Recommendation: **refuse loudly and wait**,
which is the policy everywhere else here, with the note that the sigframe
work in V3 is the moment `fxsave` stops being optional.

**Answered: refuse loudly and wait — through one tracker, not two.** The
x87 crate's tier table is already the ledger for `fxsave` (X7c) and MMX
(X7d), so the engine's refusals name those rows rather than opening a
VM-side list that can drift from it. The recommendation's note stands
and is the schedule: V3's sigframe render is `fxsave`'s first real
consumer on any path — the `_dl_runtime_resolve` trio stores pointers it
never calls under `DF_1_NOW`.

**Q13. What is the engine called?** Section 13 already asks this and it is
still open; it is repeated here because the crate now exists as `vm/` and
every day it stays there is a day the placeholder gets harder to move.

**Answered: `targumannu`, crate directory `targum/`.** The Akkadian for
interpreter — and, through Aramaic and Arabic *tarjumān*, the ancestor
of English "dragoman": the professional who stands between two tongues
and renders one in the other, which is this crate's whole function. It
keeps the house register (*zaqāru*, *kisallu*) and follows the house
shortening (kisallu → kisal, targumannu → targum). The rename belongs in
the commit that lands V1, while `vm/` is still untracked and the
placeholder is still cheap to move. Section 13's naming entry is
resolved by this answer.

**Renamed, 2026-08-30**, while it was still untracked and free.
