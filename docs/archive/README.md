# archive

Dated records of how zaqaru came to be what it is. Nothing here describes
the current code; [../architecture.md](../architecture.md) does. These
documents are kept because they hold the reasoning behind decisions the
code embodies, and the measurements that settled them.

The project went through four approaches, and the documents are grouped
by which one they belong to.

**Ahead-of-time translation of x86-64 to wasm** — the first approach,
removed on 2026-09-05 (git tag `before-aot-removal` is the last commit
carrying it):

- `design.md` — the translator's design: relocatable output, symbolic
  lifting, the emulated machine as wasm globals, register promotion.
- `code-discovery.md` — finding functions in stripped binaries.
- `x87-plan.md` — the soft FPU, and its lowering in the translator.
- `container-build-plan.md` — building a container around the translator.
- `implementation-plan.md`, `float-plan.md`, `interop-plan.md`,
  `promotion-plan.md` — earlier plans of the same approach, complete.

**The interpreter, and the kernel under it** — the approach that ships:

- `vm.md` — the case for interpretation as the floor, the machine, the
  block cache, page permissions, threads and preemption, fork.
- `container-plan.md` — the kernel: the syscall-as-call bet, the host
  boundary, the image format, the VFS, processes, and each subsystem.
- `network-plan.md` — sockets: the loopback arena, the host-terminated
  edge, and waiting without spinning.
- `worklog.md` — decisions, roadblocks and mistakes, day by day.

**Compiling hot blocks to wasm at bake time** — built, measured as a loss
on CPython, and removed on 2026-09-05:

- `tier1-plan.md` — the design, and at the end the settled result.

**The bytecode accelerator** — the current direction:

- `bytecode-plan.md` — the design.
- `bytecode-floor.md` — the measurement that motivated it: a minimal
  register-machine interpreter is 7.7× the x86 interpreter under wasmtime.
- `thread-dispatch.md` — the measurement that struck tail-call threaded
  dispatch from the plan: 5.2× slower than a `br_table` under wasmtime.

The milestone and gate names these documents use (M0–M11, V1–V5, D1–D7,
N0–N5, X1–X7, T0–T4, G1–G3, "tier 0/1/2") were the working vocabulary of
the plans and mean nothing outside them.
