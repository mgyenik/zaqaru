# Bytecode floor — how fast a wasm-hosted interpreter can be

The engine interprets x86 directly (`targum::exec` over pre-decoded
`targum::quick::Quick`). This measures how much of its per-instruction cost
is *x86's* and how much is our block-structured design, by comparing it
against a minimal register-machine bytecode interpreter running the same
work — both under wasmtime, so the comparison is of the shipped execution
path, not of native code.

## What `interp.rs` is

A lean register-machine bytecode interpreter: a flat instruction stream in a
`Vec<u64>` (one word per op: `[op:8][d:4][a:4][b:4][imm:32]`, a two-word
`LI64` for full-width constants), sixteen registers, a switch-loop dispatch,
and **faithful** memory — `load`/`store` carry a page-permission bitmap check,
as the engine must. Its `mixed` kernel mirrors `bench.c`'s `kernel_mixed`
op-for-op: an LCG, a checked load and store into a 16 KB region, and a
data-dependent branch the predictor cannot win — the shape an interpreter
loop actually runs.

## Measured (2026-09-03, this machine, under wasmtime 48, pinned)

Per-iteration time on the identical `mixed` computation:

| interpreter | ns/iter | ops/iter |
| --- | --- | --- |
| engine, x86 over `Quick` | 230 | 17 |
| this bytecode interpreter | 30 | 14.5 |

**7.7×**, and it holds per op class — arithmetic ~9×, permission-checked
load/store 5–6×, branches ~12×. So most of the engine's per-instruction
cost is our block-structured design, not x86: the flat stream keeps branches
in the loop (`pc = target`) where ours exits and re-enters per block, and the
lean fixed encoding skips the `Source`-enum operand decode and the
per-instruction budget/`checks_rip`/`has_dirty` machinery.

## Reproduce

    gcc -O2 -static -o /tmp/microbench/root/init tools/microbench/bench.c -lm
    # engine (x86): bake `mixed` at two scales and diff the reported times
    target/release/examples/bake-vm /tmp/microbench/root /tmp/mx1.wasm /init mixed 1000000
    target/release/examples/bake-vm /tmp/microbench/root /tmp/mx2.wasm /init mixed 2000000
    taskset -c 2 target/release/zaqaru-run /tmp/mx1.wasm   # read "instructions in Xs"
    taskset -c 2 target/release/zaqaru-run /tmp/mx2.wasm
    # bytecode: run at two scales under wasmtime and diff wall time
    rustc -O --target wasm32-wasip1 -o /tmp/bc3.wasm tools/bytecode-floor/interp.rs
    taskset -c 2 wasmtime /tmp/bc3.wasm 1000000
    taskset -c 2 wasmtime /tmp/bc3.wasm 2000000

## The point

This is a *floor*, not a finished win. A faithful x86→bytecode transpiler
carries what the kernels omit — widths, address computation for complex
modes, flags where live, and the computed-goto / call-ret target
translation CPython leans on — which lowers the end-to-end multiplier
(estimate 3–6×). But 7.7× on a realistic mix says the headroom is real and
the block-structured x86 interpreter is not near its floor. The design to
capture it: register bytecode, flat pre-resolved stream, dead-flag folded
in at transpile time, direct-threaded dispatch (wasm tail calls, hand-
emitted), JIT x86→bytecode per block and cached like blocks are today.
