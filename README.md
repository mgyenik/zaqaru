# zaqaru

Lifts machine code out of x86-64 ELF object files and lowers it to
WebAssembly.

The input is a relocatable object (`gcc -c`); the output is a **relocatable
wasm object** in the LLVM tool-conventions linking format, so stock `wasm-ld`
links one or more of them into a finished module. Two objects transpiled
independently can call each other and share one emulated register file, which
is the point of emitting relocatable objects rather than finished modules.

```sh
gcc -O1 -c add.c -o add.o
zaqaru add.o -o add.wasm.o
wasm-ld --no-entry add.wasm.o -o add.wasm     # `add` is exported
```

## How it works

```text
ELF .o ──(reader)────► sections + symbols + relocations
       ──(lifter)────► per-function instructions, operands resolved to
                        symbol + addend — never to an address
       ──(cfg)───────► basic blocks, dominators, loop structure
       ──(structurer)► wasm structured control flow
       ──(translate)─► x86 semantics against an emulated machine model
       ──(emitter)───► relocatable wasm object
```

The emulated machine is the load-bearing idea: the sixteen general-purpose
registers are `i64` wasm globals, the flags are `i32` globals, and the guest
stack lives in linear memory with `x86_rsp` pointing into it. Translated
functions therefore assume *nothing* about calling conventions — the binary is
self-consistent with itself, and faithful register emulation inherits that.
Host-callable wrappers marshal arguments at the export boundary.

The globals are the *convention*, not where the work happens: inside each
function body, every register, XMM half and flag the function touches is
promoted to a wasm local, copied in at entry and flushed back at calls and
exits — which is what lets the engine build SSA and register-allocate the
guest's registers. On the benchmark kernels this puts integer and
memory-bound code at parity with clang's own wasm backend.
`--no-promote` turns it off for A/B debugging.

The sixteen XMM registers are *pairs* of `i64` globals rather than `v128`
ones, which is not a preference: stock `wasm-ld` cannot link an object that
defines a `v128` global, because LLD's object reader has no case for a
`v128.const` initializer. SIMD instructions and `v128` locals inside function
bodies are fine — LLD copies code opaquely — so packed operations assemble a
vector from the pair, work on it, and take it apart again. The pair turns out
to fit SSE's grain anyway: a scalar operation writes the low 64 bits and
preserves the high 64, which here means touching one global and leaving the
other alone.

Control flow is translated two ways. The dominator-based structured
translation produces the `block`/`loop`/`if` nesting a reader expects; a
`br_table` dispatcher handles anything it cannot express (an irreducible
graph) and doubles as the oracle the structured mode is tested against.

Function pointers become slots in the indirect function table, and indirect
calls become `call_indirect` — made easy by the emulated convention, since
every translated function has the same wasm type and so signatures can never
disagree. A `switch` compiled to a jump table is the one thing that cannot be
translated at all, because its entries are code addresses; those dispatches
are recognised and rewritten into a `br_table`.

A transpiled module is not an island: given a signature, a guest function's
wrapper becomes an ordinary wasm function that any module can call, and a
call *out* to a function nobody transpiled becomes a generated thunk that
marshals the emulated registers into a typed wasm call. Signatures are
recovered from the machine code rather than read out of debug information,
because the binaries this is for are stripped.

```sh
zaqaru add.o -o add.wasm.o --infer                        # typed exports
zaqaru --thunks add.o foreign.wasm.o -o interop.wasm.o    # calls out
wasm-ld --fatal-warnings add.wasm.o interop.wasm.o foreign.wasm.o -o out.wasm
```

The thunk generator is given the wasm objects too, because that is where a
foreign signature is *stated* rather than guessed — and an argument passed
straight through leaves no trace in the native object at all. A wrong
signature at a seam is caught by the linker rather than run: `wasm-ld` type-
checks across objects, and refuses to connect a mismatch even without
`--fatal-warnings`.

`docs/design.md` has the rationale, the decision log, the current status —
what is built and what a real binary would hit next — and what is
deliberately out of scope.

## Building and testing

```sh
cargo build
cargo test
```

The test suite needs `gcc`, `clang` and `wasm-ld` on the path; wasmtime comes
in as a crate. The backbone is differential execution: every corpus function
is compiled natively into a shared library *and* transpiled, linked and
instantiated, then both are called with the same inputs and must agree
exactly. Each corpus is built forty ways — both compilers, position-independent
and absolute code, at `-O0` through `-O3` and `-Os`, through both control-flow
translations — because a transpiler meant for binaries it did not compile has
to cope with what it is handed, not with flags of its own choosing. Floating
point is compared as raw bits, with NaNs compared as a class in the one place
where the bits are the engine's choice rather than the translation's.

Compiler output covers whichever members of an instruction family that day's
compiler happened to want, which is not the same as covering the family. So
the corpus also carries hand-written assembly for what compilers rarely emit:
an irreducible control-flow graph, every write mask of the SSE move family,
the parity flag consumed after integer arithmetic, and every lane width of
every packed operation.

Everything else — an optimisation sweep over every source at every
configuration, snapshot rendering, linker acceptance, structural checks on
what was recovered, and a diff of our linking metadata against clang's —
exists to make a failure legible before it reaches that comparison.

Snapshots are refreshed with `ZAQARU_UPDATE_SNAPSHOTS=1 cargo test`, and the
resulting diff is meant to be read.

`cargo bench` runs the four-kernel performance benchmark — register-bound,
memory-bound, scalar-float and call-heavy — against clang's own wasm
backend as the ceiling, under wasmtime. It checks every build's results
against the native build before timing anything.

## Layout

```text
src/
  main.rs        CLI: zaqaru input.o -o output.wasm.o [--dump] [--print]
  reader.rs      ELF sections, symbols, relocations
  lifter.rs      decoding, symbolic operand resolution
  jump_table.rs  recovering `switch` dispatches from indirect jumps
  cfg.rs         basic blocks, dominators, loop structure
  structurer.rs  dispatcher and structured control flow
  machine.rs     the emulated register file, flags and stack
  translate.rs   x86 instruction semantics
  translate/
    vector.rs    the XMM register file and everything that moves through it
  abi/           signatures at the boundary with ordinary wasm
    effects.rs   what each instruction reads and overwrites
    infer.rs     recovering signatures from machine code
    marshal.rs   moving values in and out of the register file
  thunks.rs      generated entry points for functions we did not translate
  wasm_reader.rs the types a wasm object states for what it defines
  transpile.rs   what symbols an output object has
  emitter/       the relocatable wasm object format
  dump.rs        human-readable rendering of the front end
tests/
  corpus/        C and hand-written .s sources for differential testing
  specimens/     C compiled by clang's wasm backend, as known-good metadata
  snapshots/     .wat expectations
benches/
  kernels.rs     the promotion benchmark: transpiled vs clang-native wasm
```
