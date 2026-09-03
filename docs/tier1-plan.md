# Tier 1: hot blocks compiled to wasm at bake time

Status: **design, 2026-09-02** — written for discussion before anything
is built. Rewritten twice the same day: first around a bake-time compile
after the run-time one was found to need the one thing this project's
host contract refuses, help loading code; then around three block
sources after a profile of the user's own container was found to be a
step nobody can be expected to take. Step 2 of the plan in
[performance.md](performance.md) §7. Section 8 of [vm.md](vm.md) was the
sketch this replaces; where the two disagree, this one is current.

## The short version

The container is an x86-64 interpreter compiled to wasm, running the
guest's instructions one at a time. That is the correctness floor: it
executes anything, and it is why the Django demo works at all. It is also
why a request costs 20 ms where native costs a third of one, and why
booting costs thirty seconds — the interpreter does about a hundred
million guest instructions a second, and a boot is three billion of them.

Tier 1 is how virtual machines of this shape close most of that gap: the
code that matters is translated into real machine code, and the
interpreter keeps everything else. Usually that translation happens
while the program runs. It cannot here, because a wasm module cannot
create code — that is the sandbox's whole promise — and the only thing
that can load new code is the host, which this project has decided must
know nothing but two store imports. So the translation happens at
**bake** time instead, and what makes that safe is a property the
interpreter already has: **a compiled block is only ever used for bytes
that match exactly the bytes it was compiled from.** The bake is
therefore free to guess. A block compiled for code that never runs, or
that has been overwritten, is a function nobody enters — never a wrong
answer.

Which blocks to compile is the question, and it is answered without
running the user's container. Most of the native code in any container
is the runtime it was built on — libpython, glibc, the loader, nginx,
openssl — and those bytes are identical across every image that shares
the base. So the bake draws on **three sources**, in order: a *corpus*
of profiles taken once per runtime and matched by bytes, which covers
boot and the runtime's own hot loops for any image built on a common
base; a *static sweep* of the ELFs the image's entrypoint can reach,
ranked by evidence and capped by a size budget, which covers what the
corpus never saw, including code the user built themselves; and, for
whoever can drive their container and wants the last few percent, a
*profile* of their own workload. The first two need nothing but the
image. At run time, whenever the interpreter decodes a block, it looks
the bytes up; on a match, that block runs as machine code the host's own
compiler produced from our wasm. Cold code, rare instructions, faults
and everything strange run interpreted, which remains the only place
correctness lives.

Size was measured before this was written. The image's 745 ELFs hold
39 MB of executable text, of which 34 MB is package tooling no server
runs; the runtime's text is 5 MB, and the Django run executed under one
megabyte of it. Compiled, the runtime is on the order of 20 MB, or half
that with a knob. The module it goes into is 170 MB today because it
carries the filesystem uncompressed, and the filesystem compresses 3.4×;
section 11 makes compressing it part of this plan, and once it is, a
module with tier 1 in it is smaller than a module without it is now.

What to expect: systems of this shape land five to twenty times off the
interpreter on integer code, which is a boot of a few seconds and a
request of a few milliseconds. The ceiling is measured rather than
guessed — this project's own ahead-of-time translator reached parity
with clang's wasm backend on integer and memory kernels, about a hundred
times above the interpreter — and a translator that works block by block
will land short of it. Section 13 says how the number is taken and
section 14 what could stop it.

## Outline

1. **The three block sources** — corpus, static sweep, workload profile.
2. **What a compiled block is** — the unit, its signature, its exits.
3. **Keyed by bytes** — attachment at decode, and why a miss is harmless.
4. **The seam with the run loop.**
5. **Semantics: the `Quick` lowering, compiled** — and the helper for
   everything else.
6. **Registers, flags and memory inside a block.**
7. **Correctness: exits to the interpreter** — faults, hard cases, and
   why compiled code models none of them.
8. **Invalidation** — the block cache's, shared.
9. **Regions** — many blocks in one function, which is where the speed
   is.
10. **The sources in detail** — what a profile records, how the corpus
    is built and matched, how the sweep ranks and budgets, the sizes.
11. **Paying for code with data** — the rootfs, compressed.
12. **Determinism, preemption, threads and signals.**
13. **Where the code lives, and the gates** T0–T4.
14. **Risks and open questions** — including the run-time compile, kept
    as an optional later addition.
15. **Pitfalls index** — seeded.

## 1. The three block sources

```
zaqaru-bake image.tar app.wasm               # corpus + static sweep; no run
zaqaru-bake --hot hot.blocks image.tar app.wasm   # ...plus a workload profile
zaqaru-run  app.wasm -p 8080:80              # the host as it is today
```

A bake compiles blocks from three sources and links them into one
object beside the image and the engine, exactly as the image object is
linked today. The output is a module the unchanged runner runs. Nothing
about the host contract changes: two store imports, one boot export, as
it is now.

**The corpus** is a set of block profiles taken once per runtime — a
libpython, a glibc, an nginx — on representative workloads, by whoever
maintains this tool, and shipped with it. Each entry is a block's bytes
and how much it retired. At bake, the image's ELFs are matched against
the corpus by *content*: an ELF whose bytes are a corpus entry's
contributes that entry's blocks, and blocks are keyed by their own bytes
besides (section 3), so even a differently built library that shares
routines matches where the routines match. This is the source that needs
nothing from the user and covers the most: booting is the loader, libc's
start-up and the runtime's initialisation, all corpus code; and a
runtime's hot loops — CPython's eval loop, glibc's `memcpy`, nginx's
event loop — are hot in every program that uses the runtime.

**The static sweep** covers what the corpus has not seen. The image's
ELFs are ranked by evidence the bake can read without running anything:
the binaries the entrypoint names, their `DT_NEEDED` closure, the
directories a runtime loads extensions from, anything under
`/usr/local`; then the rest, last. Each ELF's executable segments are
decoded from their symbols and direct branch targets into blocks, and
compiled in rank order until a size budget is spent. A wrong guess
about what is code costs bytes and never correctness, so the ranking
only has to be reasonable, and the budget is what keeps a bake's size
predictable.

**The workload profile** is the third source and the only one that runs
the container. For someone who can drive their image — a boot, the
requests that matter, Ctrl-C — a profiling bake counts blocks, the
runner writes them out through one export, and a second bake compiles
exactly what ran. It is the most precise source and the one that cannot
be asked of everyone, which is why it is last and optional. It is also
how the corpus is built: a corpus entry is this profile, taken by us, of
a runtime we ship.

All three feed the same compiler, the same object and the same table,
and a block that appears in more than one is compiled once.

## 2. What a compiled block is

The unit is the extended basic block the cache already holds: a run of
instructions from one guest address through conditional branches to the
first unconditional transfer, call, return or `syscall` (`block.rs`,
`terminator`). A compiled block is one such block, entered at its entry
address and nowhere else — the same rule the cache has, and for the same
reason: a jump into the middle of a block is a second block, never a
special case.

Its wasm signature:

```
(tcb: i32, vitals: i32, entry: i64, budget: i64) -> i64
```

`tcb` is the address of the running thread's `Tcb` in linear memory.
`vitals` is the address of a small fixed-layout header the engine
publishes for the current address space: the base addresses of the
readable, writable and code bitmaps, the memory limit, and the dirty-code
flag — everything a memory access has to consult, at known offsets, so a
compiled block is not tied to one process's pointers. `entry` is the
guest address the block is running at *this time*, which section 3
explains is not known at bake. `budget` is how many instructions may
still retire in this quantum.

The result packs an exit kind into the high bits and a guest address into
the low 32 (a guest address is below 4 GiB by construction, the wasm32
ceiling):

| kind | meaning | `rip` returned |
| --- | --- | --- |
| `Continue` | the block ran to an exit and this is where execution goes | the next instruction to execute |
| `Syscall` | the block ended in `syscall`; `%rcx` and `%r11` are set as hardware leaves them | past the `syscall` |
| `Interpret` | execute this one instruction in the interpreter, then carry on normally | the instruction |

On every exit the `Tcb` is complete: registers, `rip`, the flags record
and `retired` are what the interpreter would have left. That is the
whole of the interface, and it is what lets the loop treat a compiled
block as one very long instruction.

## 3. Keyed by bytes

A compiled block is looked up by **its bytes**, not by its address, and
that one choice carries most of the design's weight.

**Attachment.** When the block cache decodes a block (`BlockCache::install`),
it hashes the bytes it decoded and probes a table the bake linked in.
The table is a sorted array of `(hash, length, offset of the bytes,
function index)`; a hit is confirmed by comparing the decoded bytes
against the stored ones, byte for byte, so a hash collision cannot
attach the wrong function. On a match, `Block.compiled` is set to the
function's table index. The probe is one binary search and one short
memcmp, once per decode — and the cache decodes a block once, so the
whole cost of tier 1's dispatch is paid at the same moment the decode
itself is.

**Why bytes are enough.** Two x86 sequences with identical bytes have
identical semantics, so the same compiled function is correct for both —
in different processes, in different mappings of the same library, in
two libraries that happen to share a routine, in an image the corpus
never saw that carries a runtime it did. Every constant in a block that
refers to an address does so *relative to the block*: rip-relative
operands and direct branch targets are encoded as displacements from the
instruction, so the bytes are the same wherever the code is mapped. The
compiled function is therefore written entirely in terms of `entry`
plus a delta, and it attaches wherever kisal happens to have mapped the
code — the slot placement of `resident.rs`, or anything else, makes no
difference. A block that happened to contain an *absolute* address (a
`mov rax, imm64` of a pointer) is fine too: the immediate is part of the
bytes, and bytes that differ do not match.

**Why a miss is harmless.** A block whose bytes never appear in a run is
a function nobody enters. A block whose bytes the guest has since
overwritten never attaches, because the re-decode after the write sees
the new bytes. A block the run-time decode cut shorter than the bake did
— a page boundary, the instruction cap, a page that stopped being
executable — has a different length and does not match. There is no
such thing as attaching wrongly, only not attaching, and not attaching
is the interpreter, which is correct. This is the property that lets
the bake guess, and it is the property the ahead-of-time translator
never had: there, a wrong guess about where code was became a wrong
answer, and a thousand lines of `code-discovery.md` were the price.
Here a wrong guess is a slightly larger module, and section 10's budget
bounds even that.

**Anonymous code** — a JIT's output, code the guest wrote — is recorded
by the profiler like any other and *could* be compiled, since the key is
bytes. It is left out of version one: the bytes a JIT emits in one run
are not reliably the bytes it emits in the next, and the size is better
spent on code that is.

## 4. The seam with the run loop

`Block` gains one field, the optional compiled entry. `Engine::run`
already looks the block up on every entry through the direct-mapped
`recent` cache, so the check costs one branch:

```
let block = cache.block(index);
if let Some(entry) = block.compiled {
    let exit = call(entry, tcb, vitals, block.entry, budget);
    budget -= retired_since;           // read back off the Tcb
    match kind(exit) {
        Continue  => { tcb.rip = rip(exit); continue }
        Syscall   => return Outcome::Syscall,
        Interpret => { run one instruction through the existing path
                       at rip(exit), without consulting `compiled`;
                       continue }
    }
}
...the interpreter, as today
```

Inside the module, calling a table index is a Rust function pointer: on
wasm32 a `fn` pointer *is* a table index, so `call` is a transmute and
an ordinary call. The compiled functions are in the module's own table
because the linker put them there — the bake's object declares them as
table elements, which is what the emitter's linking layer already does
for the AOT tier's thunks.

`Interpret` is the one exit that needs a rule: the instruction it names
is executed by the interpreter *once*, through `Cpu::run`, and only then
does the loop go back to consulting `compiled`. Without that rule a
block that exits at its own entry would be re-entered forever. The
interpreter path for one instruction is `Engine::run`'s existing body
with the compiled check skipped, so nothing is duplicated.

There is no hit counter and no threshold in the run-time engine. The
bake decided; the engine attaches what matches and runs it. The
profiling engine is the one that counts, and it is a different build.

## 5. Semantics: the `Quick` lowering, compiled

The source of truth for what an instruction does is `Cpu::quick` in
`targum/src/exec.rs`, over the pre-decoded `Quick`. Every `Op` it handles
is a handful of wasm instructions:

| `Op` | compiled to |
| --- | --- |
| `Mov`, `Lea`, `Widen`, `WidenSigned` | a load or local read, an optional extend, a store or local write |
| `Add`, `Sub`, `And`, `Or`, `Xor` | the i64 operation, a truncation to width, the write-back, and a flags record (§6) |
| `Cmp`, `Test` | the same without the write-back |
| `Push`, `Pop` | a store or load at `%rsp` and a local update |
| `Nop` | nothing |
| `Jcc` | a condition (§6) and a branch: taken to `entry + delta`, not taken to the next instruction |
| `Jmp`, `Call`, `Ret` | a constant or loaded target; `Call` pushes the return address; `Ret` pops one |

Operands are already resolved: `Source::Register(slice)` is a local with
the slice's width and high-byte rule applied, `Source::Immediate` is a
constant, `Address::Fixed` is `entry` plus a delta, and
`Address::Computed` is the same base-plus-index-times-scale arithmetic
the interpreter does, with the 32-bit wrap and the `%fs` base where
`quick.rs` says so.

**What `quick.rs` declines, the compiler does not compile.**
`Op::General` is about 5% of a real run — `imul`, shifts, `cmov`,
`setcc`, the string instructions, the SSE moves. For one of those the
compiled block emits a call to an exported engine helper:

```
targum_step(tcb: i32, block: i32, position: i32) -> i32
```

which runs `Cpu::step` for that one instruction against the cached
decode and answers whether it retired normally. Before the call the
block writes its locals and flags back; after it, it reloads them. The
call is not cheap — a full write-back and reload — but it is paid on 5%
of instructions and it keeps compiled code running through the one `shl`
in a hashing loop. A helper that answers anything but "retired, fell
through" — a trap, a `rep` that stayed put, a branch the general path
took — becomes a `Continue` exit at the address the `Tcb` now holds, and
the interpreter takes it from there.

The refinement this sets up is the obvious one: every op lowered into
`quick.rs` later gets both tiers faster, and the mnemonic histogram says
which one is next. Nothing about tier 1 has to be revisited to add an
op.

## 6. Registers, flags and memory inside a block

**Registers are locals.** The sixteen general registers and the `%fs`
base are loaded from the `Tcb` into `i64` locals on entry and stored on
exit. Which of the sixteen a block touches is known at compile time, so
only those move; the AOT tier's promotion work is the record of how much
this is worth — it took the integer kernel from 5.1× off clang to parity,
and the same flush discipline applies. Byte, word and dword slices are
masks and shifts on the local, with the dword write zeroing the top as
`Tcb::write_register` does; one rule, stated once, in one place per tier.

**Flags are lazy at compile time too.** The interpreter records the last
flag-writing operation (`Flags::record`: rule, width, left, right,
result) and computes a condition on demand (`Condition::holds`). Compiled
code keeps that record in locals, and when a `Jcc` follows the `Cmp` that
feeds it within the same block — which the histogram says is the common
shape, `Cmp` and `Test` at 14% of the stream and the conditional jumps
at 16% — the compiler knows the rule statically and emits the direct
wasm comparison: `Below` after a `Cmp` is `i64.lt_u`, `Equal` is
`i64.eqz` of the result, and so on for the sixteen conditions and the
`Add`, `Sub` and `Logic` rules. When the rule is not known statically —
a conditional jump at the start of a block, fed from a predecessor — the
block stores the record and calls
`targum_condition(tcb, condition) -> i32`, which is `Condition::holds`
exported. At every exit the record is stored into the `Tcb`'s `Flags` in
the interpreter's own representation, so a `pushf` or a signal frame
built by the interpreter a moment later sees exactly what it would have.

**Memory is linear memory.** A guest address is a linear-memory offset,
so a load is `i64.load` at the address, wrapped to `i32` *after* the
check that it is below the memory limit. The check is the interpreter's
(`Space::permitted`): the page's bit in the readable or writable bitmap,
found through `vitals`, and for a store the code bitmap as well. About
five wasm instructions per access. A failed check is an exit — section 7
— and a store to a page whose code bit is set calls
`targum_code_write(vitals, address, length)`, which queues the page
exactly as `Space::note_code_write` does, and then exits, because the
next instruction might be the one just overwritten. The interpreter's
`rep stosb` test, where a repeated store replaces its own instruction
after one iteration, passes unchanged under compiled code because the
code never gets past the store.

Whether the inline checks are affordable is a measurement, and section
13 takes it. They are not optional: they are what makes a wild pointer a
`SIGSEGV` with a faithful address rather than a silent read of somebody
else's bytes, which the design document lists as the fidelity class the
AOT tier could not have. They cannot be replaced by hardware either — a
wasm out-of-bounds access is a trap, and a trap is unrecoverable: it
unwinds to the host, and the guest's signal handler never runs. They are
also most of the compiled code's size, which section 10 comes back to.

## 7. Correctness: exits to the interpreter

Compiled code models no fault, no trap and no strange case. Whatever it
cannot fully handle at an instruction, it handles by *not executing that
instruction*: it writes its locals and flags back, sets `rip` to the
instruction, and returns `Interpret`. The interpreter then executes it
and produces whatever it produces — a `SIGSEGV` with the right access
kind and address, an undefined-instruction trap, a `Trap::Unsupported`
naming the bytes — with `rip` and `retired` exactly where the
architecture says they are, because the interpreter's own rules put them
there. The compiled code contributed nothing to the diagnosis and could
not have got it wrong.

This is also why the permission check needs no fault path of its own.
The check compiled into the block is the same check `Space::load` makes;
if it fails there it will fail in the interpreter a moment later, which
is where the `Fault` gets its `si_addr`. And it is why the `Interpret`
rule of section 4 exists: the instruction is run exactly once,
interpreted, before the loop goes back to trusting compiled code.

The consequence for the differential suite is the acceptance criterion of
milestone T2: **every program the suite runs must produce identical
output, syscall trace and exit status when baked with every block it
executes compiled**, including the programs that fault on purpose, the
ones that write code and run it, and the ones that are preempted
mid-`rep`.

## 8. Invalidation

A compiled block is attached to a decoded block and lives exactly as
long as it does. `invalidate_page` drops the decoded block; the attachment
goes with it, and the re-decode after the write attaches again only if
the new bytes match something — which, for bytes the guest just wrote,
they will not. No generation stamp is needed: the `recent` cache
validates a hit against the block it names, as it does today, and a
freed block's slab index is a miss.

A region (section 9) attaches only after *every* block in it has been
verified against the bytes at its expected place, and is registered
against the pages of every one of them, so a write to any of them drops
the whole region. That is coarser than necessary and it is the right
first answer: the alternative is a region that survives with a hole in
it, which is the shape of bug the block cache's comment warns against
reintroducing.

Nothing leaks. The compiled functions are part of the module, there for
its whole life whether attached or not, and detaching is clearing a
field.

## 9. Regions

A single compiled block is a proof of the seam, not a speed-up. Blocks
average five instructions, and calling a function that loads a dozen
locals, runs five instructions and stores the locals back is not faster
than interpreting them. The speed is in **regions**: a set of blocks
connected by constant-target branches, compiled into one wasm function.

A region has one entry function whose body is a `br_table` on which of
its blocks it was entered at, dispatching into the block bodies laid out
inside a single `loop`. Within the region:

- a conditional or unconditional branch to a block in the same region is
  a `br` to that block's label — no exit, no write-back, no lookup;
- a branch to an address outside the region is an exit with `Continue`;
- guest registers live in locals for the whole region and are written
  back only at exits and helper calls;
- the flags record lives in locals, and a `Jcc` whose feeding `Cmp` is in
  the same block folds to a direct comparison as in section 6;
- a hot loop becomes a real wasm loop, which Cranelift allocates
  registers across — the shape the AOT tier reached clang parity with,
  applied to whatever the sources found.

At bake, region formation starts at a block and follows its
constant-target successors — fall-through, taken branch, near `jmp` and
`call` targets — while they are among the blocks being compiled, up to a
cap of 64 blocks. For a profiled block the successors are the hot ones;
for a swept block they are whatever the sweep decoded. A region's blocks
are therefore at fixed *relative* offsets from its entry, which is what
the bytes-keying needs: the region's table entry records every block's
delta from the entry and every block's bytes, and attachment (section 3)
verifies all of them at the moment the entry block is decoded — reading
a few kilobytes at `entry + delta` for each, checking each page is
executable, comparing. A block that belongs to a region is also
compiled alone, so that a region that fails to verify because one of its
blocks has changed still leaves the entry block fast.

`Call` inside a region pushes the return address and dispatches to the
target if it is in the region; `Ret` pops and exits with `Continue`,
because a return address is not a constant and the loop's lookup is the
honest way to follow it. Every block entry in a region is a dispatch
target in version one; whether single-entry regions are worth their
smaller dispatch is an open question (section 14).

The size cap is not tunable in the sense of "larger is better". Cranelift
does not split a function it cannot allocate registers for, and the
engine's own history — `performance.md` §6 — is a list of things that
got slower when one wasm body grew too large. Sixty-four blocks of five
instructions is a few hundred wasm instructions per block body and a
function Cranelift handles comfortably; the cap moves when a measurement
says so, and not before.

## 10. The sources in detail

### What a profile records

The profiling engine already counts retired instructions per guest
address. What the bake needs is per *block*: its bytes, how many times
it was entered, and how many instructions it retired. So the profiling
build counts at block entry rather than at every instruction — cheaper
than the address profile, and the same `profile` feature grows a second
table keyed by the block's bytes. The bytes are the key rather than the
address for the reason of section 3: the same routine in two processes,
or in one library mapped twice, is one entry with the counts summed, and
nothing has to attribute an address to a file.

`targum_profile(into: i32, capacity: i32) -> i32` is an export that
serialises the table into guest memory the runner reads back — bytes,
entries, retired, one record per block. The runner's `--profile` calls
it once after `targum_boot` returns and writes the file. A run that ends
by Ctrl-C ends the same way it does today, with the tree told and
reaped, and then the export is called; the profile is of everything up
to that moment. An export is not host support in the sense this design
forbids: the host already calls `targum_boot`, and this is a
development-time convenience a browser harness can do in five lines or
skip. A tape recorded by the runner replays deterministically, so a
profile can also be taken from a recording after the fact.

### The corpus

A corpus entry is a profile of a runtime, taken on a workload chosen to
be representative — for a Python, a boot, a stdlib import and the
interpreter loop under a benchmark; for nginx, a boot and a few thousand
requests — and stored as the profile file plus the content hash of each
ELF it was taken from. The corpus ships with the bake tool and grows as
runtimes are added; it is ours to build and to keep current as
distributions ship new builds, and its value is proportional to how many
images share a base, which for the Python, Node, Ruby and nginx worlds is
nearly all of them.

Matching is by content, twice over. An ELF in the image whose bytes hash
to a corpus entry's contributes the entry's blocks outright. And every
block is keyed by its own bytes at attach, so a library the corpus never
saw that shares routines with one it did — a static glibc in a Go
binary, say — matches where the routines match, with no entry for it.

What the corpus does not cover: native code the user built, and code
paths in a runtime that no representative workload exercises. The first
is the sweep's job; the second is small and runs interpreted.

### The static sweep

For ELFs the corpus does not match, the bake decodes blocks from the
files themselves. Two questions: which ELFs, and where the code is.

Which ELFs is a ranking, from evidence the image carries:

1. the binaries the entrypoint and command name, if they are ELFs, and
   the shell if they are scripts;
2. their `DT_NEEDED` closure, resolved the way the loader will;
3. the directories a matched runtime loads extensions from
   (`lib-dynload`, `site-packages`, `node_modules` builds);
4. anything under `/usr/local`, which is where an image's own build
   lands;
5. everything else, last.

Where the code is: a recursive descent from each ELF's symbols and
entry points, following direct branch and call targets, then a linear
sweep of the remaining bytes of each executable segment. This is the
AOT translator's discovery problem again, and here it has no teeth: a
byte sequence that was not code compiles to a function nobody enters,
and a real entry the descent missed is interpreted. The sweep's blocks
are the cache's blocks — from an entry to the first unconditional
transfer — starting at every instruction the descent reached and,
within a region, dispatched on every block entry (section 9), so a
jump-table target inside a swept function attaches without anyone
having recovered the table.

The budget is what makes the sweep safe to size. Ranks are compiled in
order until the budget is spent, and the default is a fraction of the
image rather than a constant, so a small image is not dominated by its
compiled code. A `--budget` overrides it, and `--sweep none` turns the
sweep off for someone who wants only the corpus.

### The sizes

Measured on the Django image, 2026-09-02:

| | size |
| --- | --- |
| executable text, all 745 ELFs | 39.4 MB |
| of which package tooling no server runs (apt, dpkg, perl, sqv, libdb, libstdc++…) | ~34 MB |
| text of python, libc, the loader, nginx and the extension modules | 5.1 MB |
| what the Django run executed, by distinct address | ~0.85 MB |
| rootfs on disk | 124 MB |
| rootfs with zstd -3 | 37 MB |

Compiled wasm runs four to eight times the size of the x86 it came from,
which is what a block with inline permission checks on every memory
operand comes to; a check routed through a helper would halve it at a
cost in speed, and is a knob rather than a decision. So the sources
land at, roughly:

| source | compiled |
| --- | --- |
| corpus, 90% of a runtime's retired instructions | 0.5 MB per runtime |
| corpus, everything a runtime's workload executed | 5 MB per runtime |
| static sweep of the ranked runtime text, 5.1 MB | 20–40 MB, or 10–20 with helper checks |
| static sweep of everything | 150–300 MB — which is why there is a budget |

The last row is what the sweep would cost without the ranking and the
budget, and it is the number that makes section 11 part of this plan
rather than a separate one: the module that carries tier 1 has to be
smaller than the module that does not, and the filesystem is where the
bytes are.

## 11. Paying for code with data

The 170 MB module is the filesystem, uncompressed, plus a few megabytes
of engine. The filesystem compresses 3.4× with zstd at its cheapest
level. So the bake compresses file contents, and kisal decompresses on
open.

The image is read-only, so this touches one place: `Filesystem::contents`,
which hands back a file's bytes for `read`, `mmap` and `exec`. It
decompresses on first use into a per-file cache the kernel owns, and
frees it when the file is closed everywhere — the same lifetime an inode
already has. Decompression is kisal code compiled to wasm, on the order
of half a gigabyte a second, so a boot that opens a hundred files and
twenty megabytes pays some tens of milliseconds, once, against thirty
seconds of interpretation. Files below a size floor, and files that do
not compress, are stored raw. The index format of `container-plan.md`
gains a compression field per file and nothing else changes: the
overlay, the synthetic filesystems and the bake's layer handling are
above this.

With the filesystem at about 40 MB, a module carrying the corpus and a
budgeted sweep of the runtime is on the order of 60 to 80 MB — under
half of today's 170 — and a module carrying tier 1 is smaller than one
without it is now. This is the largest size win available anywhere in
the project, it needs no run and no host change, and it comes first in
the gates because it makes every other size question in this document a
small one.

## 12. Determinism, preemption, threads and signals

**The interleaving is unchanged.** Scheduling is a pure function of
retired instructions, and record and replay depend on that. Compiled
code must therefore stop at *exactly* the instruction the interpreter
would have stopped at when the quantum runs out, not merely near it. The
rule: at entry and at every region-internal edge, if the budget is
smaller than the next block's instruction count, exit with `Continue` at
that block's entry instead of running it. The interpreter then finishes
the quantum one instruction at a time and stops where it always stops.
Block lengths are constants at compile time, so this is one compare per
block. It makes the schedule identical whether a module carries no
compiled code, some, or all of it — and that becomes a test: **the same
tape replays against a bake with tier 1 and one without**, which
`vm.md` §12 already asks for at V5.

**Preemption** is that rule. **Threads** are unaffected: compiled code is
handed the running thread's `Tcb`, an attachment belongs to a decoded
block in one process's cache, and a context switch is a different `Tcb`
on the next call. **Signals** are delivered at block boundaries by kisal
before `Engine::run`, and a region runs at most a quantum before
returning, which is the same latency the interpreter already has. A
fault inside a region is an `Interpret` exit and then the interpreter's
fault, delivered as today with the frame the interpreter builds.

**The instruments stay exact.** The mnemonic histogram and the address
profile count in `Cpu::advance` and `Cpu::run`, which compiled code
never calls; so in a build with either feature on, nothing attaches, and
a count is still an exact count of what the guest executed. The block
profile of section 10 is the one instrument that runs beside attachment,
and it counts at entry in the loop, before the compiled check, so it is
exact too.

## 13. Where the code lives, and the gates

- `baker` — `compress.rs` (the per-file compression and the index
  field), `sweep.rs` (the ELF ranking, the descent and linear sweep, the
  budget), `corpus.rs` (loading and matching entries by content), and
  the `--hot`, `--budget`, `--sweep` and `--profiling` options.
- `targum/src/tier1/` — `profile.rs` (the block table and its
  serialisation), `attach.rs` (the linked table, the probe at decode, the
  region verification), `compile.rs` (a block or region to a wasm
  function: the `Op` table of section 5, the locals discipline of
  section 6, the exits of section 7), and `object.rs` (the functions,
  the table elements and the lookup table as one relocatable object).
  `compile.rs` and `object.rs` run natively, in the bake; `profile.rs`
  and `attach.rs` run inside the module.
- `emit/` — a new small crate holding what is today `src/emitter/`: the
  section writers, `FunctionBodyBuilder`, and the linking layer that
  produces objects `wasm-ld` accepts. Pure Rust, no dependencies, already
  written and already producing objects the bake links. The root crate
  goes on using it through the same paths.
- `kisal` — decompression on open in `Filesystem::contents`, and nothing
  else. The kernel does not know which tier executed an instruction, and
  that is the design.
- Three exported engine helpers: `targum_step`, `targum_condition`,
  `targum_code_write`, thin wrappers over `Cpu::step`, `Condition::holds`
  and `Space::note_code_write`; and the `targum_profile` export.
- `runner` — `--profile`, which calls one export and writes one file.
- `corpus/` — the shipped profiles, one directory per runtime, each with
  the content hashes it was taken from and the workload that took it.

The host interpreter (`kisal/examples/interpret`) stays tier 0: it has
no wasm to run. It remains the development instrument for kernel work,
and tier 1's own tests run under wasmtime.

**T0 — the filesystem, compressed. Built 2026-09-02.** The inode flag,
the bake's compression, decompression on open with the per-file cache,
and the negative control — an index claiming one byte more than the
frame decodes to is refused at open. The Django module went from
170 MB to **70 MB**, with boot at 29 s and a warm request at 20 ms,
unchanged. The 50 MB the gate asked for assumed the single-stream ratio
of the whole tar; per file at level 3 the rootfs compresses 2.3×, level
19 buys a further 15% at a slower bake, and the rest of the gap is the
redundancy between similar files that only a shared dictionary — which
`ruzstd` supports — would recover. Open, and not blocking: the hundred
megabytes this freed are the room tier 1 needs.

**T1 — the profile and the sweep.** The block table in the profiling
build, the export, the runner's `--profile`, the file format; the ELF
ranking, the descent and sweep, the budget. Acceptance: the Django
warm-up produces a profile whose coverage curve reproduces the table of
section 10; a profile taken from a replayed tape is byte-identical to one
taken live; the sweep of the Django image ranks python, libc, the loader,
nginx and the extension modules first and stops at the budget, and its
block set contains every block the profile found. *The sweep, the
ranking and the budget are built (2026-09-02, `src/tier1/sweep.rs`,
`bake-vm --tier1`); the profile is not yet.*

**T2 — single blocks. Built 2026-09-02, in its single-block form.** The
compiler for every lowered op with `targum_step` for the rest, the
locals and flags discipline, the exits, the object, the table,
attachment at decode, invalidation (`src/tier1/compile.rs`,
`object.rs`; `targum/src/tier1.rs`). What stands: the seven container
tests green with every block of their programs compiled from the sweep
alone (`ZAQARU_TIER1=1`), including the fork, the exec chain, the
sockets and record-and-replay; a static hello green under the engine's
own verify mode, which interprets every compiled block first on an undo
journal and compares the two machines after each; and the block
differential (`examples/tier1-diff.rs`), which runs every block of a
binary from random state compiled and interpreted, 21,000 blocks with
no disagreement. `alu` runs at 1.5× the interpreter — the single-block
ceiling the plan named, with the loads and stores of a whole register
file around five instructions. Three defects the instruments found on
the way, each a lesson about the frame rather than an op: a budget
decline that ran the epilogue over locals never loaded; a call target
parked in a scratch local the stack check reused; and an operand read
that clobbered the flags record before the access was known to succeed.
The other acceptance items — the profiled bake, the tape across bakes,
the altered-bytes control — wait on T1's profile.

**T3 — regions. Built, and capped at one member pending a defect
(2026-09-02).** Formation at bake (`src/tier1/region.rs`), the `br_table`
dispatcher, internal branches, the region-wide frame, the budget rule
per member, verification of every member's bytes at attach, and the
table extended with a row per region and per member. The engine's verify
mode is what holds it: a two-member region leaves `%rsp` eight low —
one stack slot, with every other register, the flags and `rip` correct —
which places the defect at the boundary between members, in the
retired-count accounting or the flush around a deferred instruction, not
in any instruction's arithmetic. `MAX_MEMBERS` is one until that is
found, which makes every region a single block and keeps the compiler to
what the differential and the container suite verify. The speed number
waits on the cap: single blocks are 1.5×, and the frame a region would
amortise is the rest.

**T4 — the container and the corpus.** The first corpus entries —
`python:3.12-slim`'s libpython, glibc and loader, and nginx — taken on
stated workloads. The Django demo baked three ways: corpus and sweep
with no run of the image; with its own profile added; sweep alone.
Boot, warm request, four clients, module size, load time, and the share
of retired instructions that ran compiled, reported at exit beside the
MIPS figure, for each. Recorded against the literature band of `vm.md`
§11 and the AOT tier's ceiling, in `performance.md` §1, honestly,
whatever it turns out to be. The number that decides whether the corpus
idea holds is the compiled share of the no-run bake against the
profiled one.

## 14. Risks and open questions

- **Coverage without a run is the corpus's and the sweep's.** The
  corpus covers what a representative workload of the runtime did; the
  sweep covers what it decoded, within a budget. What neither reaches
  runs interpreted, correctly. T4's compiled-share figure on the no-run
  bake is the measurement, and it is the one that matters most to
  whether this design delivers what it promises for images nobody drove.
- **The corpus is ours to keep.** A new Debian build of glibc is new
  bytes and a stale entry, matched only where routines happen to be
  unchanged. Entries have to be retaken as bases move, which is a chore
  proportional to the number of runtimes, and a chore is not a risk so
  long as somebody owns it.
- **The checks per access.** Five wasm instructions on every load and
  store is real, and memory operands are most instructions. If T3's
  kernels say the checks are the ceiling, the answer is a cheaper
  representation of the same fact — one bitmap for "readable and not
  code", say — and never dropping the fact. They also set the floor on
  size, at about 2.5× the x86 with helpers, which the budget already
  assumes.
- **`targum_step` on a hot path.** If an unlowered op sits inside a hot
  loop, the helper's write-back and reload dominate a region. The
  histogram says which op to lower next, and lowering it helps both
  tiers. The known one is the string instructions: `string` runs at 32
  MIPS where everything else clears forty, and `rep movsb` is a loop in
  either tier.
- **Load time.** wasmtime compiles the module's code at load — 0.23 s
  today, because the filesystem is data it walks past. Twenty megabytes
  more code is a few seconds more, once, and wasmtime caches compiled
  modules; a browser's baseline compiler is faster still. Measured in
  T4 and reported beside the size; if it matters, the budget is the
  knob.
- **Runtimes that JIT.** Node, the JVM and a JITting Ruby emit code at
  run time that no bake can see, and it runs interpreted. Their
  interpreter-only modes exist and are the right setting under this
  engine; the corpus entry for such a runtime is taken that way.
- **Cranelift on region bodies.** The cap is a guess at where register
  allocation stops being good, informed by the interpreter's own inlining
  history. Measured in T3; moves only then.
- **Open: entries into a region.** Every block entry is a dispatch
  target in version one. Whether single-entry regions are worth their
  smaller dispatch and fewer write-back points is a measurement.
- **Open: the sweep's descent.** Symbols and direct targets find most
  code; what they miss inside a function is found by the linear sweep,
  and what the linear sweep decodes wrongly costs bytes. Whether the
  budget is better spent on descent-reached code only is a T4 question
  with the compiled-share figure as its answer.
- **Later, optional: compiling at run time.** The same compiler,
  compiled to wasm32 inside the engine, could translate blocks a run
  finds hot and hand the bytes to a host through one import,
  `targum_install(bytes, length) -> i32`, for a host that offers it — a
  browser's `WebAssembly.instantiate` or thirty lines of wasmtime. The
  host would put the functions into the engine's table and answer the
  base index. That was the first version of this design, and it is kept
  here as an addition rather than an alternative: it needs the host's
  help, it needs batching to amortise the host's compile time, and it
  leaks an instance per invalidated region. A bake covers what the
  sources saw; a run-time compile would cover what they did not.
  Nothing in sections 2 through 9 changes for it.

## 15. Pitfalls index

Seeded from the interpreter's own history and from the earlier versions
of this design; to be extended as T2 finds things.

1. **A hash hit is not a match.** Compare the bytes. A collision that
   attached the wrong function would be a wrong answer with no
   diagnostic, and the whole design rests on that being impossible.
2. **Verify every block of a region, not its entry.** A region attached
   on its entry's bytes alone would run a block that has since changed.
   Section 9.
3. **Check the pages, not just the bytes.** A region's later blocks may
   sit on a page that is no longer executable; the interpreter would
   fault on fetching it, and a region must not run it.
4. **Compiled code must not be re-entered at the instruction it just
   handed back.** `Interpret` means one interpreted instruction before
   `compiled` is consulted again, or an exit at a region's own entry is
   an infinite loop. Section 4.
5. **Stop where the interpreter stops.** The budget rule of section 12
   is what keeps a tape replayable across bakes; checking "budget
   exhausted" only at region exits would overrun by a block and change
   the schedule.
6. **Write back before every helper call and reload after.** The helper
   runs the interpreter against the `Tcb`; a stale local afterwards is a
   register silently reverted.
7. **Wrap the address after the limit check, not before.** A 64-bit
   guest address above four gigabytes wrapped to `i32` first is a valid
   offset into somebody's page.
8. **The dword write zeroes the top half; the byte and word writes do
   not.** `Tcb::write_register` is the one statement of the rule and
   the compiled form has to match it slice for slice.
9. **Everything address-shaped is `entry` plus a delta.** A compiled
   block that baked in an absolute address is right at one mapping and
   silently wrong at the next; the bytes still match, so nothing would
   catch it. Only the immediates the guest itself wrote are absolute.
10. **Register a region against every page of every block in it.** A
    region registered against its entry's page survives a write to a
    block in its middle, which is stale execution with no diagnostic.
11. **Do not compile through a `syscall`.** A block ends there, the loop
    owns syscalls, and a region that continued past one would run guest
    code before the kernel had written the answer.
12. **Bodies that grow are bodies that get slower.** The region cap
    exists because Cranelift cannot split what it cannot allocate; every
    inlining decision in `performance.md` §6 that went wrong went wrong
    this way.
13. **Instruments off means nothing attaches.** A histogram or address
    profile taken with compiled code running is a count of what the
    interpreter saw, not of what the guest ran; the block profile is the
    one that counts before the compiled check, and is the only one that
    may run beside it.
14. **The sweep has no budget until it has one.** A sweep of everything
    is 150 to 300 MB of wasm. The ranking and the budget are not
    refinements to add later; they are what makes the sweep a source at
    all.
15. **A compressed file that decompresses short is a loud error, not a
    short file.** The index says the length; a mismatch is corruption,
    and a guest that reads fewer bytes than the file has will fail
    somewhere far from here.
