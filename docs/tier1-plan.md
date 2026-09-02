# Tier 1: hot blocks compiled to wasm at run time

Status: **design, 2026-09-02** — written for discussion before anything
is built. Step 2 of the plan in [performance.md](performance.md) §7;
section 8 of [vm.md](vm.md) is the sketch this fills in, and where this
document disagrees with that sketch, this one is current.

## The short version

The container is an x86-64 interpreter compiled to wasm, running the
guest's instructions one at a time. That is the correctness floor: it
executes anything, and it is why the Django demo works at all. It is
also why a request costs 20 ms where native costs a third of one, and why
booting costs thirty seconds — the interpreter does about a hundred
million guest instructions a second, and a boot is three billion of them.

Tier 1 is the standard way virtual machines of this shape close most of
that gap. The interpreter already decodes the guest's code once into
cached blocks and counts how often each is entered. Tier 1 takes the
blocks that turn out to be hot, translates them into wasm functions
*while the container runs*, and hands the bytes to the host through one
new import; the host compiles them with its own engine and puts the
result where the interpreter can call it. From then on, hot code runs as
machine code the host's compiler produced, with guest registers in real
registers and guest branches as real branches. Cold code, rare
instructions, faults and everything strange stay in the interpreter,
which remains the only place correctness lives.

Two facts make this cheaper than it sounds. **Everything the interpreter
would need to know is already computed.** The block cache knows where
blocks start and end and which pages they came from; the pre-decoded
form (`targum/src/quick.rs`) has already resolved every operand of the
95% of instructions it understands; the lazy-flags record already says
what a conditional branch depends on. A compiled block is those same
structures written out as wasm instead of walked by a loop. And **guest
state never leaves memory.** A thread's whole machine is a struct in
linear memory; a compiled block reads it on entry and writes it back on
exit, so the interpreter and the compiled code can hand off to each other
at any instruction boundary with nothing translated, and anything the
compiled code does not want to handle it simply hands back.

What to expect: systems of this shape land five to twenty times off the
interpreter on integer code, which is a boot of a few seconds and a
request of a few milliseconds. The ceiling is measured rather than
guessed — this project's own ahead-of-time translator reached parity
with clang's wasm backend on integer and memory kernels, about a hundred
times above where the interpreter sits — and a run-time translator that
works block by block will land well short of it. Section 10 says how the
number will be taken, and section 12 what could stop it.

## Outline

1. **What a compiled trace is** — the unit, its signature, its exits.
2. **The seam with the run loop** — the block cache and `Engine::run`.
3. **The host contract** — one import, both hosts.
4. **Semantics: the `Quick` lowering, compiled** — and the helper for
   everything else.
5. **Registers, flags and memory inside a trace.**
6. **Correctness: exits to the interpreter** — faults, hard cases, and
   why traces model none of them.
7. **Invalidation** — the block cache's, shared.
8. **Regions** — many blocks in one function, which is where the speed is.
9. **Selection and batching.**
10. **Determinism, preemption, threads and signals.**
11. **Where the code lives.**
12. **Gates and milestones** T0–T3, with acceptance and negative
    controls.
13. **Risks and open questions.**
14. **Pitfalls index** — seeded.

## 1. What a compiled trace is

The unit is the extended basic block the cache already holds: a run of
instructions from one guest address through conditional branches to the
first unconditional transfer, call, return or `syscall` (`block.rs`,
`terminator`). A trace is one such block, compiled, entered at its entry
address and nowhere else — the same rule the cache has, and for the same
reason: a jump into the middle of a block is a second block, never a
special case.

Its wasm signature:

```
(tcb: i32, vitals: i32, budget: i64) -> i64
```

`tcb` is the address of the running thread's `Tcb` in linear memory.
`vitals` is the address of a small fixed-layout header the engine
publishes for the current address space: the base addresses of the
readable, writable and code bitmaps, the memory limit, and the
dirty-code flag — everything a memory access has to consult, at known
offsets, so a trace is not compiled against one process's pointers.
`budget` is how many instructions may still retire in this quantum.

The result packs an exit kind into the high bits and a guest address into
the low 32 (a guest address is below 4 GiB by construction, the wasm32
ceiling):

| kind | meaning | `rip` returned |
| --- | --- | --- |
| `Continue` | the trace ran to an exit and this is where execution goes | the next instruction to execute |
| `Syscall` | the block ended in `syscall`; `%rcx` and `%r11` are set as hardware leaves them | past the `syscall` |
| `Interpret` | execute this one instruction in the interpreter, then carry on normally | the instruction |

On every exit the `Tcb` is complete: registers, `rip`, the flags record
and `retired` are what the interpreter would have left. That is the whole
of the interface, and it is what lets the loop treat a trace as one very
long instruction.

## 2. The seam with the run loop

`Block` gains two fields: a hit counter, and an optional compiled entry
(a table index). `Engine::run` already looks the block up on every entry
through the direct-mapped `recent` cache, so the check costs one branch:

```
let block = cache.block(index);
if let Some(entry) = block.compiled {
    let exit = call(entry, tcb, vitals, budget);
    budget -= retired_since;           // read back off the Tcb
    match kind(exit) {
        Continue  => { tcb.rip = rip(exit); continue }
        Syscall   => return Outcome::Syscall,
        Interpret => { run one instruction through the existing path
                       at rip(exit), without consulting `compiled`;
                       continue }
    }
}
block.hits += 1;
...the interpreter, as today
```

Inside the module, calling a table index is a Rust function pointer: on
wasm32 a `fn` pointer *is* a table index, so `call` is a transmute and an
ordinary call, no glue. The host is what put the function there —
section 3.

`Interpret` is the one exit that needs a rule: the instruction it names
is executed by the interpreter *once*, through `Cpu::run`, and only then
does the loop go back to consulting `compiled`. Without that rule a trace
that exits at its own entry would be re-entered forever. The interpreter
path for one instruction is exactly `Engine::run`'s existing body with
the compiled check skipped, so nothing is duplicated.

## 3. The host contract

One optional import, on the module's `env`:

```
targum_install(bytes: i32, length: i32) -> i32
```

The engine builds a complete wasm module in its own memory — section 11
says with what — and calls this. The host compiles and instantiates it,
sharing the engine's linear memory, grows the engine's exported
`__indirect_function_table` by the module's function count, stores the
new instance's exported functions into the new slots in order, and
answers the first slot. `-1` means the module was refused; the engine
logs it and leaves those blocks interpreted. Refusal is never a
correctness event, because the interpreter is always there.

Under wasmtime (`runner/src/lib.rs`), that is `Module::new` on the
bytes, a `Linker` with the engine instance's `memory` export defined as
the trace module's `env.memory` import, `Instance::new`, `Table::grow`,
`Table::set` per export. The runner already grows and fills the table
for `install_continuation`, so the shape is there. In a browser it is
`WebAssembly.instantiate` with `{ env: { memory } }` and a loop over
`table.set`. A host without the import — the AOT path, or a host that
chooses not to offer it — leaves tier 1 off, and the container runs on
the floor. Nothing about the container's other contract (the two
`ll-store` imports) changes.

The trace module imports one memory and nothing else. It has no table of
its own, no globals, no data: every function in it is a leaf that reads
and writes the guest's memory and calls, at most, the engine's exported
helpers by index through the shared table — section 4. Instantiation is
therefore cheap in everything but the host's compile time, which is why
section 9 batches.

## 4. Semantics: the `Quick` lowering, compiled

The source of truth for what an instruction does is `Cpu::quick` in
`targum/src/exec.rs`, over the pre-decoded `Quick`. Every `Op` it handles
is a handful of wasm instructions:

| `Op` | compiled to |
| --- | --- |
| `Mov`, `Lea`, `Widen`, `WidenSigned` | a load or local read, an optional extend, a store or local write |
| `Add`, `Sub`, `And`, `Or`, `Xor` | the i64 operation, a truncation to width, the write-back, and a flags record (§5) |
| `Cmp`, `Test` | the same without the write-back |
| `Push`, `Pop` | a store or load at `%rsp` and a local update |
| `Nop` | nothing |
| `Jcc` | a condition (§5) and a branch: taken to a constant target, not taken to the next instruction |
| `Jmp`, `Call`, `Ret` | a constant or loaded target; `Call` pushes the return address; `Ret` pops one |

Operands are already resolved: `Source::Register(slice)` is a local with
the slice's width and high-byte rule applied, `Source::Immediate` is a
constant, `Address::Fixed` is a constant and `Address::Computed` is the
same base-plus-index-times-scale arithmetic the interpreter does, with
the 32-bit wrap and the `%fs` base where `quick.rs` says so.

**What `quick.rs` declines, the trace does not compile.** `Op::General`
is about 5% of a real run — `imul`, shifts, `cmov`, `setcc`, the string
instructions, the SSE moves. For one of those the trace emits a call to
an exported engine helper:

```
targum_step(tcb: i32, block: i32, position: i32) -> i32
```

which runs `Cpu::step` for that one instruction against the cached
decode and answers whether it retired normally. Before the call the
trace writes its locals and flags back; after it, it reloads them. The
call is not cheap — a full write-back and reload — but it is paid on 5%
of instructions and it keeps traces long through the one `shl` in a
hashing loop. A helper that answers anything but "retired, fell through"
— a trap, a `rep` that stayed put, a branch the general path took —
becomes a `Continue` exit at the address the `Tcb` now holds, and the
interpreter takes it from there.

The refinement this sets up is the obvious one: every op lowered into
`quick.rs` later gets both tiers faster, and the mnemonic histogram says
which one is next. Nothing about tier 1 has to be revisited to add an
op.

## 5. Registers, flags and memory inside a trace

**Registers are locals.** The sixteen general registers and the `%fs`
base are loaded from the `Tcb` into `i64` locals on entry and stored on
exit. Which of the sixteen a trace touches is known at compile time, so
only those move; the AOT tier's promotion work is the record of how much
this is worth — it took the integer kernel from 5.1× off clang to
parity, and the same flush discipline applies. Byte, word and dword
slices are masks and shifts on the local, with the dword write zeroing
the top as `Tcb::write_register` does; one rule, stated once, in one
place per tier.

**Flags are lazy at compile time too.** The interpreter records the last
flag-writing operation (`Flags::record`: rule, width, left, right,
result) and computes a condition on demand (`Condition::holds`). A trace
keeps that record in locals, and when a `Jcc` follows the `Cmp` that
feeds it within the same block — which the histogram says is the common
shape, `Cmp` and `Test` at 14% of the stream and the conditional jumps at
16% — the compiler knows the rule statically and emits the direct wasm
comparison: `Below` after a `Cmp` is `i64.lt_u`, `Equal` is `i64.eqz` of
the result, and so on for the sixteen conditions and the `Add`, `Sub` and
`Logic` rules. When the rule is not known statically — a conditional jump
at the start of a block, fed from a predecessor — the trace stores the
record and calls `targum_condition(tcb, condition) -> i32`, which is
`Condition::holds` exported. At every exit the record is stored into the
`Tcb`'s `Flags` in the interpreter's own representation, so a `pushf` or
a signal frame built by the interpreter a moment later sees exactly what
it would have.

**Memory is linear memory.** A guest address is a linear-memory offset,
so a load is `i64.load` at the address, wrapped to `i32` *after* the
check that it is below the memory limit. The check is the interpreter's
(`Space::permitted`): the page's bit in the readable or writable bitmap,
found through `vitals`, and for a store the code bitmap as well. About
five wasm instructions per access. A failed check is an exit — section 6
— and a store to a page whose code bit is set calls
`targum_code_write(vitals, address, length)`, which queues the page
exactly as `Space::note_code_write` does, and then exits, because the
next instruction might be the one just overwritten. The interpreter's
`rep stosb` test, where a repeated store replaces its own instruction
after one iteration, passes unchanged under a trace because the trace
never gets past the store.

Whether the inline checks are affordable is a measurement, and section
10 takes it. They are not optional: they are what makes a wild pointer a
`SIGSEGV` with a faithful address rather than a silent read of somebody
else's bytes, which the design document lists as the fidelity class the
AOT tier could not have.

## 6. Correctness: exits to the interpreter

A trace models no fault, no trap and no strange case. Whatever it cannot
fully handle at an instruction, it handles by *not executing that
instruction*: it writes its locals and flags back, sets `rip` to the
instruction, and returns `Interpret`. The interpreter then executes it
and produces whatever it produces — a `SIGSEGV` with the right access
kind and address, an undefined-instruction trap, a `Trap::Unsupported`
naming the bytes — with `rip` and `retired` exactly where the
architecture says they are, because the interpreter's own rules put them
there. The trace contributed nothing to the diagnosis and could not have
got it wrong.

This is also why the permission check needs no fault path of its own.
The check compiled into the trace is the same check `Space::load` makes;
if it fails in the trace it will fail in the interpreter a moment later,
which is where the `Fault` gets its `si_addr`. And it is why the
`Interpret` rule of section 2 exists: the instruction is run exactly
once, interpreted, before the loop goes back to trusting compiled code.

The consequence for the differential suite is the acceptance criterion
of milestone T1: **every program the suite runs must produce identical
output, syscall trace and exit status with tier 1 forced on for every
block**, including the programs that fault on purpose, the ones that
write code and run it, and the ones that are preempted mid-`rep`.

## 7. Invalidation

A compiled trace is registered against the same pages as the block it
came from — it *is* that block, with a compiled body. `invalidate_page`
drops the block; the compiled entry goes with it, and nothing can reach
the function any more, because the only path to it is through the block
the loop just looked up. No generation stamp is needed: the `recent`
cache validates a hit against the block it names, as it does today, and
a freed block's slab index is a miss.

A region (section 8) is registered against the pages of every block in
it, so a write to any of them drops the whole region. That is coarser
than necessary and it is the right first answer: the alternative is a
region that survives with a hole in it, which is the shape of bug the
block cache's comment warns against reintroducing.

What cannot be undone is the host side: a table slot cannot be unmapped
and an instance cannot be dropped from inside the module. An invalidated
region leaks its instance until the container ends. Version one accepts
that and counts it — the diagnostic line at exit gains "regions
compiled, regions dropped, instances live" — and a host that recycles
slots and instances is a later refinement of the runner, not of the
engine.

## 8. Regions

A single compiled block is a proof of the seam, not a speed-up. Blocks
average five instructions, and calling a function that loads a dozen
locals, runs five instructions and stores the locals back is not faster
than interpreting them. The speed is in **regions**: a set of hot blocks
connected by constant-target branches, compiled into one wasm function.

A region has one entry function whose body is a `br_table` on the guest
address it was entered at, dispatching into the block bodies laid out
inside a single `loop`. Within the region:

- a conditional or unconditional branch to a block in the same region is
  a `br` to that block's label — no exit, no write-back, no lookup;
- a branch to an address outside the region is an exit with `Continue`;
- guest registers live in locals for the whole region and are written
  back only at exits and helper calls;
- the flags record lives in locals, and a `Jcc` whose feeding `Cmp` is
  in the same block folds to a direct comparison as in section 5;
- a hot loop becomes a real wasm loop, which Cranelift allocates
  registers across — the shape the AOT tier reached clang parity with,
  applied to whatever execution found hot.

Region formation starts at a hot block and follows its constant-target
successors — fall-through, taken branch, near `jmp` and `call` targets —
while they are cached and hot, up to a cap of 64 blocks. `Call` inside a
region pushes the return address and dispatches to the target if it is
in the region; `Ret` pops and exits with `Continue`, because a return
address is not a constant and the loop's lookup is the honest way to
follow it. A region is entered only at addresses that are block entries
inside it, and `compiled` on each of those blocks points at the same
region function with the entry's dispatch index; a block can belong to
one region.

The size cap is not tunable in the sense of "larger is better". Cranelift
does not split a function it cannot allocate registers for, and the
engine's own history — `docs/performance.md` §6 — is a list of things
that got slower when one wasm body grew too large. Sixty-four blocks of
five instructions is a few hundred wasm instructions per block body and
a function Cranelift handles comfortably; the cap moves when a
measurement says so, and not before.

## 9. Selection and batching

A block's hit counter crossing a threshold marks it hot and queues it. A
compile is triggered when the queue holds enough blocks, or when the
oldest queued block has waited long enough in retired instructions, so
that a hot loop discovered late in a long run is not left interpreted
for the rest of it. Regions are formed from the queue, compiled into one
module — many regions, many functions — and installed with one call.

Batching is not tidiness. The expensive step is the host's compile and
instantiate, which is a Cranelift run per module plus an instantiation:
hundreds of microseconds each, and thousands of one-block modules would
be a run spent in the compiler. One module per batch amortises it, and
one instance per batch bounds the leak of section 7.

Thresholds — hot at how many hits, a batch at how many blocks, how long
to wait — are measured in T3, not chosen here. What is decided is the
shape: counting is per block entry in the loop (one increment, on the
interpreted path only), selection is by threshold, and the whole of the
compiler runs inside the module, in the container's own thread, at a
quantum boundary. It is therefore part of the container's deterministic
execution: the same run makes the same compile decisions at the same
retired-instruction counts, which is what section 10 needs.

The Django profile sizes the working set: ninety percent of the run is
about twenty thousand addresses, on the order of four thousand blocks.
A few hundred regions cover it.

## 10. Determinism, preemption, threads and signals

**The interleaving is unchanged.** Scheduling is a pure function of
retired instructions, and record and replay depend on that. A trace must
therefore stop at *exactly* the instruction the interpreter would have
stopped at when the quantum runs out, not merely near it. The rule: at
entry and at every region-internal edge, if the budget is smaller than
the next block's instruction count, exit with `Continue` at that block's
entry instead of running it. The interpreter then finishes the quantum
one instruction at a time and stops where it always stops. Block lengths
are constants at compile time, so this is one compare per block. It
makes the schedule identical whether tier 1 is off, on, or forced on for
every block — and that becomes a test: **the same tape replays, tier 1
off and forced on**, which `vm.md` §12 already asks for at V5.

**Preemption** is that rule. **Threads** are unaffected: a trace is
handed the running thread's `Tcb`, a region belongs to an address space,
and a context switch is a different `Tcb` on the next call. **Signals**
are delivered at block boundaries by kisal before `Engine::run`, and a
compiled region runs at most a quantum before returning, which is the
same latency the interpreter already has. A fault inside a region is an
`Interpret` exit and then the interpreter's fault, delivered as today
with the frame the interpreter builds.

**The instruments stay exact.** The mnemonic histogram and the
guest-address profile count in `Cpu::advance` and `Cpu::run`; compiled
code would not call them. So with either feature on, tier 1 is off, and
a profile is still an exact count of what the guest executed — the
answer does not depend on which tier executed it.

## 11. Where the code lives

- `targum/src/tier1/` — `select.rs` (counters, thresholds, the queue),
  `region.rs` (forming regions from hot blocks), `compile.rs` (a region
  to a wasm function: the `Op` table of section 4, the locals discipline
  of section 5, the exits of section 6), and `install.rs` (the module
  assembly and the import call). All of it is Rust compiled to wasm32,
  running inside the container.
- `emit/` — a new small crate holding what is today `src/emitter/binary.rs`
  and `src/emitter/code.rs`: the section writers and
  `FunctionBodyBuilder`. Pure Rust, no dependencies, and already written;
  the root crate goes on using it through the same paths. The
  relocation and `wasm-ld` linking layer stays where it is, because a
  trace module is complete rather than relocatable.
- Three exported engine helpers: `targum_step`, `targum_condition`,
  `targum_code_write`, thin wrappers over `Cpu::step`, `Condition::holds`
  and `Space::note_code_write`.
- `runner/src/lib.rs` — the `targum_install` import, thirty lines beside
  `install_continuation`.
- `kisal` — untouched. The kernel does not know which tier executed an
  instruction, and that is the design.

The host interpreter (`kisal/examples/interpret`) stays tier 0: there is
no host to install into natively, and the traces would be compiled for a
memory the native engine does not have. It remains the development
instrument for kernel work, and tier 1's own tests run under wasmtime.

## 12. Gates and milestones

**T0 — the host contract.** `targum_install` on the runner; the engine
assembles a module holding one trivial function, installs it, calls it
through the table and reads the answer back. Both hosts: wasmtime, and
the browser harness's ten lines. Acceptance: a container that installs
and calls; a container on a host without the import runs unchanged, and
says so once in the log. Negative control: a deliberately malformed
module is refused and the container completes on the floor.

**T1 — single-block traces.** The `Op` table of section 4 for every
lowered op, `targum_step` for the rest, the locals and flags discipline
of section 5, the exits of section 6, invalidation of section 7, a
`force` setting that compiles every block on first entry. Acceptance:
**the differential suite green with tier 1 forced on**, under wasmtime,
for every program the kernel suite runs — the faults, the self-modifying
code, the preempted `rep`, the forks. Negative controls: a page write
that invalidates a compiled block mid-run, with invalidation disabled
observably failing; a fault inside compiled code reaching its handler
with the same frame the interpreter builds; the same tape replaying with
tier 1 off and forced on.

**T2 — regions.** The dispatcher, internal branches, region-wide locals,
the budget rule of section 10, folded conditions across a block. The
same acceptance, with regions forced. Plus the first speed number: the
nine kernels of `tools/microbench` under wasmtime, tier 1 forced,
against the interpreter, recorded in `performance.md` §2 as a new
column.

**T3 — selection and measurement.** Thresholds and batching, chosen by
measuring the Django container: boot, warm request, four clients, with
the instance count and the compile time reported at exit. Recorded
against the literature band of `vm.md` §11 and the ceiling of the AOT
tier, in `performance.md` §1, honestly, whatever it turns out to be.

## 13. Risks and open questions

- **Compile latency as jitter.** The first request that trips a hot
  threshold pays for a Cranelift run in its own latency. Batching bounds
  the count; a warm-up that compiles the boot's hot set before the first
  request bounds the moment. Whether a request-time compile is visible
  at all is a T3 measurement.
- **The checks per access.** Five wasm instructions on every load and
  store is real, and memory operands are most instructions. If T2's
  kernels say the checks are the ceiling, the answer is a cheaper
  representation of the same fact — one bitmap for "readable and not
  code", say — and never dropping the fact.
- **Coverage.** If `targum_step` turns out to be on a hot path, the
  helper's write-back and reload cost dominates a region. The histogram
  says which op to lower next, and lowering it helps both tiers. The
  known one is the string instructions: `string` runs at 32 MIPS where
  everything else clears forty, and `rep movsb` is a loop in either tier.
- **Instance leak.** Bounded per batch and counted; a long-running
  container with self-modifying code invalidating regions all day would
  grow. The runner can recycle table slots and drop instances; the
  engine's contract does not have to change for it to.
- **Cranelift on region bodies.** The cap is a guess at where Cranelift's
  register allocation stops being good, informed by the interpreter's
  own inlining history. The number is measured in T2 and moves only then.
- **Open: entries into a region.** Every block entry in a region is a
  dispatch target. Whether *only* the region's first block should be
  enterable — smaller `br_table`, fewer write-back points — is a
  measurement, and single-entry regions are the simpler thing to build
  first if the choice is free.
- **Open: the AOT tier as a source of hot sets.** A recorded hot-block
  set is ground-truth discovery, and `vm.md` §8 notes it could feed a
  bake. Not in scope here, and nothing here closes the door.

## 14. Pitfalls index

Seeded from the interpreter's own history; to be extended as T1 finds
things.

1. **A trace must not be re-entered at the instruction it just handed
   back.** `Interpret` means one interpreted instruction before
   `compiled` is consulted again, or an exit at a region's own entry is
   an infinite loop. Section 2.
2. **Stop where the interpreter stops.** The budget rule of section 10
   is what keeps a tape replayable across tiers; checking "budget
   exhausted" only at region exits would overrun by a block and change
   the schedule.
3. **Write back before every helper call and reload after.** The helper
   runs the interpreter against the `Tcb`; a stale local afterwards is a
   register silently reverted.
4. **Wrap the address after the limit check, not before.** A 64-bit
   guest address above four gigabytes wrapped to `i32` first is a valid
   offset into somebody's page.
5. **The dword write zeroes the top half; the byte and word writes do
   not.** `Tcb::write_register` is the one statement of the rule and
   the compiled form has to match it slice for slice.
6. **Register a region against every page of every block in it.** A
   region registered against its entry's page survives a write to a
   block in its middle, which is stale execution with no diagnostic.
7. **Do not compile through a `syscall`.** A block ends there, the loop
   owns syscalls, and a region that continued past one would run guest
   code before the kernel had written the answer.
8. **Table slot zero is null.** The linker leaves it unassigned so a
   null function pointer stays null; the first installed function is
   never index zero, and `-1` is the refusal.
9. **Bodies that grow are bodies that get slower.** The region cap
   exists because Cranelift cannot split what it cannot allocate; every
   inlining decision in `performance.md` §6 that went wrong went wrong
   this way.
10. **Instruments off means tier 1 off.** A histogram or profile taken
    with compiled code running is a count of what the interpreter saw,
    not of what the guest ran.
