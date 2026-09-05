# Finishing the x87: from crate to translated instructions

Status: **active — the crate is built and green; everything below is the
remaining work, in order.**

This plan finishes what [container-build-plan.md](container-build-plan.md)'s
x87 appendix started. The `x87/` crate exists: the extended-precision
softfloat, the register stack, the env images, the FFI symbols, 47 unit
tests and a 14-test host-FPU oracle (~1.3M bit-exact comparisons in
0.31s). What does *not* exist yet is everything that connects it to a
translated instruction. This document specifies that work precisely
enough to be executed without archaeology: every file, function and
symbol it names exists in the tree today unless it is explicitly marked
as new.

Read first, in this order: `x87/src/lib.rs` (the tier table),
`x87/src/ffi.rs` (the helper symbols — the contract the translator
compiles against), `src/translate/vector.rs`'s module header (the shape
`src/translate/x87.rs` mirrors), and the "x87 and MMX" section of
[container-plan.md](container-plan.md) (the design authority; where this
plan and that section disagree, that section wins and this plan gets
corrected).

The milestones are X1–X7. X1–X5 finish the near-term goal — the x87
rows leave the M6 grind's refusal tail and a long-double binary runs
differentially clean. X6 and X7 are the integration points and the
full-coverage rows, specified now so nothing gets rediscovered.

---

## X1 — Symbol plumbing: the translator can name the helpers

The pattern to copy is `SYSCALL_ENTRY`, end to end. Read
`src/transpile.rs:842` (`declare_imported_functions`) and
`src/translate.rs:39` (`trait SymbolResolver`) before writing anything.

**1. The helper enum** (new, in `src/translate/x87.rs` — created in X2,
but the enum can land first): one variant per FFI symbol, with two
methods:

```rust
pub enum X87Helper { Fld32, Fld64, Fld80, FldSti, FldConst, Fild16, ... }

impl X87Helper {
    pub fn symbol_name(self) -> &'static str;          // "x87_fld32", ...
    pub fn signature(self) -> FunctionType;            // params/results
}
```

The complete symbol/signature table (this is `x87/src/ffi.rs`, restated;
if the two ever disagree, the link fails loudly — that is the point of
typed imports):

| Symbol | Params | Result |
| --- | --- | --- |
| `x87_fld32` | i32 | — |
| `x87_fld64` | i64 | — |
| `x87_fld80` | i32 (address) | — |
| `x87_fld_sti` | i32 | — |
| `x87_fld_const` | i32 (0..6, opcode order) | — |
| `x87_fild16` / `x87_fild32` | i32 | — |
| `x87_fild64` | i64 | — |
| `x87_fst32` | i32 (pop) | i32 |
| `x87_fst64` | i32 (pop) | i64 |
| `x87_fstp80` | i32 (address) | — |
| `x87_fst_sti` | i32, i32 (pop) | — |
| `x87_fist16` / `x87_fist32` | i32 (pop) | i32 |
| `x87_fist64` | i32 (pop) | i64 |
| `x87_fisttp16` / `x87_fisttp32` | — | i32 |
| `x87_fisttp64` | — | i64 |
| `x87_arith_sti` | i32 (op), i32 (dst), i32 (src), i32 (pop) | — |
| `x87_arith32` | i32 (op), i32 (bits) | — |
| `x87_arith64` | i32 (op), i64 (bits) | — |
| `x87_arith_i16` / `x87_arith_i32` | i32 (op), i32 | — |
| `x87_fchs` `x87_fabs` `x87_fsqrt` `x87_frndint` `x87_fprem` `x87_fprem1` `x87_fscale` `x87_fxtract` `x87_f2xm1` `x87_fyl2x` `x87_fyl2xp1` `x87_fpatan` | — | — |
| `x87_fxch` | i32 | — |
| `x87_ffree` | i32, i32 (pop) | — |
| `x87_fincstp` / `x87_fdecstp` | — | — |
| `x87_fcmov` | i32, i32 (take) | — |
| `x87_fcom_sti` | i32, i32 (quiet), i32 (pops) | — |
| `x87_fcom32` | i32, i32 (pops) | — |
| `x87_fcom64` | i64, i32 (pops) | — |
| `x87_ficom16` / `x87_ficom32` | i32, i32 (pops) | — |
| `x87_ftst` / `x87_fxam` | — | — |
| `x87_fcomi` | i32, i32 (quiet), i32 (pop) | i32 (CF bit0, PF bit2, ZF bit6) |
| `x87_fnstcw` | — | i32 |
| `x87_fldcw` | i32 | — |
| `x87_fnstsw` | — | i32 |
| `x87_fnclex` / `x87_finit` / `x87_fwait` | — | — |
| `x87_fnstenv` `x87_fldenv` `x87_fnsave` `x87_frstor` | i32 (address) | — |

The `op` argument's encoding is `x87/src/ffi.rs::binary_op`: 0 Add,
1 Sub, 2 SubReverse, 3 Mul, 4 Div, 5 DivReverse. Mirror that as an enum
in the translator, do not re-derive it.

**2. The scan flag.** `TextReferences` (`src/transpile.rs:1587`) gains
`uses_x87: bool`, set in the same instruction scan that sets
`issues_syscalls` (line 667) whenever the mnemonic is in the x87 set
(X2's `is_x87_mnemonic` — write that predicate first and share it).
Imports must be declared before any body is built, because imports
occupy the low end of the function index space; the comment at
`transpile.rs:876` explains what breaks otherwise.

**3. Declaration.** In `declare_imported_functions`, when
`references.uses_x87`, declare *all* the helpers (mirror the
`issues_syscalls` block at line 899: `ImportedFunction` with
`ENVIRONMENT_MODULE`, `wasm.intern_type(helper.signature())`, and a
`Symbol` with `symbol_flags::UNDEFINED`). Declaring all of them when any
x87 appears is deliberate — per-helper reference tracking buys nothing,
`wasm-ld` resolves only what is called, and the alternative is a second
scan that can drift. Store them in `SymbolTable` (`transpile.rs:74`) as
a `Vec<Option<FunctionReference>>` indexed by the enum discriminant, or
a fixed-size array.

**4. The resolver.** `SymbolResolver` (`src/translate.rs:39`) gains one
method, mirroring `syscall_entry`:

```rust
fn x87_helper(&self, helper: X87Helper) -> Result<FunctionReference>;
```

Error text on the missing case mirrors the syscall one at
`transpile.rs:190`: "an x87 instruction was translated but its helpers
were never declared; the scan and the declaration disagree". Tests that
implement `SymbolResolver` by hand (grep for `impl SymbolResolver` under
`tests/`) get the same one-line `bail!` default — give the trait method
a default body that bails, so only `transpile.rs` implements it for
real.

**Acceptance:** an object containing one `fld` instruction transpiles
and the emitted object's import section names `x87_fld64` with the right
type (assert via `wasm_reader` or `wasmparser` in a small test beside
the existing emitter tests, `tests/emitter_linking.rs` style).

---

## X2 — The lowering module: `src/translate/x87.rs`

Mirror `src/translate/vector.rs` exactly in shape: a `pub(super) enum
X87Outcome { Translated, NotAnX87Instruction }`, one entry point

```rust
pub(super) fn translate_x87(
    translator: &mut FunctionTranslator,
    body: &mut FunctionBodyBuilder,
    lifted: &LiftedInstruction,
) -> Result<X87Outcome>
```

(or a method on `FunctionTranslator`, as `translate_vector` is — follow
whichever it actually is), dispatched from the fallthrough arm of
`translate_instruction` (`src/translate.rs:657`): try vector first as
today, then x87, then the existing "not implemented" bail.

### The three rules that make helper calls cheap

These come from the design doc and they are what a reviewer will check:

1. **No flush, no reload, no stack ceremony.** A helper call is
   `push args; body.call(reference)` — nothing else. Do **not** go
   anywhere near `translate_call`, `reserve_return_address`,
   `reserve_syscall_frame`, `flush_written` or `reload`. The helpers
   cannot name the register-file globals, so promoted state stays
   valid across the call; they cannot block or throw, so they are not
   resume sites and `--resume` never sees them.
2. **All addressing stays in the translator.** f32/f64/integer memory
   operands are loaded/stored with the existing machinery
   (`read_operand` / `write_operand`, `src/translate.rs:737/811`) and
   passed **by value**. Only m80 and the env images pass an address:
   `emit_effective_address` (`translate.rs:845`) pushes the i32,
   segment prefix included.
3. **Flag and register effects go through the normal state paths**:
   `self.state.write_register` for `fnstsw ax`,
   `self.state.write_flag` for `fcomi` — never a global directly.

### The mnemonic table

iced decodes x87 with per-form mnemonics; `instruction.memory_size()`
distinguishes operand widths (`MemorySize::Float32/Float64/Float80`,
`Int16/Int32/Int64`, and — usefully — `FpuEnv28` and `FpuState108` for
the env images). An ST-register operand is `OpKind::Register` with
`Register::ST0..=ST7`; its index is `register.number()`. Every row
below is in scope for this milestone; anything x87-shaped *not* below
falls through to the existing refusal (correct: per-function, loud).

| iced mnemonics | Lowering |
| --- | --- |
| `Fld` (m32/m64) | `read_operand` at Double/QuadWord → `x87_fld32/64` |
| `Fld` (m80) | `emit_effective_address` → `x87_fld80` |
| `Fld` (ST(i)) | `x87_fld_sti(i)` |
| `Fld1 Fldl2t Fldl2e Fldpi Fldlg2 Fldln2 Fldz` | `x87_fld_const(0..6)` in exactly that order |
| `Fild` (m16/m32/m64) | `read_operand` → `x87_fild16/32/64`. m16 arrives zero-extended in i32; the FFI casts through i16, so pass it raw |
| `Fst`/`Fstp` (m32/m64) | `x87_fst32/64(pop)` → park result in a temp local (`self.temporaries.take`) → `write_operand` |
| `Fstp` (m80) | `emit_effective_address` → `x87_fstp80` |
| `Fst`/`Fstp` (ST(i)) | `x87_fst_sti(i, pop)` |
| `Fist`/`Fistp` (m16/m32/m64) | `x87_fist16/32/64(pop)` → local → `write_operand` |
| `Fisttp` | `x87_fisttp16/32/64` → local → `write_operand` |
| `Fadd Fsub Fsubr Fmul Fdiv Fdivr` (m32/m64) | `x87_arith32/64(op, bits)`; `Fsubr`→SubReverse, `Fdivr`→DivReverse |
| `Fiadd Fisub Fisubr Fimul Fidiv Fidivr` (m16/m32) | `x87_arith_i16/i32(op, value)` |
| register forms + `Faddp Fsubp Fsubrp Fmulp Fdivp Fdivrp` | `x87_arith_sti(op, dst, src, pop)`; dst/src from op0/op1 registers, pop from the P-form. **Direction check:** `fsubr st(3), st` must become `dst=3, op=SubReverse`, meaning ST(3) = ST(0) − ST(3). Get this wrong and the corpus catches it; get it wrong *symmetrically* and only the oracle-style asm corpus does — which is why X4 has directed direction cases |
| `Fchs Fabs Fsqrt Frndint Fprem Fprem1 Fscale Fxtract F2xm1 Fyl2x Fyl2xp1 Fpatan Fincstp Fdecstp Ftst Fxam Fnclex Fninit` | the no-argument helper of the same name |
| `Fxch` | `x87_fxch(i)` (with no operand, i = 1) |
| `Ffree` / `Ffreep` | `x87_ffree(i, 0/1)`. `ffreep` is undocumented-but-real and glibc has emitted it; iced decodes it |
| `Fcmovb Fcmove Fcmovbe Fcmovu Fcmovnb Fcmovne Fcmovnbe Fcmovnu` | `emit_condition` (`translate.rs:1465`) pushes the predicate from the *promoted* flags; pass it as `take`: `push i; emit_condition; call x87_fcmov`. If `instruction.condition_code()` returns `None` for the FCMOV forms, map the eight mnemonics to condition codes by hand in this module |
| `Fcom`/`Fcomp` (mem) | `x87_fcom32/64(bits, pops)` |
| `Fcom Fcomp Fcompp` (reg) | `x87_fcom_sti(i, quiet=0, pops)` |
| `Fucom Fucomp Fucompp` | `x87_fcom_sti(i, quiet=1, pops)` |
| `Ficom Ficomp` | `x87_ficom16/32(value, pops)` |
| `Fcomi Fcomip Fucomi Fucomip` | `x87_fcomi(i, quiet, pop)` → local, then unpack (below) |
| `Fnstsw` (AX) | call → the i32 result is on the stack → `self.state.write_register(body, RegisterSlice::of(Register::AX)?)` — Word width, so RAX's upper bytes survive, which is what the hardware does |
| `Fnstsw` (m16) | call → local → `write_operand` at Word |
| `Fnstcw` | same two forms as `Fnstsw` |
| `Fldcw` | `read_operand` at Word → `x87_fldcw` |
| `Fnstenv Fldenv Fnsave Frstor` | `emit_effective_address` → helper |
| `Fwait` / `Wait` | `x87_fwait()`. iced decodes the `9B` prefix as its own `Wait` instruction, so assembler-level `fstsw`/`finit`/`fsave` arrive as *two* instructions: `Wait` then the `Fn*` form — no extra handling needed beyond translating `Wait` itself |
| `Fnop` | nothing |

**`fcomi` flag unpacking.** The helper returns CF at bit 0, PF at bit 2,
ZF at bit 6. Park in a local `r`, then for each of Carry/Parity/Zero:
`local_get r; i32_const shift; i32_shr_u; i32_const 1; i32_and;
write_flag`. Then `i32_const 0; write_flag(Overflow)` and the same for
Sign — FCOMI architecturally clears OF and SF (AF is unmodelled).

**What needs no work, but must be verified once:** the effects analysis
(`src/abi/effects.rs::location_of`, line 130) returns `None` for ST
registers, so iced's ST-register uses are silently ignored — while the
same iced info still reports the *GPR* base/index registers of memory
operands as reads and `fnstsw ax`'s RAX as a write. That means
promotion sees exactly what it needs with zero changes. Write one test
that transpiles an `fnstsw ax` + dependent-`rax`-read sequence under
promotion (both structurer modes) to pin this.

**Acceptance:** every mnemonic in the table transpiles in both
control-flow modes; `tests/lifting.rs`-style snapshot or a directed
`tests/x87_lowering.rs` asserting the emitted call sequence for one
representative of each shape (memory-value, memory-address, register,
flag-writing, AX-writing). No flush/reload appears around any helper
call — assert by rendering the body (`wasmprinter`) and checking no
`global.set` between the argument pushes and the call.

---

## X3 — Linking the staticlib everywhere a translated object can land

The archive only contributes referenced members, so it joins every link
unconditionally — no "does this test use x87" logic anywhere.

1. **`tests/support/mod.rs`**: clone `kisal_staticlib()` (line 1039)
   as `x87_staticlib()` — same `OnceLock` + private
   `CARGO_TARGET_DIR` pattern, directory `target/wasm-x87`, package
   `x87`, `--release`. The separate target dir is not optional: cargo
   takes a lock per directory and the tests are themselves running
   under cargo (the comment above `kisal_staticlib` explains).
2. Append it to the object list in **`DifferentialFixture::build`**
   (the `link_wasm(&wasm_objects, ...)` call), in **`MixedFixture`**'s
   link, and in **`link_container_for_program`** beside
   `kisal_staticlib()`.
3. **kisal's native tests** never see the archive and must not need
   it: any kisal→x87 references land behind
   `#[cfg(target_arch = "wasm32")]` (X6).
4. The production bake path, when it grows a real `bake` invocation
   (M6 appendix: "nothing yet calls them from a bake invocation"),
   inherits the same rule: libx87.a is part of every container link,
   stated wherever that command line gets assembled.

**Acceptance:** the whole existing differential suite still passes with
the archive present (it resolves nothing today — that is the test), and
one new corpus file that *does* reference a helper links and runs.
Run targets, not the world: `cargo test --test differential`,
`cargo test --test interop`, one container test from
`tests/boot.rs`.

---

## X4 — Corpus differentials: the translation proven against native

Two new corpus files plus directed cases, all through the existing
`DifferentialFixture` (native shared-library oracle vs every
compiler × code-model × optimisation × mode combination).

**The ABI trap to not fall into:** SysV passes `long double` arguments
on the stack and returns them in `st0`. The harness's typed wrappers
speak integers and doubles only. So corpus functions do their
long-double work *internally* and cross the boundary as `double` or
`long` — e.g.:

```c
double via_long_double(double a, double b)
{
        long double t = (long double)a * b + 0.25L;
        return (double)t;
}
```

**`tests/corpus/long_double.c`** (new). Functions covering, at minimum:

- arithmetic chains with mixed m32/m64/m80 spills (force spills with
  enough locals that the compiler round-trips `fstpt`/`fldt`);
- every cast direction: `long double` ↔ `double`/`float`/`int`/`long`,
  including the compiler's `fnstcw`/`fldcw` truncation dance for
  `(long)x` — build with the default baseline so `fisttp` is *not*
  available, which is the whole point;
- compares that branch (`>`/`<`/`==`/unordered via `isnan`) — these
  become `fucomi(p)` + `jp`/`jb` shapes;
- `fabsl`, `-x`, `sqrtl` if the libc inlines it;
- an `feholdexcept`/`fesetround`-shaped function (fenv round-trip:
  save cw, set chop, convert, restore, return both results);
- a `printf("%.21Lg")`-free strtold-like accumulation loop (parse
  digits into a `long double` by repeated `*10 + d`) — the
  `floatscan` shape without libc;
- denormal-range arithmetic (multiply two `DBL_MIN`-scale values as
  `long double`, return the classification via `fpclassify`).

**`tests/corpus/x87_control.s`** (new, AT&T style like
`parity_flag.s`, `long name(long, long)` convention). Hand-written
because no compiler emits these:

- the `fprem` loop exactly as musl's `fmodl` writes it (`1:` fprem;
  fnstsw %ax; test $0x400,%ax? — actually `sahf`-free form: `fnstsw
  %ax; testb $4, %ah; jnz 1b`), operands arriving as i64 bit patterns
  through memory. This is the C2-protocol end-to-end test, and it
  exercises the emulated partial-step path against a *loop that must
  terminate* — if the step rule were wrong the test hangs rather than
  miscompares, so give it a small iteration bound in the harness
  sense (the transpiled module runs under wasmtime; rely on the
  differential result, the loop converges in ≤ 3 passes for the
  chosen operands and a diverging translation returns wrong bits);
- `fscale`/`fxtract` pairs; `fxam` on each operand class, result via
  `fnstsw`; `fincstp`/`fdecstp`/`ffree` stack gymnastics ending in a
  clean stack; the arithmetic **direction battery**: one case per
  `fsub`/`fsubr`/`fdiv`/`fdivr` × register-form direction whose
  results differ if `dst`/`src` or the reversal is wrong;
- `fnsave`/`frstor` round-trip: save, clobber, restore, prove the
  clobber vanished (return a register through the round trip).

**Acceptance:** both corpus files agree with native across the full
fixture matrix. Then extend `tests/optimisation_sweep.rs`'s source list
(if that is how the sweep picks corpus — check) so the long-double file
rides the existing higher-`-O` sweeps.

---

## X5 — The gate: the grind loses its x87 rows

1. Rebuild the static glibc hello (`gcc -static -O2`), run the
   linked-mode transpile exactly as the M6 worklog entry did, and diff
   the refusal list (`main.rs` prints refusals with reasons; the
   worklog's "Where the numbers went" table is the format to extend).
   `fld` and every other f-mnemonic must be gone from the reachable
   tail. Append the new row to that worklog table.
2. Build a small static binary whose `main` does
   `strtold("3.14159...")`, arithmetic, and `printf("%.21Lg")`, and
   run it end to end through the container path (`tests/boot.rs`
   pattern: transpile, `link_container_with_image`, run, capture the
   write) — output compared byte-for-byte against the native run.
   glibc's `__printf_fp` long-double path plus fenv machinery is the
   real consumer the histogram found; this is it, live.
3. Only after both: run the full workspace suite once, as the
   milestone-completion run the wall-clock discipline allows.

**Acceptance is the diff, not a feeling:** the refusal-tail diff and
the byte-identical output are the two artifacts; both get a worklog
entry.

---

## X6 — Kisal integration points (the two that are due now)

- **`execve` resets the FPU.** Where kisal's exec path resets machine
  state for a fresh image (`kisal/src/exec.rs` — the same place the
  registers/stack are set up), call `x87_reset()`. Declaration
  pattern: exactly `kisal/src/machine.rs`'s
  `#[cfg(target_arch = "wasm32")] unsafe extern "C"` block with
  `#[link_name = "x87_reset"]` — resolved by `wasm-ld` from the
  archive, absent from native test builds. The native `Machine` trait
  side needs a no-op counterpart so kisal's unit tests still run —
  follow how `set_segment_base` is faked there.
- **Fork/snapshot needs nothing.** The state is linear memory; the
  snapshot already carries it. Do not add anything.

The two that are *not* due now, restated so they stay named: M7's
context switch calls `x87_save(ptr)`/`x87_load(ptr)`
(`x87_image_size()` bytes in the TCB) beside
`x86_save_machine`/`x86_load_machine`; M10's sigframe uses
`x87_render_fxsave`/`x87_load_fxsave`, which do not exist yet and are
specified in X7.

**Acceptance:** a container test that execs twice (the exec path exists
per the M6 appendix) with an x87-dirtying first program and a
`fnstsw`-reading second: the second sees FNINIT state.

---

## X7 — Full coverage: the remaining tier-table rows

Each row lists what "done" means and the design decisions already made
(container-plan.md "x87 and MMX" is the authority). Order within X7 is
by expected first contact with a real binary, and every row ends by
flipping its line in `x87/src/lib.rs`'s tier table — that table is the
tracker, keep it truthful.

**X7a — `fsin`/`fcos`/`fsincos`/`fptan`.** Crate-only work plus four
table rows in the lowering. Semantics: C2-partial protocol for
|x| ≥ 2⁶³ (set C2, leave the operand — the machinery `fprem` already
has); `fptan` pushes 1.0 after the tangent (two results, stack-overflow
check before pushing, same as `fxtract`); `fsincos` likewise pushes
two. Accuracy target: correctly-rounded-or-nearly at extended (≤1 ulp)
via double-double argument reduction and polynomial cores on the extF80
primitives — *not* bit-matching any vendor (Intel and AMD disagree;
the 2014 fsin saga is the citation). First version may ship f64-backed
like the existing four **only when** the oracle measures it and the
tier table says so. Oracle: ulp-tolerance tests mirroring
`fyl2x_tracks_hardware_within_tolerance`, plus exact special cases
(±0, the C2 boundary, `fptan`'s pushed 1.0).

**X7b — `fbld`/`fbstp`.** Ten-byte packed BCD ↔ F80, ~20 lines each on
the existing `F80`; m80-style by-address helpers `x87_fbld(addr)` /
`x87_fbstp(addr)`. Semantics worth knowing: `fbstp` rounds per RC,
stores 18 digits + sign, and delivers the BCD indefinite (top two bytes
`FFFF`) on invalid. Host oracle: random valid BCD patterns both ways.

**X7c — `fxsave`/`fxrstor` and the sigframe render.** One 512-byte
layout, two consumers. The crate grows
`x87_render_fxsave(addr)`/`x87_load_fxsave(addr)` writing/reading the
x87 portion: FCW, FSW (TOP composed), the **abridged** tag byte (that
is FXSAVE's format, unlike FNSTENV's full tags), FOP/FIP/FDP zeros, and
ST0–ST7 in 16-byte slots, logical order. The *instruction* lowering is
the one two-writer render in the design: the crate fills the x87
portion, and the translator emits code filling XMM0–15 and MXCSR from
the vector globals after the helper call (`read_vector` per half,
`i64_store` at the documented offsets; MXCSR is not modelled — store
the reset value `0x1F80` and document it). `fxrstor` reverses both.
M10's ucontext fpstate then reuses the same pair from kisal. Oracle:
hardware `fxsave` of a constructed state vs the render, x87 portion
byte-compared, XMM portion compared in the translated-module test
instead (the crate cannot see the globals — that split is the point).

**X7d — MMX.** The biggest row; sequenced last-before-delivery because
nothing SSE2-era emits it, scoped in because x86-64 guarantees it
(CPUID cannot be curated to deny it) and hand-written MMX exists in
real binaries. State: already correct — `mm0..7` are the eight
`significand` fields; an MMX *write* sets `sign_exponent` to `0xFFFF`
and the tag to valid, TOP to 0; `emms` empties all tags. Crate side:
`x87_mmx_read(i) -> i64` / `x87_mmx_write(i, bits)` / `x87_emms()`,
with the ALU ops done **in the translator** as ordinary i64/wasm-SIMD
lane arithmetic on values fetched through those accessors — the crate
owns aliasing, the translator owns arithmetic, mirroring where SSE
arithmetic already lives (`vector.rs`). The 3DNow! extensions stay
refused. Differential corpus: hand-written `.s`, saturating and
wrapping lanes, the `movq mm, xmm`-free x86-64 subset, `emms`
interleaved with x87 to prove the tag interplay.

**X7e — Unmasked-exception delivery.** The last row because it needs
M10's signal machinery live. Already in place now: FSW exception bits
and ES maintained scrupulously (`refresh_summary`), `x87_fwait` as the
named hook. The work: at helper entry (one check in the FFI layer, not
in every op) and in `x87_fwait`, if ES is set and the corresponding
FCW mask bit is clear, record a pending SIGFPE with kisal
(`kisal_raise`-style extern, to be defined against M10's actual API)
— *deferred* delivery at the existing signal-check points, which is
faithful: hardware defers to the next x87 instruction too. Gate test:
`feenableexcept(FE_DIVBYZERO)` + `1.0L/0.0L` + handler runs —
compare against native.

---

## Testing and wall-clock discipline for all of the above

- The crate's own suite (unit + oracle) is the inner loop: sub-second,
  run it freely. `cargo test -p x87`.
- The lowering's inner loop is a *single* differential target:
  `cargo test --test differential <one_test_name>` or the new
  `x87_lowering` test — never the whole fixture matrix per edit.
- The full `DifferentialFixture` matrix for the new corpus runs at
  X4's completion, the workspace suite once at X5's. Both get their
  wall time measured and recorded in the worklog the first time.
- Oracle iteration volumes are sized by measurement and say so in
  `x87/tests/oracle.rs`; the same rule applies to any new oracle
  tests X7 adds.
- Every deviation discovered against hardware gets the
  probe-then-cite treatment the crate already uses: a dated comment
  at the code that encodes the finding, and a worklog line. The five
  existing citations (DE suppression, storeless #D, fscale/PC,
  fprem's step, pseudo-denormal canonicalization) are the format.

## Pitfalls index (things that will otherwise cost an afternoon each)

1. `long double` in an exported corpus signature — the harness cannot
   marshal `st0` returns; keep the boundary integer/double (X4).
2. Emitting a helper call through `translate_call` — it would
   allocate a return-address slot and a resume site; helpers are bare
   calls (X2, rule 1).
3. `fsubr`/`fsubrp` direction — trust the table, then trust the
   direction battery, not intuition; AT&T listings of these mnemonics
   famously lie (X2, X4).
4. Declaring x87 imports lazily mid-build — imports precede defined
   functions in the index space; declare on the scan flag (X1).
5. Sharing cargo target dirs between the test build and the staticlib
   build — deadlock (X3).
6. `fnstsw ax` writing 32 or 64 bits of RAX — it writes AX only;
   `RegisterSlice` at Word width already preserves the rest (X2).
7. Comparing partial-`fprem` condition codes C0/C1/C3 anywhere — they
   are undefined while C2 is set; mask them (the oracle already
   does — copy its mask, `x87/tests/oracle.rs`).
8. Assuming the effects analysis needs ST-register support — it
   ignores them by construction (`location_of` → `None`); verify with
   the pinned test, then leave it alone (X2).
