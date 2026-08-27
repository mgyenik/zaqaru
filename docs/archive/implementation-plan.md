# zaqaru — implementation plan

Status: **complete** — every milestone's acceptance criterion passes; archived
on 2026-08-25. Nothing replaced it: the remaining work is the "Future work"
section of [design.md](../design.md), and the decisions taken while building
are recorded in that document's decision log.

Companion to [design.md](../design.md); the design doc holds the *why*, this
holds the *what and in what order*.

## Outcome

All six milestones landed, in order, each verified against its own criterion.

| Milestone | Acceptance | Outcome |
|---|---|---|
| 1. Read and dump | Two-function `.c` dumps every operand symbolically with correct addends | Passes (`tests/lifting.rs`). A non-zero-addend case was pulled forward from milestone 5 to pin the program-counter-relative arithmetic down at the point it is written |
| 2. Straight-line end to end | `add(2,3) == 5` through `wasm-ld` and wasmtime, plus 1000 random pairs against the native `.so` | Passes (`tests/differential.rs`). The weak-global de-risk experiment succeeded first time, so the companion machine-state object was never needed (`tests/emitter_linking.rs`) |
| 3. Branches and loops | Corpus passes differentially through the dispatcher | Passes. The corpus was extended past the planned five functions with a width-and-flags stress case, because the flag rules are where wrong answers are silent |
| 4. Calls | Cross-object demo green; recursive `fib` matches native | Passes, including spill-heavy frames. Calls the assembler had already resolved into bare offsets — no relocation to read — turned out to need target resolution by section offset as well as by symbol |
| 5. Data | Data corpus passes differentially, with a non-zero-addend case | Passes, covering `.data`, `.rodata`, `.bss`, a relocated pointer held in data, and a global defined in one object and read from another |
| 6. Structured control flow | Whole corpus green in both modes; a hand-written irreducible object falls back | Passes. Every corpus function is transpiled twice and both modules are checked against the same native oracle; `tests/control_flow_shape.rs` separately proves the structured mode is actually being used rather than quietly falling back |

Two things also landed that the plan folded elsewhere: the `cdq`/`cqo` and
`div`/`idiv` group from the design's instruction scope, and the `.wat`
snapshot tier, which was written once the instruction set had stopped moving
so that its expectations did not need rewriting six times.

The repository layout gained two files the plan did not name: `src/machine.rs`
(the emulated machine state and its wasm spelling) and `src/transpile.rs`
(deciding what symbols an output object has), splitting apart what the plan
had folded into `translate.rs` and `emitter/`.

## Prerequisites and tooling

- Rust stable; single binary crate to start (split into a workspace only
  when something earns it).
- Crates: `object` (ELF reading), `iced-x86` (decoding), `wasm-encoder`
  (emission), `wasmprinter` (debug/snapshot rendering); dev-dependencies:
  `wasmtime` (execution tests), `libloading` (native side of differential
  tests).
- Host tools, checked at test time and skipped-with-notice if absent:
  `gcc` or `clang` (corpus compilation), `wasm-ld` (from lld),
  `clang --target=wasm32` (reference specimens), optionally wabt's
  `wasm-objdump` (linking-section dumps).

## Repository layout

```
zaqaru/
  Cargo.toml
  src/
    main.rs          CLI: zaqaru input.o -o output.wasm.o (+ dump flags)
    reader.rs        ELF sections, symbols, relocations → internal model
    lifter.rs        decode, basic blocks, symbolic operand resolution
    cfg.rs           CFG types, dominators (hand-rolled, ~50 lines)
    structurer.rs    dispatcher mode; later dominator-based mode
    translate.rs     x86 instruction → wasm ops (machine-model semantics)
    emitter/
      mod.rs         module assembly
      code.rs        function bodies, padded-LEB relocation sites
      data.rs        data segments and segment info
      linking.rs     `linking` + `reloc.*` custom sections
  tests/
    corpus/          C sources for differential tests
    specimens/       trivial C compiled with clang --target=wasm32 (checked in
                     as source; objects built at test time)
    differential.rs  native .so vs wasm-ld+wasmtime, fixed + random inputs
    snapshots/       .wat snapshot expectations
  docs/
    design.md
    implementation-plan.md
    archive/         superseded docs move here
```

Conventions from day 1: descriptive names everywhere (no coined
abbreviations); unimplemented instructions are hard errors naming the
instruction; every emitted object must pass `wasm-ld` cleanly.

## Milestones

Each milestone has an acceptance criterion; a milestone is done when its
criterion passes, not before.

### 1. Read and dump

Parse the object file and print what the rest of the pipeline will consume.

- `zaqaru --dump add.o`: functions with disassembly, operands shown
  symbolically (`call add_guest`, `[rip + counter+4]`), relocation list,
  section/symbol summary.
- Lifter invariant established here: disassemble at section offset 0,
  match ELF relocation offsets to instruction displacement/immediate byte
  ranges via iced's constant-offsets API.

**Accept:** dump of a two-function `.c` (one calling the other, one
`.rodata` reference) shows every operand symbolically with correct addends.

### 2. Straight-line end to end

The `int add(int a, int b)` case through the whole pipe. Deliberately tiny
in scope, deliberately complete in plumbing — this milestone forces the
entire emitter skeleton and flushes the three biggest risks (below).

- Machine model: register/flag globals (weak symbols), sub-register write
  semantics, `emit_address` choke point (unused yet, but the seam exists).
- Instructions: `mov`, `add`, `sub`, `lea`, `movsx`/`movzx`, `ret`,
  `endbr64`-as-nop — whatever the `-O1` output of the corpus needs.
- Host-entry wrappers: uniform `(i64 ×6) → i64`, `x86_rsp` initialized
  from imported `__stack_pointer` with the align-down-16-minus-8
  treatment; wrapper owns the clean symbol name, guest function is
  `name_guest`, hidden.
- Emitter: types, functions, code with padded-LEB call sites, `linking`
  section (symbol table), `reloc.CODE`.
- Reference specimen: check in a trivial wasm32 C file; a test diffs the
  *structure* of our linking/reloc sections against clang's.
- **De-risk experiment (do first, throwaway code allowed):** hand-emit two
  minimal objects both weakly defining a global, link with `wasm-ld`,
  confirm dedup. If it fails, switch the design to the companion
  machine-state object before building the real emitter.

**Accept:** `gcc -c add.c` → transpile → `wasm-ld --no-entry --export=add`
→ wasmtime: `add(2,3) == 5`, and the differential harness agrees with the
native `.so` on 1000 random input pairs.

### 3. Branches and loops (dispatcher)

- Basic-block CFG; dispatcher translation (`loop` + `br_table` over a
  block-index local).
- Eager flag computation; `cmp`, `test`, `jcc`, `jmp`, `setcc`, `cmov`,
  shifts, `neg`, `not`, `and`, `or`, `xor`, `imul`.
- Corpus: iterative `gcd`, iterative `fib`, `abs`, three-way compare
  (exercises flags live across block boundaries), `clamp` (exercises
  `cmov`).

**Accept:** corpus passes differential testing, dispatcher mode.

### 4. Calls

- `call`/`ret`/tail-`jmp` translation with RSP bookkeeping and sentinel
  return-address slots; `push`/`pop`; function-index relocations.
- Stack memory in use for the first time — spill-heavy functions join the
  corpus.
- **Payoff demo:** two C files transpiled *separately*, linked together by
  `wasm-ld`, calling each other; differential test passes.

**Accept:** cross-object call demo green; recursive `fib` (needs real
stack) matches native.

### 5. Data

- `.data`/`.rodata` segments + segment info, `.bss` zero-fill; data
  symbols in the symbol table; `reloc.DATA`.
- RIP-relative loads/stores → `i32.const` + `R_WASM_MEMORY_ADDR_SLEB` with
  the PC32 addend adjustment (design doc: "off-by-four factory").
- Corpus: lookup in a `static const` table, read-modify-write of a global
  counter, string-length over a `.rodata` literal.

**Accept:** data corpus passes differentially; a deliberate
`symbol + non-zero-addend` case is covered.

### 6. Structured control flow

- Dominator tree, back-edge/loop detection, the dominator-based structured
  translation; irreducibility detection falling back to the dispatcher.
- Both modes run on the whole corpus and must agree (dispatcher as
  oracle), then differentially against native.

**Accept:** entire corpus green in both modes; at least one hand-written
irreducible-CFG object (assembled from `.s`) correctly falls back.

### After the MVP (ordered candidates, not commitments)

DWARF-typed host wrappers (`gimli`), flag-liveness optimization, division
edge-case fidelity, SSE/floats, jump tables → `br_table`, indirect calls
via table indices + `call_indirect`, selective ABI lifting with
register-to-local promotion, memory64 flag.

## Risks and their scheduled kill-points

| Risk | Mitigation | Flushed out in |
|---|---|---|
| wasm-ld rejects our hand-built linking metadata | Imitate and structurally diff against clang-produced specimens | Milestone 2 |
| Weak-global dedup not supported in practice | Dedicated two-object experiment before emitter work; fallback = companion machine-state object | Milestone 2 (first task) |
| Padded-LEB emission awkward in wasm-encoder | Raw-byte path (`Function::raw`); worst case, own the code-section bytes entirely | Milestone 2 |
| PC32 addend translation off-by-four | Dedicated non-zero-addend differential test | Milestone 5 |
| Flags-across-blocks correctness | Eager flags (no liveness shortcuts) + three-way-compare corpus case | Milestone 3 |
| Structurer bugs | Dispatcher oracle, dual-mode runs on everything | Milestone 6 |

## Testing discipline

- The differential harness lands in milestone 2 and every later milestone
  extends the corpus rather than inventing new harnesses.
- Fast checks first: `.wat` snapshots and `wasm-ld` acceptance are
  cheap and run on every change; differential execution is the per-corpus
  integration tier. Nothing in the inner loop takes minutes.
- Failures are attempt one: a red differential test gets diagnosed and
  fixed, not skipped or narrowed.
