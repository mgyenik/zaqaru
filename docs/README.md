# docs

Project documentation lifecycle:

- Active documents live here. Each carries a `Status:` line.
- When a document is superseded or its work is complete, move it to
  `docs/archive/` (keep the filename; add a line noting what replaced it,
  if anything).

Current documents:

- [design.md](design.md) — architecture and design rationale for the
  native-code-to-wasm transpiler, including the decision log and the current
  status: what is built, and what is left.
- [container-plan.md](container-plan.md) — draft design for running an OCI
  container image: the syscall-as-wasm-call seam, the in-guest kernel
  (**kisal**), the empirical syscall baseline from tracing a real app, the
  structfs/isotope host boundary (two ll-store imports, services under
  `/iso/`), the baked-image bundle and index format, kisal's VFS and
  resolution model, and per-subsystem designs (threads on the resume
  machinery, futex, mmap, dynamic loading, sockets, signals).
- [container-build-plan.md](container-build-plan.md) — the implementation
  plan for the above: milestones M0–M11 from toolchain gates through
  "Flask, served", with the two integration checkpoints (hello-write,
  static CPython prints), per-milestone acceptance criteria, testing and
  wall-clock discipline, and risks ranked by first impact. **In
  progress**: M0's five toolchain gates are answered with their verdicts
  written into the document; M1 (the kernel seam, kisal and the wasmtime
  runner), M2 (`%fs`), M3 (the baker and the read-only VFS), M4 (the
  overlay and the synthetic `/dev` and `/proc`) and M5 (`brk`, `mmap` and
  the VMA tree) are built. Each milestone's section carries what an
  adversarial review found after it was first reported done.
- [worklog.md](worklog.md) — the working layer under the build plan:
  decisions taken mid-build and why, roadblocks and how they were cleared,
  and the mistakes worth not repeating. Where the build plan records a
  milestone's verdict, this records what it cost to get there.

Archived:

- [archive/implementation-plan.md](archive/implementation-plan.md) — the MVP
  milestones, repository layout, risks and testing discipline. Complete; its
  outcome is recorded at the top of the document.
- [archive/interop-plan.md](archive/interop-plan.md) — the wasm interop
  milestones: typed export wrappers, generated outgoing thunks, and
  signature recovery. Complete; its outcome is recorded at the top of the
  document, including the one thing it got materially wrong.
- [archive/float-plan.md](archive/float-plan.md) — the SSE and
  floating-point milestones: XMM state, the parity flag, scalar semantics,
  the host boundary, and packed operations. Complete; its outcome is
  recorded at the top of the document.
- [archive/promotion-plan.md](archive/promotion-plan.md) — the register
  promotion milestones: the benchmark harness, the storage choke points,
  machine state moved from globals into wasm locals with a flush
  discipline at calls and exits, and the mutation battery that proves the
  discipline. Complete; its outcome is recorded at the top of the
  document.
