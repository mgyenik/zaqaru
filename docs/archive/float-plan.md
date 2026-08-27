# zaqaru — floating point and SSE: implementation plan

Status: **complete** — every milestone's acceptance criterion passes; archived
on 2026-08-26. Nothing replaced it: what the work settled is recorded in
[design.md](../design.md)'s "Vectors and floating point" section and its
decision log.
Date: 2026-08-26

Companion to [design.md](../design.md), whose "Vectors and floating point"
section holds the *why*; this holds the *what and in what order*. The shape
mirrors the archived [MVP plan](implementation-plan.md): each milestone has an
acceptance criterion and is done when the criterion passes, not before.

## Outcome

All five milestones landed, in order, each verified against its own criterion.

| Milestone | Acceptance | Outcome |
|---|---|---|
| 1. XMM state and the move family | The restored corpus passes differentially across the full matrix; no corpus source retains a shape chosen to dodge SSE movers | Passes. The interpreter's swap is back in its natural adjacent form, which turned out to need `pshufd` as well as `movq` — so the lane-rearranging family was pulled forward from milestone 5 and given one shared translation driven by a table of lane sources. "Both compilers" also meant adding clang to the differential matrix, which had been gcc-only |
| 2. The parity flag | The bail is gone; the hand-written case passes; every existing corpus still passes | Passes. `imul` was cut from the parity corpus after the first run: it leaves the flag architecturally undefined and the hardware preserves the previous value, so comparing it would have been testing the processor rather than the translation |
| 3. Scalar arithmetic, compares, conversions | The scalar float corpus passes across the full matrix with fixed edge inputs, compared as bits with the NaN-class rule the only exception | Passes. The three places the naive mapping is wrong were each confirmed by deliberately breaking them and watching the corpus catch it. Floating-point literal pools arrive as untyped local labels with no size, which the transpiler had been skipping — defined symbols in a data section are now addressable whatever the ELF type field says |
| 4. Floats across the host boundary | A corpus of natural-signature functions passes across the full matrix | Passes. The multiple-result spike succeeded first time, so the accessor-export fallback was never needed; it stays in `tests/emitter_linking.rs` beside the weak-global experiment. clang's branchless compare made `cmpsd` a milestone-4 requirement rather than a milestone-5 one |
| 5. Bit idioms and packed operations | The vectorised corpus passes differentially at every optimisation level, and the whole corpus passes a `-O0`..`-O3`/`-Os` transpile sweep | Passes. The demand-driven approach worked as intended but left a gap it could not see: thirty-one translations were implemented as complete families while the corpus only reached some members, and a deliberate mutation proved the unreached ones untested. `tests/corpus/vector_lanes.s` covers every one on purpose, and finding it immediately caught a real bug — `i64x2.gt_s` had been emitted with the opcode of `i64x2.lt_s` |

Two things also landed that the plan did not name: the optimisation sweep
became a test of its own (`tests/optimisation_sweep.rs`) rather than a manual
check, and the differential matrix grew from twelve variants to forty — two
compilers, two code models, five optimisation levels, two control-flow
translations — which the whole suite still runs in about twelve seconds.

The repository layout gained one file: `src/translate/vector.rs`, holding the
XMM register file and everything that moves through it, which the plan had
folded into `translate.rs`.

## Why this is not optional

SSE registers are not a floating-point feature on x86-64 — they are where
`float` and `double` arguments travel, where `-O2` fuses adjacent integer
moves, and how every nontrivial struct copy is spelled. A transpiler that
takes binaries as they come cannot refuse them. Today every SSE instruction
is a loud per-instruction error, and the parity flag — which floating-point
compares report *unordered* through — is deliberately unmodelled.

## Groundwork already landed

Two de-risk experiments were run before this plan was written, in the style
of the MVP's weak-global experiment, and their results fix the design:

1. **`v128` globals do not link.** `wasm-ld` rejects any object defining one
   (`invalid opcode in init_expr: 253`): LLD's object reader has no case for
   a `v128.const` initializer, including on LLVM `main`. XMM state therefore
   cannot be `v128` globals.
2. **`i64`-pair state with SIMD bodies works end to end.** Two objects each
   weakly defining an `i64` pair, one writing it and the other reading it
   back through `i64x2.splat`/`replace_lane`/`add`/`extract_lane` over a
   `v128` local, link with stock `wasm-ld` (the weak pairs collapse) and run
   bit-exactly under wasmtime. LLD copies code bodies opaquely, so SIMD
   *instructions* are unrestricted even though SIMD *globals* are.

The supporting emitter work is already in and tested: `ValueType::V128`, the
`v128.const` initializer encoding, the `i64x2` builder instructions, a
`v128` temporaries pool, `simd128` in `target_features` whenever a body uses
SIMD, and the `Carrier` enum that keeps the integer translation paths
exhaustive instead of growing unreachable arms.

## The representation, fixed by the experiments

Each XMM register is a **pair of weak `i64` globals** (`x86_xmm0_lo`,
`x86_xmm0_hi`, … `x86_xmm15_hi`), deduped across objects exactly like the
integer register file. The pair turns out to fit SSE's own grain: scalar
operations write the low 64 bits and preserve the high 64, which in pair
state is "touch only the `lo` global". Packed operations assemble a `v128`
local from the pair, use wasm SIMD, and split the result back.

MXCSR is not modelled: wasm arithmetic is IEEE-754 round-to-nearest-even,
which is SSE's default mode, and `ldmxcsr`/`stmxcsr` stay loud errors. x87
(`long double` only, on x86-64) stays out of scope with loud errors.

## Milestones

### 1. XMM state and the move family

The payoff of this milestone is *integer* code: `-O2` uses `movq xmm` to
fuse adjacent word moves and `movdqa`/`movups` for struct copies, in code
with no floating point in it — the gap design.md carried as "the largest
remaining obstacle to taking binaries as they come".

- Machine model: the 16 pairs, defined weakly by `MachineState` alongside
  the integer registers.
- Moves: `movq`/`movd` (xmm↔gp, xmm↔mem), `movsd`/`movss` (all forms),
  `movaps`/`movups`/`movdqa`/`movdqu` (two `i64` loads/stores — no SIMD
  needed for pure copies), `movlpd`/`movhpd`, and the zeroing idioms
  `pxor`/`xorps`/`xorpd` of a register with itself.
- **The merge/zero asymmetry, spelled out because it is silently wrong
  otherwise:** `movsd xmm, m64` zeroes bits 64–127 while
  `movsd xmm1, xmm2` merges the low 64 and preserves the rest; `movss`
  from memory zeroes bits 32–127 while the register form merges only bits
  0–31. Each form's exact write mask is part of this milestone, with corpus
  cases whose results differ if a mask is wrong.
- Corpus: struct copy and swap functions sized to trigger SSE movers at
  `-O2`, including the non-adjacent-slot workaround case from the MVP
  interpreter corpus **restored to its natural adjacent form** — the form
  that was cut because `movq xmm0` stopped it.

**Accept:** the restored corpus passes differentially across the full
matrix (both compilers × both code models × `-O0`..`-O2` × both
control-flow modes), and no corpus source retains a shape chosen to dodge
SSE movers.

### 2. The parity flag

Floating-point compares report *unordered* through PF, and compilers branch
on `jp`/`jnp` immediately after `ucomisd` — so the flag must exist before
compares can. This is a machine-model change, not an instruction.

- `Flag::Parity` (`x86_pf`) joins the four existing eager flag globals.
- Every integer flag rule computes it: parity of the result's low byte,
  `popcnt(result & 0xff) & 1 ^ 1` — one `i32.popcnt`, in keeping with
  "eager flags, no liveness shortcuts" (the flag-liveness pass remains the
  first *optimization*, unchanged).
- `emit_condition` gains `p`/`np`, deleting the bail; `jp`/`jnp`,
  `setp`/`setnp`, `cmovp`/`cmovnp` follow from the existing machinery.
- Corpus: a hand-written `.s` case consuming PF after integer arithmetic
  (compilers rarely emit that; the `.s` precedent is the irreducible-CFG
  case), plus coverage through every milestone-3 compare test thereafter.

**Accept:** the parity bail is gone; the `.s` case passes differentially;
all existing corpora still pass (the new computation must not disturb the
other four flags).

### 3. Scalar arithmetic, compares, conversions

The core set `gcc -O1` emits for `float`/`double` code, translated by
reinterpreting the `lo` global (`f64.reinterpret_i64`; for `float`, the low
32 bits with the upper 32 of `lo` preserved, per the milestone-1 masks).
Both widths land together — the mappings are parallel.

- Arithmetic: `addsd`/`subsd`/`mulsd`/`divsd`/`sqrtsd` and the `ss` forms →
  the corresponding `f64`/`f32` wasm ops. Wasm and SSE are both IEEE-754
  round-to-nearest-even, so these are bit-exact — no tolerance anywhere.
- Three places the naive mapping is **wrong** and gets explicit emulation:
  - `minsd`/`maxsd`/`minss`/`maxss` return the *second* operand on ties
    and NaN; wasm `f64.min` propagates NaN and orders ±0. Compare+select,
    never the wasm min/max ops.
  - `cvttsd2si`/`cvttss2si` (both destination widths) on NaN, ±∞ or
    overflow produce the integer indefinite (`0x80000000` /
    `0x8000000000000000`); wasm `trunc` traps and `trunc_sat` saturates
    differently. Bounds-check+select.
  - `ucomisd`/`comisd` (and `ss`) set `ZF,PF,CF` = (1,1,1) unordered,
    (0,0,1) less, (1,0,0) equal, (0,0,0) greater, and zero SF/OF — after
    which the existing `ja`/`jb`/`je` emission works unchanged and `jp`
    works by milestone 2.
- Conversions: `cvtsi2sd`/`cvtsi2ss` (32- and 64-bit sources; these merge
  into the low lane like scalar arithmetic), `cvtsd2ss`, `cvtss2sd`.
- Corpus signatures stay integer at the boundary in this milestone —
  values cross as bits in `uint64_t` and `memcpy` into `double` inside —
  because the host wrappers cannot carry floats until milestone 4, and
  because bits-at-the-boundary makes the differential comparison exact by
  construction. Edge inputs are fixed, not only random: NaN (both signs),
  ±∞, ±0, subnormals, values astride every conversion overflow boundary.
- Harness NaN policy: where the guest *generates* a NaN, x86 produces the
  negative quiet NaN `0xFFF8…` while wasm engines canonicalize
  nondeterministically — so the harness compares NaNs as a class and
  everything else as raw bits. Documented in the harness where the
  comparison happens.

**Accept:** the scalar float corpus passes differentially across the full
matrix with the fixed edge inputs included; every arithmetic result is
compared as bits, with the NaN-class rule the only exception.

### 4. Floats across the host boundary

The uniform host-entry wrapper is `(i64 ×6) → i64`; SysV passes floats in
`xmm0–7` and returns them in `xmm0`, so no float function is callable from
the host regardless of how well its body translates.

- **De-risk first (same discipline as the `v128` experiments):** confirm
  stock `wasm-ld` accepts relocatable objects whose function types have
  multiple results, and wasmtime calls them. Expected to pass — LLD reads
  result vectors generically — but the design leans on it, so it gets its
  own two-object spike before wrapper code is written.
- The uniform wrapper grows to `(i64 ×6, f64 ×8) → (i64, f64)`: fill
  `rdi…r9` *and* the `xmm0–7` low halves, call the guest, return
  `(rax, xmm0.lo reinterpreted)`. Still zero per-function type knowledge —
  callers ignore the halves they don't want, exactly as integer callers
  ignore unused argument registers today.
- Fallback if the spike fails: keep the `i64` result and read float
  returns through a small exported accessor that returns `x86_xmm0_lo`
  reinterpreted. Uglier, equally correct.
- The differential harness classifies each corpus signature's arguments
  into integer and float slots (it knows the C signature it is testing —
  this is the harness's SysV knowledge, not the transpiler's) and calls
  natural-signature float functions directly.
- DWARF-typed wrappers remain future work; this milestone extends the
  zero-information shim, it does not add type inference.

**Accept:** a corpus of natural-signature functions — `double` parameters
and returns, `float` likewise, and mixed integer/float argument lists —
passes differentially across the full matrix.

### 5. Bit idioms and packed operations

The long tail, driven by what the corpus and real binaries demand rather
than enumerated up front — the loud per-instruction error keeps this
honest, exactly as it has since milestone 1 of the MVP.

- Bit idioms: `fabs`/negation/`copysign` compile to `andpd`/`xorpd`/
  `orpd`/`andnpd` against `.rodata` masks — pair state makes these two
  `i64` ops; the mask constants arrive through the existing data path.
- Compare-mask forms: `cmpsd`/`cmpss` predicates producing all-ones/zero
  masks, `movmskpd`/`movmskps` for branchless selection.
- Packed arithmetic (auto-vectorized `-O2`/`-O3` loops): assemble the pair
  into a `v128` local, use wasm SIMD (`f64x2.add`, `paddd`→`i32x4.add`,
  …), split back — the proven spike path. Lane-shuffling
  (`unpcklpd`/`shufpd`/`pshufd`) as encountered.
- Corpus: a summation loop and a scaling loop shaped so gcc actually
  vectorizes them, fabs/copysign/branchless-select idioms at `-O1` and
  `-O2`, plus whatever the `-O3` sweep of the *existing* corpus starts
  demanding once milestones 1–4 are in.

**Accept:** the vectorized corpus passes differentially at every
optimisation level, and the full corpus — MVP and float — passes a
`-O0`..`-O3`/`-Os` transpile sweep with zero failures, restoring the
320-configuration bar the MVP set, now without any SSE carve-outs.

## Risks and their scheduled kill-points

| Risk | Mitigation | Flushed out in |
|---|---|---|
| `wasm-ld` rejects `v128` globals | **Already flushed**: it does, on LLVM main included; design switched to `i64` pairs before implementation | Pre-plan spike |
| SIMD bodies rejected somewhere in the pipeline | **Already flushed**: spike links and runs them | Pre-plan spike |
| `movss`/`movsd` merge-vs-zero masks wrong | Explicit per-form masks; corpus cases whose results differ if a mask is wrong | Milestone 1 |
| Parity computation disturbs existing flags | Full existing-corpus rerun is part of the acceptance criterion | Milestone 2 |
| NaN bit patterns diverge (wasm canonicalization) | NaN-as-class comparison in the harness; all other values as raw bits | Milestone 3 |
| `cvtt` edge semantics (integer indefinite vs trap/saturate) | Explicit bounds-check emulation; fixed edge inputs in the corpus | Milestone 3 |
| min/max tie/NaN operand selection | Compare+select emulation; never wasm `f64.min`/`f64.max` | Milestone 3 |
| Multi-value results rejected by `wasm-ld` or wasmtime | Dedicated spike before wrapper work; fallback = accessor export | Milestone 4 (first task) |
| Misremembered SIMD opcode encodings | wasmtime validation in every differential run; `.wat` snapshots make the encoding readable | Every milestone |
| Packed long tail is unbounded | Demand-driven with loud per-instruction errors; the `-O3` sweep bounds "done" by what compilers actually emit | Milestone 5 |

## Testing discipline

Unchanged from the MVP, restated because every rule applies directly:

- The differential harness is the backbone; each milestone extends the
  corpus rather than inventing new harnesses. Bit-exact comparison,
  NaN-class rule excepted.
- Fast checks first: snapshots and `wasm-ld` acceptance on every change;
  differential execution as the per-corpus integration tier; the
  `-O0`..`-O3`/`-Os` transpile sweep at milestone boundaries. Nothing in
  the inner loop takes minutes.
- Failures are attempt one: a red differential test gets diagnosed and
  fixed, not skipped or narrowed. A corpus case reshaped to avoid an
  instruction (as the interpreter swap once was) counts as a narrowed
  test — milestone 1 exists to remove the one instance of that.
