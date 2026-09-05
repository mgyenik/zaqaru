# Threaded dispatch vs a switch loop, under wasmtime — and why

`docs/bytecode-plan.md` §4.1 planned a v2 interpreter with **tail-call
threaded dispatch** — each op handler ending in `return_call_indirect` to the
next, a distinct indirect site per op — projecting the ~1.3–1.5× the
interpreter literature (wasmi, Deegen/LuaJIT-remake) reports over a central
`br_table`. That literature is about **native assembly** interpreters, where a
threaded tail call compiles to a raw `jmp`. This measures whether it holds
under **wasmtime**, and root-causes the result.

## The interpreters (`gen.py`)

Two hand-written wasm interpreters run the *identical* bytecode loop — eight
`add`s, a `dec`, a conditional back-edge, 200 M iterations, 2 billion
dispatched ops — differing only in dispatch:

- `switch.wat`: one central `br_table` over the op, in a `loop`.
- `pinned.wat`: one function per op in a table, dispatched by
  `return_call_indirect`, with the interpreter state (pc, r0, r1) threaded
  through the tail calls as **arguments** (register-pinned, the Deegen design —
  `threaded.wat` is the same but with state in globals, to show it is not the
  memory traffic).

Both leave the same result; `_start` exits `r0 & 0xff` so exit codes confirm
agreement.

## Measured (2026-09-03, this machine, wasmtime 48, pinned to a core)

| interpreter | time for 2 B ops |
| --- | --- |
| `br_table` switch loop | **1.65 s** |
| threaded, state in globals | 8.77 s |
| threaded, register-pinned (state as args) | **8.53 s** |

Threaded is **~5.2× slower**, and register-pinning does not help — so it is not
memory traffic, it is the dispatch instruction. The isolation experiments
(`loop.wat`, `tailself.wat`, `tailind.wat` — a bare loop, a direct
`return_call` counter, an indirect one, 2 B iterations each) decompose it:

| construct | 2 B iterations | per op |
| --- | --- | --- |
| `loop` + `br_if` | 0.39 s | ~0.2 ns |
| direct `return_call` | 2.75 s | ~1.4 ns |
| `return_call_indirect` | 5.95 s | ~3.0 ns |

Two additive costs, both the sandbox's: the **tail call itself** adds ~1.2 ns
over a loop branch (argument setup and frame teardown — a real call ABI, even
in tail position), and the **indirect table dispatch** adds ~1.6 ns more
(table bounds check, funcref load, signature check). A `br_table` is an
in-function jump with none of that, and lets Cranelift keep the interpreter's
state in registers across it. A native threaded interpreter gets a raw `jmp`
for the whole thing; wasm's safe indirect tail call cannot, so per dispatch it
is ~5–15× the `br_table`.

## The conclusion

The `br_table` switch loop is the right — and already the fastest available —
dispatch for a wasm-hosted interpreter, and `targum::bytecode`'s v1 is it.
Tail-call threading is struck from the plan: it is not slow to *build*, it is
slow to *run*, by a factor a measurement settles and a decomposition explains.
The lever for the covered path is therefore not cheaper dispatch (there is
none) but **fewer dispatches** — superinstruction fusion, extending the
compare-branch fusion already built — and cheaper per-op work.

## Reproduce

    python3 tools/thread-dispatch/gen.py     # writes switch/threaded/pinned .wasm
    for f in loop tailself tailind; do wat2wasm --enable-tail-call tools/thread-dispatch/$f.wat -o /tmp/$f.wasm; done
    taskset -c 2 wasmtime tools/thread-dispatch/switch.wasm   # time it; likewise the others
