# zaqaru — native-code-to-wasm transpiler: design

Status: **active** (move to `docs/archive/` when superseded)
Date: 2026-08-25 · MVP built the same day; see
[archive/implementation-plan.md](archive/implementation-plan.md) for what
each milestone delivered. Vectors and floating point were designed and built
2026-08-26; see [archive/float-plan.md](archive/float-plan.md) for what each
of its milestones delivered. Interop with ordinary wasm followed the same
day; see [archive/interop-plan.md](archive/interop-plan.md).

## Overview

zaqaru lifts machine code from native object files and lowers it to
WebAssembly. The input is an x86-64 ELF relocatable object (`.o`); the output
is a **relocatable wasm object file** following the LLVM tool-conventions
linking format, so that stock `wasm-ld` can link one or more transpiled
objects into a final `.wasm` module. Execution and testing target wasmtime.

The canonical minimal case:

```c
int add(int a, int b) { return a + b; }
```

`gcc -c add.c` → `zaqaru add.o -o add.wasm.o` → `wasm-ld` → a module whose
exported `add` returns 5 for (2, 3), verified against the natively compiled
function.

## Goals (MVP)

- Translate simple, standalone x86-64 ELF `.o` files: integer code, direct
  calls, branches and loops, references to `.data`/`.rodata`/`.bss`.
- All symbols in the input are defined (no unresolved externals).
- Emit relocatable wasm objects that stock `wasm-ld` links, including
  linking two separately transpiled objects that call each other.
- Export every function defined in the input object from the final module.
- Differential correctness: transpiled functions produce the same results as
  the natively compiled code on the same inputs.

## Non-goals (MVP)

- System interfaces (syscalls, libc), dynamic linking, TLS, unwinding.
- Varargs, by-value struct passing, stack-passed arguments (beyond what
  faithful stack emulation gives free). Floating point and SSE were a non-goal
  of the MVP and have since been built; see "Vectors and floating point".
- Self-modifying code, overlapping instructions, non-function code in
  `.text`.
- Performance of the generated code. Correctness first; the optimization
  path is designed-for but not built.

## Why relocatable output (the load-bearing decision)

Two output modes were considered:

- **Self-contained**: zaqaru acts as a mini-linker — assigns final addresses
  to all sections, resolves every relocation itself, emits a finished
  `.wasm`.
- **Relocatable** (chosen): zaqaru translates ELF relocations into wasm
  relocations and defers layout, symbol resolution, and merging to
  `wasm-ld`.

The relocatable choice buys:

- Real linker semantics for free: layout, symbol resolution, GC of unused
  symbols, stack allocation.
- Separate compilation: transpile `foo.o` and `bar.o` independently, link
  them together — the payoff demo for this architecture.
- A future path to mixing transpiled objects with clang-compiled wasm
  objects (with adapter shims; see "Calls and ABI").

It costs: emitting the `linking` and `reloc.*` custom sections ourselves,
and padded-LEB immediates at patch sites. If this proves more painful than
expected, the fallback is the self-contained mode — the lifter is symbolic
either way (see below), so only the final stage changes.

## Pipeline

```
ELF .o ──(reader)────► sections + symbols + relocations       [object crate]
       ──(lifter)────► per-function basic-block CFG,
                        operands resolved to symbol+addend    [iced-x86]
       ──(structurer)─► wasm structured control flow
       ──(emitter)────► relocatable wasm object               [hand-rolled
                                                               binary encoding]
```

### Reader

The `object` crate parses sections, the symbol table, and relocations.
Function extents come from `STT_FUNC` symbols (value + size within
`.text`).

### Lifter

iced-x86 decodes each function. Basic-block leaders are branch targets and
fall-throughs of terminators; blocks end at jumps, conditional jumps,
`ret`, and (for CFG purposes only) not at `call`.

**Symbolic operands are the core lifting invariant.** Functions are
disassembled at section offset 0 — no concrete addresses are ever assigned.
Wherever an ELF relocation lands inside an instruction, iced's
constant-offsets API tells us which bytes are the displacement or
immediate; that operand is recorded as *symbol + addend*, not a number.
Direct call targets are therefore symbols, RIP-relative data references are
symbols, and PC-relativity evaporates during lifting (see relocation
translation below).

**No new instruction set in the MVP.** The intermediate form is the
basic-block CFG of decoded x86 instructions annotated with symbolic
operands. A dedicated IR earns its place later (selective ABI lifting,
optimization); the design keeps that door open by making calls carry a
symbolic target plus an optional signature from day one.

### Structurer

Wasm has no goto, so the basic-block CFG must be expressed with
`block`/`loop`/`if`/`br`/`br_table`. Two translation modes, built in this
order:

1. **Dispatcher** (universal): one `loop` wrapping a `br_table` over a
   "current block index" local — a switch-threaded interpreter shape.
   Correct for *any* CFG, including irreducible ones, with zero graph
   theory. This gets the project end-to-end fast.
2. **Structured** (dominator-based): compiler-generated CFGs are almost
   always reducible, so the algorithm from Ramsey's *"Beyond Relooper"*
   (ICFP 2022) applies directly — compute the dominator tree
   (Cooper-Harvey-Kennedy over reverse postorder), back-edge targets become
   `loop` headers, nodes with multiple forward predecessors become `block`
   join points placed per the dominator tree, and branches become `br` at
   computed depths. Irreducible CFGs are detected and fall back to the
   dispatcher.

The dispatcher stays permanently as (a) the irreducible fallback and (b) a
**differential-testing oracle**: every test runs through both modes and the
results must agree. Because all machine state lives in globals (below),
every wasm block is empty-typed — no block parameters to manage.

## Machine model

Everything in this section describes the state as it exists **at function
boundaries and between objects** — the convention. Inside a translated
body, every cell the function touches is promoted to a wasm local and the
globals are only read at entry and written at escape points; see "Register
promotion" under "Calls and ABI" for the discipline and the measured
effect.

- **Registers**: the 16 general-purpose registers are `i64` wasm globals
  (`x86_rax`, `x86_rbx`, … `x86_r15`). Sub-register writes (`eax`, `ax`,
  `al`, `ah`) follow x86 semantics — notably, 32-bit writes zero the upper
  half.
- **Vector registers**: the 16 XMM registers are *pairs* of `i64` globals
  (`x86_xmm0_lo`, `x86_xmm0_hi`, … `x86_xmm15_hi`) rather than `v128`
  globals, for a reason that is entirely about what LLD can read; see
  "Vectors and floating point".
- **Flags**: separate `i32` globals (`x86_zf`, `x86_sf`, `x86_cf`,
  `x86_of`, `x86_pf`), computed **eagerly** by every flag-setting
  instruction. The adjust flag is not modeled and an instruction that
  *consumes* it is a translation error; it exists only for binary-coded
  decimal, which compilers do not emit. Parity is modeled, because
  floating-point compares report *unordered* through it. An early version
  of this document argued flags had to stay in globals because flag
  liveness crosses block boundaries; promotion made that moot — a local is
  function-scoped, and blocks are inside functions. What flags do *not* do
  is cross calls or returns, on the SysV ABI's authority; the one seam
  they do cross, a tail jump into a compiler's split-off cold path, is the
  one place promoted flags are flushed. The planned "global liveness pass
  to delete dead flag computation" was overtaken by the same change: once
  flag stores stop escaping the function, the engine's own dead-code
  elimination deletes the dead computations, measurably.
- **Memory**: one linear memory (wasm32 for now — see "Pointer width").
  The native stack lives *inside* linear memory; `x86_rsp` is just another
  register global pointing into it. `push`/`pop`, red-zone accesses, and
  spill slots therefore work with no special cases.
- **Stack allocation**: transpiled objects import `__stack_pointer` — the
  mutable global wasm-ld already materializes and sizes (`-z stack-size`)
  for the wasm C ABI — and host-entry wrappers initialize `x86_rsp` from
  it. The linker owns stack allocation; we build nothing. Alignment note:
  at guest-function entry, SysV requires `rsp % 16 == 8` (as if a return
  address was pushed); wrappers compute
  `rsp = (align_down_16(__stack_pointer)) - 8` and store a sentinel in the
  fake return-address slot.

### Machine state across objects (linking design)

If two transpiled objects link together they must share one register file.
The register and flag globals are emitted in **every** translated object as
**weakly-bound defined symbols**, so wasm-ld dedupes them the way it
dedupes C++ inline functions, and the single-object case needs no companion
file. The linking format supports weak binding on global symbols; whether
wasm-ld's *implementation* handles weak globals as cleanly as weak
functions is an early de-risk experiment (implementation plan, milestone
2). Fallback if it misbehaves: a tiny companion "machine state" object that
defines the globals while translated objects import them.

## Pointer width

x86-64 registers are 64-bit; that is non-negotiable, all register state and
arithmetic is `i64`. The open question was only the memory address space:

- **wasm32 + wrap** (chosen for now): address computation stays in `i64`;
  a single `emit_address` choke point wraps to `i32` at every load/store.
  With wasm-ld owning layout, data is placed low and every legitimate
  address is small, so truncation risk in simple C is minimal. This is
  also wasm-ld's best-trodden road.
- **memory64**: honest 64-bit pointers, no truncation class at all; the
  64-bit wasm relocation variants (`R_WASM_MEMORY_ADDR_LEB64` etc.) exist
  for it. Revisit trigger: guest code that plays high-bit games, or interop
  scenarios where 8-byte pointers in shared structs matter.

Because all address emission flows through one helper, flipping this is a
build flag, not a redesign.

## Calls and ABI

### Internal calls: the emulated-register convention

Guest-to-guest calls keep all state in the shared globals; a translated
function has wasm type `() -> ()`.

- `call target` → `x86_rsp -= 8`; store a sentinel where the return address
  would be; wasm `call` (carrying a `FUNCTION_INDEX_LEB` relocation against
  the target symbol).
- `ret` → `x86_rsp += 8`; wasm `return`. RSP arithmetic stays balanced
  because the callee's `ret` pops the slot the caller pushed.
- A tail `jmp` to another function symbol → call + return.

**Why emulated registers rather than lifting args to wasm params:** the
emulated convention assumes *nothing* about calling conventions. The binary
is always self-consistent with itself, and faithful register emulation
inherits that consistency — correct even for hand-written asm and for
compiler-optimized internal functions where GCC/LLVM deviate from SysV for
local symbols (interprocedural register allocation, constprop clones).
ABI lifting requires a correct signature for every function, and a wrong
signature is not an error but silent corruption. Callee-saved semantics
come free: the guest's own prologue/epilogue spill code executes faithfully
against the emulated stack.

### The export boundary: host-entry wrappers

A `() -> ()` export is unusable from a host, so each exported guest
function gets a **host-entry wrapper**. With no signature knowledge it is
the uniform zero-information shim
`(param i64 ×6, f64 ×8) (result i64, f64)`, which fills every argument
register of *both* register files — rdi/rsi/rdx/rcx/r8/r9 and the low
halves of xmm0–7 — and hands back both result registers, rax and xmm0. The
wrapper sets those registers, initializes `x86_rsp` from `__stack_pointer`
(with the alignment treatment above), and calls the guest function.

Filling everything is what keeps the shim free of per-function type
knowledge: a caller sets the slots its function actually reads, leaves the
rest at zero, and ignores the half of the result its return type does not
live in. Arguments past those counts travel on the stack, which stays out
of scope.

Naming: the wrapper owns the clean ELF name (`add`) so the final module's
exports match the input object's symbol table; the guest-convention
function is `add_guest`, hidden. Cross-object guest calls are rewritten
consistently by the translator (both sides are our output), so relocations
target `_guest` symbols. This breaks only when linking against
*non-transpiled* wasm objects providing the same symbol — which is exactly
the future interop case that needs adapter shims anyway.

### Interop: where the two conventions meet

What was first filed here as one project ("ABI lifting") is two. The first
is built; [archive/interop-plan.md](archive/interop-plan.md) holds the milestones and what
each settled.

**Interop** is thunks at the seams, and both seams were pre-cut by the
naming scheme above. A guest function's clean name already belongs to a
wrapper, so giving that wrapper a real signature makes it an ordinary wasm
function that anything can call. And a call to an undefined symbol `foo` is
already emitted as an import of `foo_guest`, so when `foo` turns out to be
foreign, *generating* `foo_guest` — read the argument registers, call the
typed import, write `rax` or `xmm0` back — is all that is missing.

Those thunks are emitted into a **separate object**, built from a view of
the whole link set (`zaqaru --thunks a.o b.o -o interop.wasm.o`). Inline
emission would be wrong rather than merely inconvenient: an object calling
`foo` would carry a typed import of `foo` that collides with the wrapper
another transpiled object defines under that name. Seeing the whole set also
means a symbol some object in it defines is never mistaken for foreign.

**The one thing a thunk must not forget** is the linker's stack pointer. The
guest stack and the shadow stack a wasm-native callee uses are the same
region of the same linear memory, both growing down from the same place, and
guest code moves only `x86_rsp`. A foreign call made without first setting
`__stack_pointer` below every live guest frame allocates its frame on top of
them. The thunk sets it and restores it, and that turns out to buy
re-entrancy as well: a foreign function that calls *back* into a transpiled
export gets a fresh guest stack below its own frame rather than on top of
the outer one.

**Signatures for the functions we translate come from inference**, not from
DWARF. The binaries this project aims at are stripped — debug info is large
and removing it is the default — so the analysis reads argument registers out
of the machine code: which are live at entry, closed under SysV's in-order
assignment, as a fixpoint over the call graph because `f(x) { return g(x); }`
compiles to a bare `jmp` and never touches its argument register at all.
DWARF, where it survives, is one more piece of evidence to agree with.

**Signatures for the functions on the far side are read, not inferred.** A
wasm object states an explicit type for every function it defines, and
interop means having that object in hand — it is what is being linked
against — so `--thunks` takes the wasm objects alongside the native ones and
reads their types. This is not a convenience: an argument passed straight
through leaves no trace on either side of the call, so
`f(x) { return g(x) + 1; }` at `-O2` genuinely does not record that `f` or
`g` takes anything. The information was never written down in the native
object, and it was written down in the wasm one.

Call-site inference — which argument registers a caller fills before a call,
and whether it reads a result afterwards — is therefore the *last* source,
for foreign functions no object defines, such as host imports. It is also
where the loud refusals come from: call sites that disagree with each other,
and calls that set the vector-count register, which is SysV saying the callee
is variadic and its arguments do not all travel in registers.

The order of authority is **declaration, then wasm object, then call-site
inference**. Declarations come first because they are the documented
override — the way to say something no object records.

**Inference refuses rather than guesses**, in three cases where the machine
code genuinely does not record the answer: a signature spanning both
register files (SysV fills the two independently, so `f(i32, f64)` and
`f(f64, i32)` are byte-identical), an unused trailing parameter, and a
result whose width only the caller knows. A declaration file supplies those;
declarations always beat inference. The reason to refuse is that at a
boundary a wrong signature is worse than no signature — no signature keeps
the uniform wrapper, which always works.

**Why inference-first is safe enough**: `wasm-ld` checks function types
across objects. A mis-inferred boundary signature is reported as a warning
(promoted with `--fatal-warnings`, which is part of the documented recipe) —
and even unpromoted, LLD refuses to connect the call, routing it through a
generated stub whose body is `unreachable`. There is no configuration in
which a wrong boundary signature quietly produces a working call. This was
measured before anything was built on it.

### Register promotion: locals inside, globals at the seams

Built — see [archive/promotion-plan.md](archive/promotion-plan.md) for the
milestones and their measured outcomes. The design rests on one
observation: **wasm locals are mutable**, so promotion needs no IR, no SSA
and no register allocator of ours. A one-local-per-cell mapping is
semantically identical to the globals at every point except where someone
outside the function can observe the state, and the engine — which builds
SSA from locals and register-allocates them — does the optimisation. The
whole transformation is a storage-class change plus a flush discipline:

- **Entry**: copy in every cell the function touches (from the same
  per-instruction register facts inference uses, so coverage is the whole
  instruction set, translated or not). A cell the scan misses stays in its
  global — slower, never wrong, because each cell resolves the same way
  for the entire function.
- **Escape points**, exhaustively: before a call (direct or indirect),
  flush every promoted cell the body may write; after it, reload every
  promoted register and XMM half. At a return, flush after the `rsp += 8`.
  A tail jump is a call plus a return and gets both. Reload-all is correct
  with zero interprocedural analysis because SysV callee-saved registers
  are preserved *through* the globals by the callee's own push/pop
  emulation.
- **Flags** are always promoted, never flushed at calls or returns, and
  never reloaded — the ABI makes all five call-clobbered, so nothing
  conforming reads one across a call. The narrowing is load-bearing: with
  a flush at returns, every loop iteration's flag computation becomes live
  (any iteration might be the last), and the measured cost is +33–62% on
  the affected kernels. The one exception is a **tail jump**, where flags
  are flushed and every function entry copies them in: compilers split one
  function across sections and conditionally jump between the halves, and
  the cold half may branch on what the hot half compared.

`--no-promote` disables it — the unpromoted configuration is the empty
promotion map, not a second code path — which pins any suspected
miscompile on the pass in one A/B run.

Measured on the four-kernel benchmark (`benches/kernels.rs`, wasmtime,
clang-native wasm as the ceiling): the integer and memory kernels went
from 5.1× and 2.1× the ceiling to **parity**; the call-heavy kernel from
4.7× to 2.5×, where the remainder is the guest stack's real memory
traffic, not state sync; the scalar-float kernel stayed at ~3.1× because
its time is per-operation translation cost of branchless SSE
(`cmpltsd`/`andpd`/`andnpd`/`orpd`), which promotion was never going to
touch — that gap belongs to a future translation-quality effort.

### Future: typed internal calls

What remains of the original "selective ABI lifting" idea: pass arguments
between transpiled functions as real wasm parameters using the inferred
signatures, so the hot path stops touching globals at calls entirely.
Deliberately not built with promotion, because it changes the failure
mode: a wrong signature at a module seam gets `wasm-ld`'s loud refusal,
but an under-inferred signature on an internal call silently drops an
argument. It needs its own safety story. Indirect calls promote last,
since every potential `call_indirect` target must agree on the signature.

Note that neither promotion nor typed calls fixes *data layout* interop
with wasm32 C (8-byte vs 4-byte pointers and `long`s in structs); that is
a separate, deeper problem, and it is why the interop corpus uses
fixed-width types throughout.

## The address-space split (a target-platform fact, not a backend choice)

x86-64 has one flat address space where code and data addresses are the
same kind of thing. Wasm has three: linear-memory addresses, table indices
(the only spelling of "function pointer"), and no addressing of code bytes
at all. Consequences, regardless of output mode:

- Jump tables and computed intra-function targets have no wasm-relocation
  spelling and must be consumed by the lifter (lowered to `br_table`).
  Helpfully, a `.rodata` jump table carries relocations against the
  `.text` section symbol with addends, so targets are recoverable
  *symbolically* — no concrete addresses needed. **Built**; see
  "Indirect transfers" below.
- Address-taken functions map to `R_WASM_TABLE_INDEX_*` relocations — the
  guest's "function pointer" value becomes a table index, exactly as
  clang-compiled wasm C does it. **Built**, together with indirect calls
  via `call_indirect`.
- Guest code reading its own code bytes has no clean spelling in the
  relocatable model and stays out of scope.

## Indirect transfers

### Function pointers

A function's address becomes its slot in the imported
`env.__indirect_function_table`, and an indirect call becomes
`call_indirect`. The emulated-register convention makes this far easier here
than it is for a typical lifter: every translated function has wasm type
`() -> ()`, so there is exactly **one** signature in the whole program and
the agreement problem that normally dominates indirect-call support does not
arise at all. A pointer names the callee's `_guest` entry point, which is
what `call_indirect` will invoke.

Consequences of a function pointer's *value* being a slot number rather than
an address:

- **Null still works.** `wasm-ld` leaves slot zero unassigned, so a null
  function pointer stays null and `if (ops->read)` behaves.
- **Round-tripping works**: stored, loaded, cast to `void *` and back,
  compared with another function pointer. That is what interface code does.
- **Ordering against a data pointer does not.** A slot number and a
  linear-memory address are unrelated quantities. No divergence has been
  observed in real C, and there is no way to do better in wasm.
- Struct layout stays x86's: a pointer field is still eight bytes, holding a
  four-byte slot number with a zero upper half. Guest code reading it with
  its own offsets stays self-consistent; only interop with clang-compiled
  wasm C, which uses four-byte pointers, is affected — and that is the
  separate data-layout problem noted above.

Taking the address of a function defined elsewhere reaches it through a
global offset table (`R_X86_64_GOTPCREL` and its relaxable variants). There
is no such table in the translated module, so the load is rewritten into the
address computation it stands for — exactly the relaxation a native linker
performs.

### Jump tables

A `switch` dense enough to compile to a table is the one construct that
cannot be translated at all: its entries are *code* addresses, which have no
wasm spelling of any kind. The dispatch has to be recognised and consumed.

The obvious approach — matching the instruction sequence to extract the
index register — does not survive contact with real compilers. gcc and clang
emit different shapes, each changes shape between `-O0` and `-O1`, and again
between position-independent and absolute code.

So the index is not recovered. The transpiler instead uses the one thing it
controls, the table's *contents*: entries are rewritten so that whatever the
guest's address arithmetic computes equals `table_address + arm`, and the
dispatch becomes a `br_table` over `computed − table_address`. Every
instruction that computes the target is then translated normally, with
nothing suppressed. What must be recognised is only *which* table a jump
reads, which comes from relocations.

Two rules keep that honest. A table is told from a vtable by where its
relocations land — a function pointer names a function's *start*, a table
entry names a block inside the dispatching function. And a table's extent
comes from its relocation run bounded by the next table's start, computed
across the whole object, because tables abut one another routinely and in the
relative form reading one as another's yields *wrong* targets rather than
surplus ones.

An indirect jump that reads no table is an indirect tail call. One that reads
something table-shaped whose entries cannot be read is an error, never a
call: translating it as one would branch somewhere arbitrary.

## Vectors and floating point

**Built**; what each milestone delivered is in
[archive/float-plan.md](archive/float-plan.md), and this section holds the
design and what fixed it. SSE is not optional for real binaries even before
any floating point appears: `-O2` fuses adjacent integer moves through
`movq xmm`, and struct copies go through `movdqa`/`movups`.

### XMM state: pairs of `i64` globals, not `v128` globals

The obvious representation — one `v128` global per XMM register — is dead
on arrival, established by experiment before implementation: **`wasm-ld`
cannot link an object that defines a `v128` global**, because LLD's object
reader has no case for a `v128.const` initializer (`invalid opcode in
init_expr: 253`), on LLVM `main` included. This is a hole in the format's
implementation, not a version gap, and stock `wasm-ld` is the project's
defining constraint.

So each XMM register is a **pair of weak `i64` globals** (`x86_xmm0_lo`,
`x86_xmm0_hi`, …), deduped across objects exactly like the integer register
file. A second experiment proved the rest of the path: SIMD instructions
and `v128` *locals* inside function bodies flow through `wasm-ld`
untouched — LLD copies code opaquely — and validate and run under wasmtime,
cross-object, bit-exactly.

The pair fits SSE's own grain. Scalar operations write the low 64 bits and
preserve the high 64: in pair state that is "touch only the `lo` global",
so the architecture's most common case is structurally correct rather than
carefully emulated. Packed operations assemble a `v128` local from the
pair, use wasm SIMD, and split the result back. The one real trap is the
move family's merge-vs-zero asymmetry (`movsd` from memory zeroes the high
lane, the register form preserves it; `movss` likewise at 32 bits), which
gets explicit per-form write masks and corpus cases that fail if a mask is
wrong.

### Scalar semantics: bit-exact, with three exceptions

Wasm and SSE are both IEEE-754 round-to-nearest-even (SSE's default MXCSR
mode), so scalar arithmetic maps directly and the differential tests
compare raw bits, no tolerances. Three places the naive mapping is wrong
and gets explicit emulation:

- `minsd`/`maxsd` return the *second* operand on ties and NaN; wasm
  `f64.min` propagates NaN and orders ±0. Compare+select, never the wasm
  ops.
- Truncating conversions produce x86's integer indefinite
  (`0x8000…`) on NaN/overflow; wasm `trunc` traps and `trunc_sat`
  saturates differently. Bounds-check+select.
- `ucomisd` reports through `ZF,PF,CF` — (1,1,1) unordered, (0,0,1) less,
  (1,0,0) equal — which requires the parity flag the MVP deliberately
  omitted; it joins the eager flag set, computed for integer results too
  (`popcnt` of the low byte).

All three were confirmed by deliberately breaking each one and watching the
corpus catch it; a case that would have passed either way is not a test of
the thing it names.

Where the guest *generates* a NaN, x86 produces `0xFFF8…` while wasm
engines canonicalize payloads nondeterministically; the harness compares
NaNs as a class and everything else as bits. The class rule is confined to
values arithmetic produced: `fabs`, negation and `copysign` are *bit*
operations, so a NaN has to come through them with its payload intact and
those are compared exactly. Not modelled, loudly: MXCSR rounding-mode
changes, and x87 entirely (`long double` only, on x86-64 — `float`/`double`
never touch it).

### Floats across the host boundary

SysV passes floats in `xmm0–7` and returns in `xmm0`, which the uniform
`(i64 ×6) → i64` wrapper cannot express. It grows to
`(i64 ×6, f64 ×8) → (i64, f64)` — fill every argument register of both
files, return both result registers, callers ignore what they don't want —
preserving the zero-type-knowledge property. Multi-value results got their
own de-risk spike first, in the style of the weak-global and `v128`
experiments; it passed, so the fallback of an exported accessor for
`x86_xmm0` was never needed. Guest-to-guest calls need none of this: all
state is global, so faithful emulation already carries floats between
translated functions.

A `float` occupies only the low half of its register, so its four bytes
travel in the low half of the `f64` that carries it — reinterpreted, never
converted, which is what makes the value arrive unchanged. The harness is
what knows which SysV slot each argument of a given C signature belongs in;
the transpiler still knows nothing about any function's real type.

## Relocatable wasm output

The emitter produces a structurally valid wasm module plus the LLVM
tool-conventions linking metadata:

- **`linking` custom section** (version 2): symbol table mirroring the ELF
  one (function symbols for `_guest` functions and wrappers, data symbols
  for `.data`/`.rodata`/`.bss` content, global symbols for the machine
  state), segment info, weak/hidden/exported flags per ELF binding and
  visibility.
- **`reloc.CODE` / `reloc.DATA` custom sections**: relocation entries
  (type, section offset, symbol index, addend).
- **Padded LEBs**: relocation sites inside code must be full-width
  (5-byte) non-canonical LEB128 immediates so the linker can patch in
  place. `wasm-encoder` emits canonical LEBs on its normal path, so
  patchable instructions go through raw-byte emission (`Function::raw`,
  `RawSection`). This is the main place we fight the crate.
- **Data**: `.data`/`.rodata` as data segments with segment-info entries;
  `.bss` as zero-fill.

**Reference-specimen tactic**: compile trivial C with
`clang --target=wasm32 -c` and treat the result as the known-good example —
`llvm-objdump`/wabt's `wasm-objdump` dump linking and reloc sections, so
our emitter's output is *diffed against what clang produces* rather than
debugged blind against wasm-ld's error messages.

### Relocation translation

| ELF relocation | Context | Wasm relocation emitted |
|---|---|---|
| `R_X86_64_PLT32` / `PC32` vs. function | `call` displacement | `R_WASM_FUNCTION_INDEX_LEB` on the wasm `call` immediate |
| `R_X86_64_PC32` vs. data | RIP-relative `lea`/`mov` | `R_WASM_MEMORY_ADDR_SLEB` on an `i32.const` |
| `R_X86_64_64` / `32` / `32S` vs. data | pointer in a data section | `R_WASM_MEMORY_ADDR_I32` in the data segment |
| `R_X86_64_64` / `32` / `32S` vs. function | a function pointer in a data section | `R_WASM_TABLE_INDEX_I32` in the data segment |
| `R_X86_64_GOTPCREL` and its relaxable variants | taking the address of a symbol defined elsewhere | whichever of the two above the symbol turns out to be |

**Addend adjustment (classic off-by-four factory):** `PC32`'s value is
*symbol + addend − site*; its typical `−4` addend cancels the
instruction-end offset. Lifting to an absolute `i32.const` must adjust the
wasm addend so the const resolves to plain *symbol + intended offset*. The
first data-access differential test exists to catch exactly this.

A jump table's relocations are the one kind never translated at all: they
name code, which has no linear-memory address, and the lifter consumes them
when it rewrites the table's entries.

## Instruction translation scope

The integer set is what `gcc/clang -O1` emits for integer code:

`mov`, `movzx`, `movsx`, `lea`, `add`, `sub`, `adc`, `sbb`, `imul`/`mul`
(including the one-operand widening forms, whose product fills a register
pair), `neg`, `not`, `and`, `or`, `xor`, shifts (`shl`/`shr`/`sar`), `cmp`,
`test`, `jcc`, `jmp`, `call`, `ret`, `push`, `pop`, `setcc`, `cmov`,
`cdq`/`cqo`, `idiv`/`div`, `endbr64` (as nop), `nop` family.

The SSE set is what the corpus turned out to demand across two compilers and
five optimisation levels — the move family and its write masks, the bitwise
and lane-rearranging families, scalar arithmetic, compares and conversions,
and the packed arithmetic, comparisons and shifts an auto-vectorised loop
produces. It is not enumerated here because it is not a fixed list: the loud
per-instruction error is what keeps it honest, and the sweep is what bounds
"done" by what compilers actually emit.

Notes:

- `lea` is pure arithmetic (no memory access, no flags).
- Division is fiddly: `idiv r32` divides the 64-bit `edx:eax` pair. The
  common `cdq; idiv` pattern is a faithful `i64` computation; overflow
  cases diverge from native `#DE` trap behavior (UB-adjacent in C; wasm
  traps on `INT_MIN / −1` via `i32.div_s` but the widened path differs).
  Documented divergence, revisited if it ever bites a test.
- Anything not implemented is a **hard translation error** naming the
  instruction — never a silent skip.

The test corpus is compiled with only the flags that switch off explicit
non-goals — control-flow protection, stack protectors, unwind tables, and the
`errno` setting that turns `sqrt` into a call into libm. Nothing narrows the
instruction set the transpiler is handed.

## Testing strategy

1. **Differential execution (the backbone)**: each corpus C file is
   compiled natively to a shared object (loaded via `libloading`) *and*
   transpiled + wasm-ld-linked + instantiated in wasmtime. Both are called
   with fixed and random inputs; results must match exactly. Every corpus is
   built forty ways — two compilers × two code models × five optimisation
   levels × two control-flow translations — because a transpiler meant for
   binaries it did not compile has to cope with what it is handed.
2. **Dual-mode oracle**: every CFG test runs through both the dispatcher
   and the structured translation; results must agree.
3. **Hand-written cases for what compilers rarely emit**: an irreducible
   graph, every write mask of the move family, the parity flag consumed
   after integer arithmetic, and every lane width of every packed family.
   Compiler output covers whichever members of a family that day's compiler
   wanted, which is not the same as covering the family.
4. **Optimisation sweep**: every corpus source transpiled at every
   configuration, reporting all failures rather than the first. This answers
   "is there anything the transpiler refuses", which is a different question
   from "does it compute the right thing".
5. **Snapshot tests**: `wasmprinter` renders emitted modules to `.wat` for
   readable diffs of the emitter's output.
6. **Specimen diffs**: linking/reloc sections compared structurally against
   clang-produced wasm objects.
7. **Linker acceptance**: every emitted object must round-trip through
   stock `wasm-ld` without warnings.

A test that passes is not by itself evidence that it *would* fail: where a
rule is subtle — a write mask, a tie-breaking rule, a lane width — the
translation was deliberately broken and the corpus watched to catch it.

**A corpus source must not be shaped to avoid an instruction**, because a
corpus that dodges what it finds inconvenient measures the corpus rather than
the transpiler. There is currently one place that does: `copy_unaligned` in
`vector_moves.c` copies a *constant* number of bytes, because a
variable-length `__builtin_memcpy` becomes `rep movsb` at `-Os` and the
string instructions are not implemented. It is recorded here rather than left
in the source alone, since the whole point of the rule is that exceptions to
it should be visible.

## Decision log

| Decision | Choice | Rationale | Revisit when |
|---|---|---|---|
| Output mode | Relocatable wasm object for wasm-ld | Separate compilation, real linker semantics, interop path | Emitting linking metadata proves disproportionately painful |
| Lifting | Symbolic (symbol+addend), no concrete addresses | Cleaner IR, required by relocatable output, testable | — |
| Pointer width | wasm32, `i64` math, wrap at one choke point | Best-trodden wasm-ld path; linker places data low | High-bit tricks in guests; 8-byte-pointer interop needs |
| Register file | `i64` globals, weak symbols in every object | Convention-agnostic correctness; linker dedupes | Weak-global dedup fails in wasm-ld (→ companion object); later: selective promotion to locals |
| Flags | Eager ZF/SF/CF/OF globals | Simple, correct incl. cross-block/cross-ret liveness | Perf work begins (→ global flag liveness pass) |
| Control flow | Dispatcher first, then dominator-based structurer; dispatcher stays as fallback + oracle | End-to-end fast; irreducible safety net; built-in differential oracle | — |
| Calling convention | Emulated registers internally; typed/uniform wrappers at export boundary | Assumes nothing about ABI; wrong signatures corrupt silently | Selective ABI lifting stage |
| Guest stack | In linear memory; `x86_rsp` init from imported `__stack_pointer` | Linker owns stack allocation; zero machinery | Cross-calls with wasm-C-ABI code sharing the stack |
| IR | Annotated x86 CFG, no new instruction set | MVP honesty; avoid speculative design | Selective ABI lifting / optimization work |

### Decisions taken while building the MVP

These were settled by implementation rather than up front; they are recorded
here so the reasons do not have to be rediscovered.

| Decision | Choice | Rationale | Revisit when |
|---|---|---|---|
| Wasm encoding | Hand-rolled (`emitter/binary.rs`), not `wasm-encoder` | Every relocation site needs a padded LEB *and* its byte offset within the code section; mixing raw bytes into the crate's encoders coupled more than writing the ~40 opcodes we use. `wasmparser` validation and `wasmprinter` snapshots are the safety net instead | The emitted instruction set grows past what one file can hold clearly |
| Terminator ownership | The structurer translates `jcc`/`jmp`/`ret`; the translator handles everything else | The structured translation is recursive — a branch may inline its target's whole subtree — which a callback interface into the translator cannot express without aliasing it | — |
| Global references | Every register and flag access emits a 5-byte global index plus a relocation | Inherent to relocatable output: the linker renumbers globals when it merges objects. Objects are correspondingly large; `wasm-ld` garbage-collects the globals a module never touches | — |
| Call targets | Resolved by symbol *and* by section offset | The assembler resolves a call to a function in the same section itself, leaving no relocation to read | — |
| Data segments | One per input section, with a local `<section>.whole` symbol | Preserves every intra-section offset, including references past the end of any single symbol. A per-symbol split would break string pooling and table interiors | Garbage collection at symbol granularity becomes worth the complexity |
| `imul` flags | 64-bit overflow from a 128-bit high product built out of four 32-bit partial products | Branch-free and trap-free, unlike the division-based test; `__builtin_mul_overflow` really does read this flag | — |
| `imul` sign/zero flags | Given the obvious values where the architecture leaves them undefined | Cheaper than modelling "undefined" and no program may depend on the difference | — |
| Division | Exact for widths up to four bytes, including the divide-error traps; the eight-byte form handles a dividend that fits in 64 bits and traps otherwise | 128-bit division is a large piece of work for a case compilers do not emit — `cqo` and a zeroed `rdx` both produce fitting dividends | Guest code divides a genuinely 128-bit dividend |
| Unwind tables | `.eh_frame` and `.gcc_except_table` are never translated | They describe code, and hold relocations against text symbols that have no linear-memory address | Unwinding comes into scope |
| Indirect calls | One `call_indirect` type for the whole program | Every translated function is `() -> ()`, so signature agreement — normally the hard part — cannot fail | Selective ABI lifting introduces real signatures |
| Jump tables | Rewrite the entries rather than recover the index | Compilers emit at least four dispatch shapes across two compilers, two code models and the optimisation levels; entry contents are ours to choose, instruction shapes are not | A compiler computes the target in a way that is not `table + entry` |
| Table extent | Relocation run, bounded by the next table, computed object-wide | Tables abut; a per-function bound reads one table's entries as another's, which in the relative form gives wrong targets, not surplus ones | — |
| Global offset table | A load through a slot is rewritten as the address computation it stands for | There is no such table in the output, and the relaxable relocation types exist precisely to license this | — |
| Conditional tail calls | A branch out of the function becomes `if (cond) { call; return }` | `-O2` splits cold paths into `.text.unlikely` and reaches them with `jcc` | — |

### Decisions taken for vectors and floating point

Settled by the two pre-plan experiments and the design work recorded above.

| Decision | Choice | Rationale | Revisit when |
|---|---|---|---|
| XMM representation | Pairs of weak `i64` globals, never `v128` globals | LLD cannot parse a `v128.const` initializer, on LLVM `main` included — verified by experiment; SIMD stays available inside bodies, which LLD copies opaquely | LLD learns `v128` init expressions *and* pair-state cost shows up somewhere that matters |
| Scalar float ops | Reinterpret the `lo` global; `hi` untouched | Wasm and SSE share IEEE-754 round-to-nearest-even, so arithmetic is bit-exact; scalar SSE preserves the high lane, which pair state gives structurally | — |
| min/max and truncating conversions | Explicit compare/select emulation, never the wasm ops | x86 picks the second operand on ties/NaN and produces the integer indefinite where wasm traps or saturates — silent divergence otherwise | — |
| Parity flag | Joins the eager flag set, `popcnt` of the result's low byte | One wasm instruction per flag-setting site; `ucomisd` reports unordered through PF and compilers branch on `jp` immediately | Flag-liveness optimization (same trigger as the other four) |
| Host boundary | Uniform wrapper grows to `(i64 ×6, f64 ×8) → (i64, f64)` | Fills both register files with zero per-function type knowledge, exactly as the integer-only shim does | DWARF-typed wrappers land |
| NaN fidelity | Differential harness compares NaNs as a class, all else as raw bits | Wasm engines canonicalize *generated* NaN payloads nondeterministically; x86 produces `0xFFF8…` — bit comparison there tests the engine, not the translation | — |
| MXCSR and x87 | Not modelled; consuming instructions stay loud errors | Wasm is fixed to SSE's default rounding mode; x87 carries only `long double` on x86-64 | A real binary changes rounding modes or takes `long double` seriously |

### Decisions taken while building vectors and floating point

| Decision | Choice | Rationale | Revisit when |
|---|---|---|---|
| Lane rearrangement | One translation over a table saying where each of the result's four doubleword lanes comes from | `pshufd`, both `unpck` families, `shufps` and `shufpd` are the same operation with different tables; writing them as one is what made adding each cheap enough not to guess | A shuffle appears whose result is not a permutation of whole doubleword lanes |
| Packed operations | Assemble a `v128` from the pair, one wasm SIMD instruction, split back | Proven by the pre-plan spike; the alternative of lane arithmetic on the pair is only simpler where the lanes are already 64 bits wide | — |
| Whole-register byte shifts and `pmuludq` | Pair arithmetic, not SIMD | Both land on the pair's own grain — a byte shift is a window onto a 128-bit value, `pmuludq` is one 32-by-32 product per half — and neither has a wasm instruction that matches | — |
| Packed shift counts | Immediate forms only; a count reaching the lane width is settled at translation time | x86 zeroes the result there while wasm reduces the count modulo the width, and the count is known, so no run-time test is needed. Register-count forms are loud errors, which is what compilers never emit | A compiler emits the register-count form |
| Untyped data symbols | Any defined symbol in a section that became a data segment is addressable | Floating-point literal pools arrive as untyped local labels with no size; the ELF type field does not distinguish them from anything else, and a text section never becomes a data segment so nothing else can be caught by the widening | — |
| Coverage of instruction families | Every member of every family implemented gets a hand-written case | Demand-driven implementation cannot see what it has not been asked for: thirty-one translations were written as complete families while the corpus reached only some, and a mutation proved the rest untested. Finding that immediately caught a real bug | — |
| Assembly corpus sources | Compiled with no flags at all | The C flags mean nothing to an assembler; gcc ignores them silently and clang warns, and a warning is a failure here | — |

### Decisions taken for interop and signature inference

| Decision | Choice | Rationale | Revisit when |
|---|---|---|---|
| Signature source | Inference from machine code, primary; DWARF and declarations as cross-checks | The target is external binaries, which ship stripped — a DWARF-first design would be a design for inputs this project does not have | — |
| Safety of a wrong signature | Rely on the linker: `wasm-ld` type-checks across objects and routes a mismatch through a trapping stub | Measured before building on it. Turns "a wrong signature corrupts silently" — true *inside* an object — into "a wrong signature fails twice over" at a boundary | LLD stops checking, or starts connecting mismatches |
| Where thunks live | A separate object built from the whole link set (`--thunks`) | Inline emission collides a typed import of `foo` with the wrapper another object defines under that name; the set-wide view is also what tells foreign from merely-undefined | — |
| Stack pointer at a foreign call | The thunk hands `__stack_pointer` down below every live guest frame and restores it after | The guest stack and the callee's shadow stack are one region growing down from one place; without this a foreign frame lands on live guest frames. Also what makes foreign-to-guest callbacks work | — |
| Ambiguous signatures | Refuse, and keep the uniform wrapper | At a boundary no signature is strictly better than a wrong one: the uniform shim always works, and a declaration is one line | A source of the missing order appears — DWARF, or a caller in the same link set |
| Register effects | From iced-x86's instruction information, not from the translator | The analysis must cover instructions the translator cannot handle, which is the main case rather than a corner: a stripped binary is most worth asking about before it fully translates | — |
| Value tracking | One fact — where a value came from — through full-width moves and stack slots only | Pointer evidence lands on a register that is not the argument register; following sign-extending moves or full arithmetic mistyped narrow arguments. Bounded on purpose, as the alternative is an IR nothing yet needs | *Trigger fired and resolved the other way*: register promotion arrived and needed no IR, so the bound stands until some consumer genuinely needs more |
| An IR | Still not built — twice now: the trigger was evaluated here and did not fire, and register promotion, the predicted consumer, turned out not to need one (wasm locals are mutable; the engine builds the SSA) | Inference is read-only and changes no emitted byte, so an IR would have been paid for at the point of least benefit | A pass that must *rewrite* instruction sequences — the scalar-SSE translation-quality work, or typed internal calls — finds itself pattern-matching emitted wasm |
| Foreign signatures | Read from the wasm object being linked against; inference is the fallback | An argument passed straight through leaves no trace on either side of the call, so the native objects genuinely do not record it — and the far side's object states it outright. Inference at a boundary is better spent on refusals than on guesses | A foreign function has no object at all, which is what the fallback is for |
| Order of authority | Declaration, then wasm object, then call-site inference | A declaration is the documented override for what no object records; an object is knowledge; inference is evidence | — |
| Variadic callees | Detected by the vector-count register and refused | Their arguments do not all travel in registers, so no thunk can carry them; guessing a fixed signature would produce a call that links and misbehaves | Stack arguments are carried at all |

### Lessons from building inference

Recorded because each one cost a debugging cycle and none of them is
obvious from the instruction set:

- **The obvious signal is usually the wrong one.** Five separate rules had
  to be corrected against real compiler output — `lea` is arithmetic and not
  addressing, a register *written* is not evidence about the value that
  arrived in it, address use has to beat 64-bit use because gcc walks arrays
  with 64-bit `lea`, and a result register that is written is not thereby a
  result, because clang aligns the stack with `push %rax`/`pop %rax` after
  the floating-point work. Every one was found by grading against known
  signatures; none came from reasoning about the ISA.
- **Locations are too coarse to filter on.** `movslq %edi,%rdi` reads EDI
  and writes RDI, which are the same *location*. Anything asking "is this
  location read here" lets the write through and concludes the incoming
  value was 64 bits wide. Per-register access is the distinction that
  matters.
- **Grade against ground truth at every configuration, not at one.** Of the
  divergences found, most appeared at two or three of the twenty
  configurations and would have been invisible in a single-compiler,
  single-optimisation test. The differential harness already knew every
  corpus function's true signature; making inference answer to it was
  cheaper than any amount of inspection.
- **Ask where the information actually is before building an analysis to
  recover it.** Two milestones of caller-side inference were planned to
  recover foreign signatures, and the answer turned out to be that the
  native objects do not contain them and the wasm object states them
  outright. The analysis was still worth building — it is what refuses a
  variadic call and what catches call sites disagreeing — but it was the
  wrong thing to have made primary.
- **Write down what is out of reach, with the reason, next to the
  assertion.** Two functions in the grading corpus cannot be inferred
  correctly, and recording their actual output as "expected" would have made
  the limitation stop looking like one. They are named and excused instead,
  and the test fails if either starts working — which is the intended way to
  find out.

### Decisions taken for register promotion

| Decision | Why |
|---|---|
| No IR, no SSA, one local per cell | wasm locals are mutable, so promotion is a storage-class change; the engine builds SSA from locals and register-allocates them. Building our own middle-end would duplicate the consumer |
| Promotion is strictly intra-function; globals remain the convention at every seam | relocatable objects link against other objects that read the globals; nothing about the interop mechanism moves |
| Touched-set copy-in, written-set flush, reload-all after calls | correct with zero interprocedural analysis: SysV callee-saved registers come back through the globals, restored by the callee's own push/pop emulation |
| A cell the effects scan misses stays in its global | under-approximation is safe by construction — every access to a cell resolves the same way for the whole function — so the facts layer never has to be perfect, only honest |
| Flags are call-clobbered: never flushed at calls or returns, flushed at tail jumps, copied in at entry | SysV licenses the narrowing; the tail-jump exception is forced by the compiler's cold-path split; and mutation testing measured what the narrowing protects — +33–62% forfeited when flags flush at returns |
| `--no-promote` is the empty promotion map, not a second path | an A/B bisection tool for miscompiles at zero maintenance cost |
| Jump tables moved from `HashMap` to `BTreeMap` | the milestone's byte-identity gate found the emitter was already nondeterministic; deterministic output is table stakes for a toolchain |

### Lessons from building promotion

- **A byte-identity gate can catch bugs older than the refactor it
  guards.** Requiring byte-identical output across 198 configurations for
  the choke-point refactor exposed pre-existing nondeterminism: jump
  tables were emitted in `HashMap` iteration order, so the same binary
  produced different bytes on different runs. The gate exists to prove a
  refactor changed nothing; here it proved the baseline itself was not a
  fixed point.
- **The flag win lives at loop exits, not in straight lines.** The
  expectation was that not flushing flags lets the engine delete dead flag
  computations between instructions. The measured mechanism is stronger:
  a flush at return makes flag locals live *out of the loop*, and since
  any iteration may be the last, every iteration's computation becomes
  live. Removing the flush is what lets loop-exit liveness collapse —
  worth +62% on one kernel.
- **Mutate the discipline, not the decision.** The first version of the
  "delete one entry copy" mutation removed the register from the promotion
  map instead — and every test passed, because an unmapped cell falls back
  to its global by design. The battery accidentally proved the fallback
  property, and the corrected mutation (promote without copying) was
  caught at once. A mutation that survives may be testing the wrong claim,
  not missing a test.
- **Know which budget a gap lives in before optimizing it.** Promotion
  removes state-synchronisation cost and nothing else. The integer and
  memory kernels, whose gap *was* state traffic, went to parity with
  clang-native wasm; the float kernel, whose gap is per-operation
  translation cost of branchless SSE selects, did not move. The benchmark
  said which was which before any effort was spent in the wrong place.
- **A promotion bug can present as a hang.** Losing an argument to a
  missing entry copy sent a corpus function into an infinite loop rather
  than a wrong answer, and the wasmtime harness has no execution deadline
  — the suite relies on the runner's timeout. Worth knowing before
  reading a stuck test run as anything else.

## Status: what is built, and what is left

Built and tested differentially, across both compilers, both code models,
`-O0` through `-O3` and `-Os`, and both control-flow translations: the
integer instruction set above, calls and returns, data, jump tables,
function pointers and indirect calls, structured and dispatcher control
flow, and the whole of "Vectors and floating point" — see
[archive/implementation-plan.md](archive/implementation-plan.md) and
[archive/float-plan.md](archive/float-plan.md) for what each milestone of
those two efforts delivered.

Interop with ordinary wasm is built and tested the same way — see
[archive/interop-plan.md](archive/interop-plan.md) for what each of its
milestones delivered. A transpiled object and a clang-compiled wasm object
link together and call each other in both directions, in a corpus that
carries one declaration in total: pointers cross both ways, a foreign callee
calls back into a transpiled export with a live frame across the call, and
signatures come from the machine code and from the far side's own object
rather than from anything written by hand.

Register promotion is built, on by default, and measured — see
[archive/promotion-plan.md](archive/promotion-plan.md) for the milestones
and `benches/kernels.rs` for the standing benchmark. Function bodies run
on locals and touch the shared globals only at entry, calls and exits;
the integer and memory benchmark kernels sit at parity with clang's own
wasm backend, the call-heavy kernel at 2.5× it, and the scalar-float
kernel at 3.1× for reasons that are translation quality, not state
traffic. The flush discipline is mutation-tested: every deletable piece
of it makes the differential suite fail, and the one deliberate narrowing
— flags do not cross calls — is invisible to every behavioural test and
worth up to 62% on flag-heavy loops.

What follows is what a real binary would hit next. It is written down
because "anything not implemented is a hard translation error naming the
instruction" makes the gaps discoverable but not *visible*: nothing tells
you what is missing until you feed it something that needs it. The list
below was produced by feeding the transpiler each of these deliberately.

### Instruction gaps

**Integer.** `rol`/`ror`, `bswap`, `popcnt`, `xchg`, and the `rep
movs`/`stos` string family. This is the group that would bite first, and it
is worth being blunt about why: the SSE sweep is clean across every
configuration the corpus is built at, so what is missing there is what this
corpus does not happen to contain rather than what compilers do not emit —
whereas `rol` is what gcc reaches for to swap adjacent words at `-O2`, and
only the particular shapes the corpus settled on keep it out.

**SSE.** The long tail the float plan bounded by demand rather than by
enumeration: packed `min`/`max`, `pmovmskb`, `palignr`,
`pshuflw`/`pshufhw`, the packed conversions (`cvttpd2dq`, `cvtpd2ps`,
`cvtps2pd`), SSE4's integer minima and maxima, horizontal adds, blends,
`pabsd`, `ptest`. Packed `min`/`max` deserve a warning of their own: they
carry the same second-operand-on-ties-and-NaN rule as their scalar forms, so
they are not a matter of reaching for `f64x2.min` — that is precisely the
mapping the scalar design rejects. Register-count packed shifts are a loud
error; only the immediate forms compilers emit are translated. AVX and the
VEX encodings are untouched entirely.

**Deliberately not modelled**, and staying that way until something real
needs them: MXCSR (`ldmxcsr`/`stmxcsr`), x87, and the adjust flag.

### Structural gaps

- **The host boundary** carries six integer and eight floating-point
  arguments, whether it wears the uniform shim or a typed face. Anything
  past that travels on the stack, which neither can express — as do varargs
  and by-value structs.
- **Signatures spanning both register files** cannot be inferred, because
  SysV fills the two independently and nothing records how the source
  interleaved them; they have to be declared. So do unused trailing
  parameters, and results whose width only the caller knows.
- **Host-to-guest callbacks**: a function pointer handed *in* needs an
  adapter that speaks the emulated convention.
- **Division** still traps on a genuinely 128-bit dividend, where hardware
  would divide.

### Designed-for, not built

Typed internal calls — arguments passed between transpiled functions as
real wasm parameters on inferred signatures, the remaining half of what
was once called "selective ABI lifting" (register-to-local promotion, the
other half, is built); a translation-quality pass for scalar SSE select
idioms, which is where the float benchmark's remaining 3.1× lives; the
memory64 flag. The flag-liveness optimization this list used to carry is
done, by a different mechanism than planned: promotion keeps flag stores
inside the function, and the engine's own liveness does the deleting.
