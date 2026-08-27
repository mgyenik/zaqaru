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
