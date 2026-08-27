# zaqaru — wasm interop and signature inference: implementation plan

Status: **complete** — every milestone's acceptance criterion passes;
archived on 2026-08-26. What the work settled is recorded in
[design.md](../design.md)'s "Interop: where the two conventions meet" section
and its decision log.
Date: 2026-08-26

Companion to [design.md](../design.md), whose "Calls and ABI" section holds the
*why* of the emulated convention; this holds the *what and in what order* for
making transpiled code call, and be callable by, ordinary wasm. The shape
mirrors the archived [MVP plan](implementation-plan.md) and
[float plan](float-plan.md): each milestone has an acceptance
criterion and is done when the criterion passes, not before.

## Outcome

All five milestones landed, in order, each verified against its own
criterion.

| Milestone | Acceptance | Outcome |
|---|---|---|
| 1. Groundwork spikes | Both spikes pass and stand as tests; the mismatch case fails the link | Passes, and the answer was better than assumed: `wasm-ld` reports a signature mismatch as a *warning*, but refuses to connect the call regardless, routing it through a trapping stub. There is no configuration in which a wrong boundary signature quietly produces a working call |
| 2. The boundary mechanism | The mixed corpus passes across the full matrix with declarations supplying every signature | Passes. Re-entrancy fell out for free — because the sync lives in the thunk rather than the wrapper, a foreign function calling back into a transpiled export gets a fresh guest stack below its own frame. The transpile sweep also found the one-operand widening `imul`, which was implemented rather than dodged |
| 3. Callee-side inference | Inferred argument lists match the C signature for every global function at every configuration | Passes: 340 graded signatures across twenty configurations. Five rules had to be corrected against real compiler output, none of which were obvious from the instruction set; pointer detection needed one bounded fact — where a value came from — followed through moves and spills. The sweep found `adc`/`sbb` on the way |
| 4. Caller-side inference | The outgoing declarations are deleted and the corpus still passes | Passes, **by a different mechanism than planned**. An argument passed straight through leaves no trace on either side of the call, so no analysis of the native objects can recover it — but the wasm object being linked against states the type outright. Reading it is the primary path; call-site inference became the fallback for host imports and the source of the refusals |
| 5. The standing corpus | Every direction covered with inference supplying every signature; every listed mutation caught | Passes. Seven of eight mutations caught; the eighth exposed a real gap — a stack pointer set but never restored, which no single call can see — and the test written for it makes three thousand crossings. The remaining mutation is unobservable rather than untested, and is recorded as such |

The repository gained four files: `src/abi/` (signatures, register effects,
inference, marshalling), `src/thunks.rs`, `src/wasm_reader.rs`, and
`tests/inference.rs`.

One thing this plan got materially wrong, and it is the most useful thing in
it: two milestones were spent building caller-side inference to recover
foreign signatures, and the answer turned out to be that the native objects
do not contain them and the object on the far side states them outright. The
analysis still earns its place — it is what refuses a variadic call and what
catches call sites disagreeing — but it was the wrong thing to have made
primary. *Ask where the information is before building an analysis to
recover it.*

## Why this is not optional

A transpiled module is currently an island. Its functions have wasm type
`() -> ()` with all state in the emulated register file, so another wasm
module cannot call them, and a guest call to a symbol no transpiled object
defines has no way to reach a function that clang's wasm backend — or a
hand-written wat file, or a host-provided import — actually provides. The
end goal of the project is external binaries, and an external binary is
useful precisely because it gets linked into a program that has other parts.

## Interop needs thunks, not lifting

design.md files this work under "selective ABI lifting", but that name
bundles two different projects:

- **Interop** — the two conventions meeting correctly at seams. A seam is by
  definition a thunk: even a fully lifted module needs one wherever an
  emulated caller, an indirect call, or a foreign module touches a lifted
  function. This plan is the interop project.
- **Lifting** — promoting emulated registers to real wasm parameters and
  locals inside function bodies so the engine can optimise. That is a
  performance project. It stays out of scope here, it reuses this plan's
  signature analysis wholesale when it comes, and it starts with a
  benchmark, not a hunch — globals are cheap in current engines and the
  guest's own spill code stays in the translated body either way, so the
  size of the win is an open question.

What the two share — and what this plan mostly is — is knowing signatures.

**Inference is the primary path.** The external binaries this project aims
at ship stripped; debug info is large and the default is to remove it. So
DWARF, where it happens to survive, is one more piece of evidence to
cross-check against, never the plan of record. Nothing below depends on it.

## The seams already exist

Both boundary directions were pre-cut by the MVP's naming scheme, which is
why this plan adds mechanisms at the edges and changes nothing in the
translator:

- **Incoming** (foreign wasm calls us): the host-entry wrapper already owns
  the clean ELF name and marshals into the register file. Today it is the
  uniform `(i64 ×6, f64 ×8) → (i64, f64)` shim; given a signature it
  becomes a *typed* wrapper — `(i32, f64) → i32` reads like any other wasm
  function. Transpiled-to-transpiled calls never touch the wrapper (they
  target `_guest` symbols directly), so typing it changes nothing for them.
- **Outgoing** (we call foreign wasm): every guest call to an undefined
  symbol `foo` is already emitted as an import of `foo_guest`. When `foo`
  is foreign, *generate* `foo_guest`: a thunk that reads the argument
  registers from the globals, calls typed import `foo`, and writes `rax`
  or `xmm0` back. The seam was waiting for a definition.

### Outgoing thunks live in their own object

Thunks are emitted into a **separate thunk object**, not into the
transpiled objects — a second mode that reads the same ELF inputs (all of
them, the whole intended link set), finds the function symbols undefined
across the set, and emits one wasm object defining their `_guest` thunks:

```sh
zaqaru a.o b.o -o …             # per-object, as today
zaqaru --thunks a.o b.o -o interop.wasm.o
wasm-ld a.wasm.o b.wasm.o interop.wasm.o foreign.o -o out.wasm
```

Two reasons. First, a transpiled-to-transpiled link must stay exactly as it
is: if thunks were emitted inline for every undefined symbol, an object
calling `foo` would carry a typed import of `foo` that collides with the
uniform wrapper another transpiled object defines under that name. Second,
caller-side inference wants exactly the whole-link-set view the thunk mode
receives — a symbol defined by another object in the set needs no thunk,
and every call site to a genuinely foreign symbol is evidence.

### Correctness details the thunk cannot skip

- **The return-address slot.** The `call` translation pushes a sentinel and
  relies on the callee's `ret` to pop it. The thunk stands in for that
  callee, so it does `rsp += 8` itself before returning.
- **`__stack_pointer` sync.** The guest stack grows down from where
  `__stack_pointer` stood at host entry, but the global itself is never
  moved — so a foreign callee using the shadow stack would allocate its
  frame on top of live guest frames. The thunk sets
  `__stack_pointer = x86_rsp` aligned down to 16 before the call and
  restores the old value after.
- **Widths.** i32 arguments zero-extend into the 64-bit globals (the
  callee's own width-correct reads make the choice of extension
  unobservable); i64 results from `rax` wrap to i32 where the signature
  says so. `float` travels as the low 32 bits of the xmm low half,
  reinterpreted, exactly as the uniform shim already does for `f64`.
- **Pointers.** Guest pointers are i64-held offsets into the *same* linear
  memory and the target is wasm32, so they fit: wrap going out, zero-extend
  coming in. This is what makes pointer-passing interop work at all — both
  sides address one memory.

## What inference is

Two analyses, one per direction, sharing an evidence vocabulary.

**Callee-side — for functions we define** (feeds typed export wrappers):
which argument registers are live-in, i.e. read before written along some
path. Interprocedural, because `f(x) { return g(x); }` at `-O2` is
`jmp g` and never touches `rdi` itself: liveness propagates through calls,
as a fixpoint over the call graph with SCCs for recursion and unknown
callees treated as reading every argument register. The raw result is a
register *set*; the SysV prefix rule turns it into an arity — if `rdx` is
live-in, `rdi` and `rsi` are arguments even if unused. Return types cannot
be seen from inside the callee (`rax` always holds *something* at `ret`),
so return evidence comes from **internal call sites** of the same function
where they exist, and otherwise defaults to `i64` — a guess whose failure
mode is the loud one described below.

**Caller-side — for undefined symbols** (feeds outgoing thunks): at each
call site, which argument registers are definitely written before the call,
and whether `rax`/`xmm0` is read after it; unioned and cross-checked across
every site in the link set. Sites that disagree are an error naming the
sites, not a majority vote.

> **Superseded in part.** Milestone 4 found that this cannot recover an
> argument passed straight through — neither side of such a call mentions
> the register — while the wasm object being linked against states the type
> outright. Reading that object is the primary path; call-site inference is
> the fallback for foreign functions no object defines, and the source of
> the refusals. See milestone 4's outcome.

Evidence quality varies by kind, and the plan leans on the strong kinds:

- **Float widths come free**: `movss` vs `movsd` into an argument register
  says f32 vs f64 outright.
- **Pointers have signals**: a value used as the address of a memory
  access, carrying a data relocation, or derived from `rsp` is a pointer —
  which on a wasm32 target means i32, even though the x86 caller writes
  all 64 bits. Without this rule every pointer argument would infer as
  i64 and mismatch, systematically.
- **Integer widths are fuzzy**: `edi`-vs-`rdi` access is a hint, not
  proof. Per-slot evidence, i64 when mixed, and the report (below) shows
  which slots were guessed.
- **Varargs are detectable**: `al` set immediately before the call is the
  SysV vector-count protocol, and it means *refuse loudly*, not guess.

### Why inference-first is safe enough: the linker checks

The design doc's standing objection — "a wrong signature is not an error
but silent corruption" — is about *internal* lifting, where both sides of a
mistake are our own output. At the boundary there is a third party:
`wasm-ld` checks function signatures between defined and undefined symbols.
An outgoing thunk that imports `foo` with the wrong type, or a typed export
whose type disagrees with what the foreign caller declared, is a link-time
diagnosis, not corruption. Wrong arity, wrong width, a dropped unused
trailing argument, a guessed return type — all of these surface as a type
disagreement with the other module. The residual silent zone is a mistake
that coincidentally type-matches the foreign side's expectation, a far
narrower target. This claim is load-bearing, so it is milestone 1's spike,
and if LLD's check turns out to be a warning rather than an error, the
harness and the documented link recipe promote it with `--fatal-warnings`.

Two backstops complete the safety story:

- **Declarations as the exception path.** A sidecar file mapping symbol
  names to signatures. Declarations override inference; inference
  cross-checks declarations (a declaration contradicting observed register
  use is an error, not a shrug). The expected workflow is: link, read the
  linker's complaint, write one line.
- **The inference report.** Every emitted signature with its evidence —
  which registers, which widths, which sites, what was guessed — rendered
  through `--dump`, so results are auditable instead of trusted. DWARF,
  when present, joins as one more cross-check with the same
  disagreement-is-an-error rule.

One asset makes inference testable from day one: the differential harness
already knows the true C signature of every corpus function (its `Arguments`
builder is the harness's SysV knowledge). Inference can therefore be graded
against ground truth across the whole existing corpus at every optimisation
level, before any interop machinery consumes it.

## Milestones

### 1. Groundwork spikes: the linker as type checker, one call each way

In the style of the `v128` and multi-value spikes: settle what the design
leans on before building on it.

- **Signature checking.** Link a transpiled-style object against a
  clang-wasm object with a deliberately mismatched function type, in both
  directions. Confirm the mismatch is diagnosed; determine error versus
  warning; pin the behaviour in a test (with `--fatal-warnings` in the
  recipe if needed).
- **One cross-convention call each way, hand-built.** A clang-wasm caller
  invoking a transpiled export through a hand-written typed wrapper, and a
  transpiled guest calling a clang-wasm function through a hand-written
  `_guest` thunk — including the `__stack_pointer` sync, with the foreign
  callee written to use enough shadow stack that a missing sync corrupts a
  live guest frame. This proves the seam mechanics with zero zaqaru
  changes.

**Accept:** both spikes pass and live on as standing tests beside the
weak-global and multi-value experiments; the mismatch case demonstrably
fails the link.

**Kill-point:** if `wasm-ld` does not check signatures at all, the loud
failure story collapses; the fallback is declarations required at every
boundary plus our own transpile-time verification against them, and the
inference milestones are re-scoped from "primary source" to "cross-check".

**Outcome: passes** (`tests/interop_spikes.rs`, four tests). The measured
answer on signature checking is *better* than this plan assumed, and the
difference matters enough to record:

- `wasm-ld` reports a mismatch as a **warning**, not an error, so
  `--fatal-warnings` is part of the documented recipe as anticipated. (The
  test harness already promotes warnings to failures independently.)
- But even unpromoted, LLD **does not connect the call**. It routes it
  through a generated `signature_mismatch:<name>` stub whose body is
  `unreachable`. A mis-inferred signature therefore fails twice over: at
  link time if the warning is read, and at the call itself if it is not.

So the safety claim is stronger than "the linker warns": there is no
configuration in which a wrong boundary signature silently produces a
working-looking call. Both spike directions link and run, and the unsynced
negative control confirms a withheld `__stack_pointer` really does destroy
a live guest frame — the sync test is testing something.

### 2. The boundary mechanism, driven by declarations

Mechanism before analysis: build both thunk directions with signatures
supplied by the sidecar file, so every marshalling detail is settled and
tested before inference exists to feed it.

- The declaration file format, minimal: symbol name, parameter types,
  result type.
- **Typed export wrappers.** When a signature is known for an exported
  function, the clean name carries the typed wrapper. The uniform shim
  remains for functions without one — typed when known, uniform when not,
  per function. Typed faces carry bits exactly (i64 params carry the same
  bits the uniform shim did; `f64`/`f32` params reinterpret into the xmm
  low half), so the harness's bit-level comparisons survive the move.
- **The thunk mode.** `--thunks` over the link set emits the thunk object:
  for each foreign function, a `foo_guest` doing the return-slot pop,
  argument marshalling from the globals, `__stack_pointer` sync, typed
  call, result write-back.
- The differential harness learns mixed programs: a corpus pair where one
  source is compiled natively *and* with clang `--target=wasm32`, the
  other transpiled; the native oracle links both natively, the wasm side
  links transpiled + clang-wasm + thunk object. Same inputs, exact
  agreement, both call directions. Corpus sources use fixed-width types
  so the source means the same thing under LP64 and ILP32.

**Accept:** the mixed corpus — integer, float (both widths), and
pointer-passing cases in both call directions — passes differentially
across the full matrix, with declarations supplying every signature.

**Outcome: passes** (`tests/interop.rs`, four tests over the 40-variant
matrix; `src/abi/`, `src/thunks.rs`, `--thunks` and `--signatures` on the
CLI). Three things worth recording:

- **Re-entrancy came out for free, and is the reason the corpus has a
  guest→foreign→guest case.** A typed wrapper starts the guest stack from
  `__stack_pointer`, and the thunk has already moved that below the calling
  guest's frames — so a foreign function that calls back into a transpiled
  export gets a fresh guest stack below its own frame rather than on top of
  the outer one. Nothing was written to make that work; it falls out of the
  sync being in the thunk rather than in the wrapper.
- **The sync is load-bearing and now proven so twice.** Removing it fails
  `guest_uses_array`, which passes a pointer to a guest-stack array into a
  foreign function deep enough to overwrite it. That case earns its place:
  the failure is invisible to every corpus function whose locals stay in
  registers.
- **The sweep found an unrelated gap, which was fixed rather than dodged.**
  `interop_foreign.c`'s `long long % 97` compiles to the one-operand
  widening `imul`, which had no translation. It now has one at every width
  in both signednesses (`translate_wide_multiply`, reusing the
  partial-product high half the `imul` overflow test already needed), with
  a hand-written corpus covering all twenty-four combinations, because
  compilers emit exactly one of them. Three mutations confirm it bites.

### 3. Callee-side inference: typed exports without declarations

The interprocedural liveness analysis, graded against ground truth before
it faces foreign callers.

- Read-before-written over the call graph: fixpoint, SCCs, unknown callees
  read everything; SysV prefix closure; width and pointer evidence as
  described above; return types from internal call sites, else the i64
  default.
- The inference report through `--dump`.
- Local symbols keep whatever contract inference finds — after
  interprocedural register allocation and constprop clones it is not SysV
  and does not need to be, because nothing foreign can name them. Only
  global symbols get typed faces.

**Accept:** for every *global* function in the existing corpus, at every
cell of the full matrix, the inferred argument list matches the C signature
the harness already knows — disagreement is a test failure, not a report
line. Milestone 2's incoming declarations are then deleted and its corpus
still passes.

**Outcome: passes** (`src/abi/effects.rs`, `src/abi/infer.rs`,
`tests/inference.rs`, `tests/corpus/signatures.{c,expected}`; `--infer` and
`--print-signatures` on the CLI). 340 graded signatures across the twenty
configurations, and the interop corpus now links and runs with every guest
signature inferred rather than declared.

**The facts layer, and why it is not an IR.** Register reads and writes come
from iced-x86's instruction information rather than from the translator,
because the analysis has to work on instructions the translator cannot
handle — which is the *main* case, since asking a stripped binary for its
signatures is most useful when it does not yet fully translate. iced also
turned out to have already made three distinctions the analysis needs: it
reports a 32-bit write as a write of the full register (x86-64
zero-extends), it recognises `xor eax,eax` as a pure write, and it
distinguishes the merging register form of `movss` from the zeroing memory
form of `movsd`.

**Five things the first version got wrong**, each found by grading against
the corpus rather than by reasoning, and each a case where the obvious
signal is the wrong one:

- `lea 0xc(%rsp),%rdi` names rdi as a 64-bit operand while *destroying* the
  argument in it. Width evidence has to come from reads only.
- `movslq %edi,%rdi` reads EDI and writes RDI, which are the same
  *location*. Filtering evidence per location let the write through and made
  every narrowed argument look 64 bits wide; it has to be per register
  access.
- `lea (%rdi,%rsi,1),%rax` is arithmetic, not addressing. Its width is the
  destination's, and treating its operands as addresses mistyped every
  64-bit add.
- Address use has to *beat* 64-bit use rather than lose to it: gcc walks an
  array with `lea (%rdi,%rsi,4),%rdx`, which is honest evidence that rdi is
  a 64-bit register and no evidence at all that the value is a 64-bit
  integer.
- A result register that is *written* is not thereby a result: clang aligns
  the stack with `push %rax`/`pop %rax`, and that `pop` lands after the
  floating-point work. What matters is the last *event* — and a `cmp` is not
  a consumption, because comparison leaves the register alone.

**Provenance, the one place value flow was needed.** Pointer parameters are
`i32` on wasm32, and the evidence for that — a dereference — routinely lands
on a register that is not the argument register: gcc at `-O1` copies the
pointer with `mov %rdi,%rax` first, and at `-O0` both compilers spill every
argument to the stack. This was named in advance as the point where the
analysis would start wanting an IR, and what it actually needed was one
fact — where a value came from — followed through full-width moves and stack
slots. Two deliberate limits keep it from becoming a general value analysis:
a sign- or zero-extending move is not followed at all (the extended value is
a different value, and following it made `int` parameters look like `i64`),
and arithmetic is followed only weakly, carrying enough to recognise a
pointer and not enough to claim a width.

**Three things inference refuses rather than guesses**, each recorded with
its reason where a reader will meet it:

- **A signature spanning both register files.** SysV fills the integer and
  floating-point registers independently, so `f(i32, f64)` and `f(f64, i32)`
  are byte-identical and the source order is simply not recorded.
  `guest_uses_mixed` is declared for this reason, which also keeps the
  override path tested.
- **An unused trailing parameter.** `f(int, int)` ignoring its second is
  byte-identical to `f(int)`.
- **A result whose width only the caller knows**, where a division leaves
  sixty-four bits in rax and only the low thirty-two are the `int` result.

**The sweep found another integer gap, and it was fixed rather than dodged.**
clang compiles the corpus's mutual recursion into `adc` against a flag a
comparison had already set. `adc`/`sbb` are now translated at every width,
with a hand-written corpus covering both, because the carry *out* is not the
ordinary rule: a sum landing exactly on its left operand has wrapped if
something was carried in and has not if nothing was. A mutation dropping
that term is caught on `adc_byte_flags(0, -1, carry=1)`.

### 4. Caller-side inference: thunks without declarations

- Written-before-call per site; union and cross-site agreement over the
  link set, disagreement an error naming the sites; prefix rule; result
  use from reads of `rax`/`xmm0` after the call.
- Varargs detection (`al` before the call) refuses with an error naming
  the symbol and the declaration escape hatch.
- The report covers inferred imports identically.

**Accept:** milestone 2's outgoing declarations are deleted and its corpus
still passes; a deliberately mis-inferred case (an argument register
written for unrelated reasons) demonstrably fails at link time rather than
running wrong; the cross-site disagreement case produces its error.

**Outcome: passes, by a different mechanism than this plan proposed**
(`src/wasm_reader.rs`, `src/abi/infer.rs`, `src/thunks.rs`;
`tests/corpus/call_sites.s`, three new tests in `tests/inference.rs` and one
in `tests/interop.rs`). The corpus now carries **one** declaration in
total — `guest_uses_mixed`, for the register-file ordering — and everything
else, in both directions, is recovered.

**What this plan got wrong.** Caller-side inference cannot recover an
argument that is passed straight through. `guest_uses_scale(int v) { return
foreign_scale(v) + 1; }` compiles at `-O2` to `call foreign_scale; add
$1,%eax; ret`, which does not mention rdi anywhere. The callee-side analysis
cannot see the argument because the caller never touches the register; the
caller-side analysis cannot see it for the same reason. That is not a
weakness of either analysis — the information was never written down, so no
amount of analysis recovers it.

**Where the information actually is.** In the wasm object being linked
against. A wasm object states an explicit type for every function it
defines, and interop means having that object in hand — it *is* what is
being linked against. So `--thunks` now takes the wasm objects alongside the
native ones and reads their types directly. That is knowledge rather than
inference, and it makes inference what it should have been at a boundary all
along: the fallback for a foreign function no object defines — a host
import — and the source of the loud refusals.

The order of authority is therefore **declaration, then wasm object, then
call-site inference**. A declaration stays first because it is the
documented override: the way to describe something no object records.

**What caller-side inference does deliver**, measured rather than assumed:

- **Results, reliably.** "Is a result register read after the call, and as
  what" is a clean signal at every configuration tested.
- **Arity, when the caller sets its arguments up explicitly** — the ordinary
  case for anything that is not a pass-through.
- **Over-reporting when the caller uses an argument register as scratch**,
  which `-O0` does constantly: `foreign_store` comes out with three
  arguments because rdx held a temporary. This is inherent — a register
  holding a leftover value is indistinguishable from one holding an
  argument — and it is why inference is the last source rather than the
  first.

**Two bugs the corpus found**, both evidence attributed to the wrong thing:

- The derived-provenance branch was inheriting *computed* values as well as
  arguments. `sub $0x18,%rsp` makes rsp a computed 64-bit value, and
  `lea 0xc(%rsp),%rdi` then carried that width onto rdi — shadowing the rule
  that a stack address is 32 bits. Derived provenance now carries only
  arguments, which is all it was ever for.
- A caller that vectorised a loop before a call left packed values in the SSE
  registers, which made the call appear to pass floats — and since a
  signature spanning both register files is refused, the call site was lost
  entirely. Only a *scalar* floating-point operation now marks a register as
  holding a possible argument.

**The refusals are what inference is really for at a boundary**, and each has
a test: call sites that disagree are refused by name (`disputed`, from
`passes_one` and `passes_two`, so the declaration that resolves it can be
written knowing what it resolves); a call that sets the vector-count register
is refused as variadic; and a deliberately mis-inferred `foreign_scale()`
stops the link with the symbol named, rather than producing a program that
runs and returns the wrong answer.

### 5. The standing cross-convention corpus

The end state as a permanent test, inference-only.

- Both call directions, scalars and pointers and both float widths and
  mixed argument lists, a pointer created on one side and dereferenced on
  the other, a foreign callee deep enough in shadow-stack use to punish a
  broken sync — all with zero declarations except one case kept declared
  on purpose (a return-type override), so the exception path stays tested.
- Mutation discipline, as with the float plan: break the `__stack_pointer`
  sync, flip an extension, drop a trailing argument, mis-order two
  arguments — each mutation must be caught by a test, or the missing test
  gets written.

**Accept:** the corpus passes differentially across the full matrix with
inference supplying every boundary signature, and every listed mutation is
caught.

**Outcome: passes.** The corpus now covers every direction the plan named,
with one declaration in the whole link set. Both call directions; scalars,
both float widths and a mixed argument list; a pointer created by the guest
and dereferenced by foreign code (`guest_uses_pointer`, `guest_uses_array`)
*and* one created by foreign code and dereferenced by the guest
(`guest_uses_foreign_pointer`); a pointer *parameter* on a transpiled export
written through from the far side (`guest_fill`); guest→foreign→guest
(`guest_round_trip`) and foreign→guest with a live foreign frame across the
call (`foreign_uses_guest_fill`); and foreign callees deep enough in shadow
stack to destroy a guest frame if the sync were wrong.

**The mutations, and what each one taught.**

| Mutation | Caught by |
|---|---|
| `__stack_pointer` sync removed | `guest_uses_array` — a pointer to a guest-stack array, handed to a foreign callee whose frame would land on it |
| Return-address slot never popped | the corpus generally: the guest stack drifts by 8 bytes per call |
| Stack pointer set but never restored | `repeated_crossings_do_not_walk_the_stack_down` — **written because this mutation was not caught** |
| Float converted rather than reinterpreted | `guest_uses_narrow`, on the bits |
| First two integer arguments swapped | `guest_fill`, `foreign_store` |
| First two float arguments swapped | `guest_uses_blend` |
| Typed wrapper drops its last argument | `guest_uses_blend`, `guest_fill` |
| i32 argument sign-extended instead of zero-extended | **nothing, and correctly so** — see below |

Two of these are worth more than a row.

**The stack-restore mutation is the one this milestone earned.** A thunk that
hands `__stack_pointer` to a foreign callee and never puts it back returns
the right answer — the first time. The damage is cumulative: the next host
entry starts its guest stack from wherever the pointer was left, so every
crossing begins lower than the one before, and the descent eventually reaches
the data segments silently, because nothing in wasm objects to a store below
the stack. Every existing test made too few calls to see it. The new test
makes three thousand and checks the answer never moves, which is the
difference between testing that the sync *happens* and testing that it is
*undone*.

**The extension mutation is not a missing test.** Sign- versus zero-extending
a 32-bit argument differs only in the upper half of the register, and nothing
that consumes one ever looks there: `load_argument` wraps back to `i32`, and
a conforming callee reads `edi`. SysV leaves those bits unspecified precisely
so that no one may depend on them. The one case where the choice does matter
is a pointer — `AbiType::I32` covers both — and only for an address at or
above 2 GiB, which a wasm32 module reaching that size could produce but this
corpus cannot. Zero-extension remains correct and deliberate; it is simply
not observable through any conforming path, and a test that could see it
would have to construct a state no conforming program reaches.

## Out of scope, stated so the boundary is visible

- **Varargs across the boundary** — detected and refused loudly.
- **By-value structs, and struct layout generally** — x86-64 is LP64,
  wasm32 C is ILP32: pointers and `long` are 8 bytes on one side and 4 on
  the other, so even structs passed *by pointer* only interop when their
  layout is identical under both models (fixed-width fields, no pointers).
  This is the data-layout wall design.md already records; no thunk fixes
  it.
- **Function pointers across the boundary** — a guest function pointer is
  a table index of a `() -> ()` function; foreign code would
  `call_indirect` with a real type and trap. One address cannot carry two
  conventions without breaking pointer identity; this needs its own
  design.
- **Stack arguments** (beyond 6 integer / 8 float registers) — not
  marshalled; the arity mismatch is caught at link time like any other.
- **Internal lifting** — the performance project; reuses this analysis,
  starts with a benchmark.

## Risks and their scheduled kill-points

| Risk | Mitigation | Flushed out in |
|---|---|---|
| `wasm-ld` does not check signatures, or only warns | Dedicated spike before anything else; `--fatal-warnings` in the recipe if a warning; fallback re-scopes inference to cross-check with mandatory declarations | Milestone 1 |
| Missing `__stack_pointer` sync corrupts guest frames silently | Spike callee written to punish it; sync-breaking mutation in milestone 5 | Milestones 1, 5 |
| Pointer arguments systematically infer as i64 against wasm32's i32 | Pointer evidence (address use, relocations, `rsp` derivation); report shows guessed slots; declaration override | Milestones 3, 5 |
| Export return types are uninferable | Internal-call-site evidence; i64 default whose mismatch is a link error; the one declared case in milestone 5 keeps the override path tested | Milestones 3, 5 |
| Unused trailing arguments are invisible to callee-side inference | Prefix closure covers interior gaps; trailing loss surfaces as a link mismatch | Milestone 3 |
| Call sites disagree about a foreign signature | Error naming the sites, never a vote | Milestone 4 |
| Varargs marshalled wrong silently | `al`-protocol detection refuses loudly | Milestone 4 |
| IPRA/constprop locals have non-SysV contracts | Only global symbols get typed faces; locals keep their observed contract | Milestone 3 |
| Thunk object linked alongside a transpiled definition of the same symbol | Duplicate-symbol diagnosis at link; `--thunks` sees the link set and emits nothing for symbols the set defines | Milestone 2 |
| A corpus source compiled under LP64 and ILP32 means two different things | Fixed-width types in mixed-corpus sources; the rule recorded with the existing corpus-shape rule | Milestone 2 |

## Testing discipline

Unchanged from the MVP and float plans, restated because every rule
applies:

- The differential harness is the backbone; the mixed-convention programs
  extend it with a whole-program native oracle rather than inventing a new
  harness. Bit-exact comparison, NaN-class rule excepted.
- Inference is graded against the harness's existing signature knowledge —
  ground truth that already spans two compilers and five optimisation
  levels — before any foreign module consumes its output.
- Fast checks first: snapshots and `wasm-ld` acceptance on every change,
  differential execution per corpus, the transpile sweep at milestone
  boundaries. Nothing in the inner loop takes minutes.
- Failures are attempt one: a red differential test gets diagnosed and
  fixed, not skipped or narrowed. A corpus source must not be shaped to
  avoid an instruction or a signature shape; exceptions are recorded in
  design.md where the existing one is.
