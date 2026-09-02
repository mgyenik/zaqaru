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
  machinery, futex, mmap, dynamic loading, sockets, signals, x87/MMX
  soft emulation).
- [code-discovery.md](code-discovery.md) — the design authority for how the
  linked-ELF front end decides where functions are: the strong/weak witness
  invariant (strong witnesses bound, weak witnesses only fill uncovered
  gaps), the built witnesses and the ones still to build, the negative
  filters, the computed-goto trap the invariant defends against, the
  saturated tier that makes an arbitrary binary run when the witnesses
  miss, the front-end fixpoint that lets recovered jump-table arms cut
  (D1–D7), and the survey of the field it rests on.
- [vm.md](vm.md) — **in progress**: the userspace-VM alternative path — an
  x86-64 interpreter compiled to wasm as the correctness floor (tier 0),
  runtime hot-block translation as the accelerator (tier 1), and the AOT
  transpiler demoted to an optional precompiler (tier 2), all under an
  unchanged kisal. Carries the measured throughput spike, the
  SMC/page-permission design that repeals the AOT deal, the collapsed
  thread/signal machinery, the subsystem-by-subsystem reuse inventory,
  and gates G1–G3 with milestones V1–V5; the adoption decision sits at
  V2. **Built through V3** (`targum/`, `kisal/`): the engine, the lockstep
  oracle, the kernel seam, dynamic loading with prelink absent, the bake
  that links engine plus image into a `.wasm`, threads with a
  retired-instruction quantum, signal delivery with faults delivered as
  signals a handler can catch, and the process table of section 7a —
  `fork`, `execve`, `wait4`, `SIGCHLD`, pipes as structural fd hoisting,
  and `poll`/`epoll`. G2 is answered — at 29 MIPS in wasm when it was
  measured, and at 67.7 MIPS since the engine work of 2026-08-31; see
  [performance.md](performance.md). The deliverable:
  the official `python:3.12-slim` OCI image as a 119.8 MB `.wasm`, running
  CPython that forks subprocesses and shell pipelines. Sockets have since
  been built — see [network-plan.md](network-plan.md). G3, tier 1 and
  V4's JIT trophy are not, and the document says which is which.
- [x87-plan.md](x87-plan.md) — the plan that finishes the x87: symbol
  plumbing and the translator lowering (X1–X2), linking the staticlib
  into every build (X3), corpus differentials (X4), the refusal-tail
  gate (X5), kisal integration points (X6), and the full-coverage rows
  through MMX and unmasked-exception delivery (X7) — written to be
  executable without archaeology, with a pitfalls index.
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
- [network-plan.md](network-plan.md) — **built**: the design and plan for
  networking under the interpreter, aimed at the nginx+gunicorn+django
  demo served by `zaqaru-run` and answered by `curl`: the two-network
  split (loopback as arena state, the edge as host-terminated streams —
  no packets anywhere), the `/iso/net` store protocol over the unchanged
  two imports, the blocking wait read that retires the idle spin, the
  syscall surface priced against the real stack with its potholes named
  (`SCM_RIGHTS`, `MAP_SHARED`-across-fork, netlink), the amendments to
  fold back into `container-plan.md`, and gates N0–N5 from a native
  strace baseline through "curl gets the Django page".
  **N0–N5 are built (2026-08-30) and the demo answers.** An ordinary
  Dockerfile — `python:3.12-slim` plus nginx, gunicorn and Django — baked
  to one 170 MB `.wasm`, served by `zaqaru-run -p 8080:80`, and
  `curl http://localhost:8080/` returns the Django page: five processes
  inside the module, nginx's own access log naming the client. It idles at
  **0% of one core** between requests, and Ctrl-C ends the tree the way
  `docker stop` does — the worker told over the channel socketpair, the
  master reaping it by `SIGCHLD`, exit status 0. The same stack traced both
  ways diffs to **one structural divergence** (there is no vDSO, so glibc
  reads the clock by syscall where a native run reads it from a mapped
  page) plus four single calls, recorded in
  `demo/hello-django/baseline/n5-diff.txt`. And a served HTTP session
  **records and replays byte for byte with no network at all**, which is
  the determinism claim demonstrated rather than asserted.
  `demo/hello-django/` holds the image and the scripts. Both of those
  claims are now checked by tests rather than by running the demo — the
  tape against a world deliberately changed underneath it, and the edge
  by a guest that publishes a port the test's own client connects to,
  which is what found three defects in the close path that a long-lived
  nginx cannot show you.
- [tier1-plan.md](tier1-plan.md) — **design**: hot blocks compiled to
  wasm at run time, step 2 of the performance plan. A plain-language
  introduction first, then the design: the trace unit and its exits, the
  seam with the run loop, the one-import host contract, the `Quick`
  lowering compiled with a helper for everything it declines, registers
  and lazy flags in locals, exits to the interpreter as the whole of the
  fault model, invalidation shared with the block cache, regions as the
  source of speed, selection and batching, the budget rule that keeps a
  tape replayable across tiers, gates T0–T3 with negative controls,
  risks, and a seeded pitfalls index.
- [performance.md](performance.md) — **reference**: what a container
  costs, where the time goes, and what has been tried. The Django demo
  native against wasm and the nine workload shapes with their ratios; the
  three instruments — a mnemonic histogram, a guest-address profile, and
  `perf` against wasmtime's JIT map — with the command for each; the
  measurement discipline, which is not preamble, because two of the
  numbers this file used to carry were artefacts of the harness; the
  table of six changes that were argued from the code and measured at
  nothing or worse; and the inlining rule that every real win since
  turned out to be. Carries the three-step plan of 2026-09-02 — page
  ownership for the process switch (built: a warm request from 41 to 20
  ms), tier 1 (waiting), a boot snapshot (waiting) — what is open, and the
  record of the concurrency collapse that turned out to be a ring slot
  reused under a half-closed socket.
- [fidelity.md](fidelity.md) — **reference**: the index of where kisal
  differs from Linux, read out of the code rather than the plans. The
  three answers a guest can get for something not fully built — a loud
  refusal, a recorded no-op, an honest errno — and then the lists: the
  31 named refusals with the program shape each one needs, the no-ops
  that succeed and change nothing with the evidence that allowed each
  and what would notice, and what is simply not built (egress, DNS,
  `SCM_RIGHTS`, `MAP_SHARED` across fork). Carries the one-line grep
  that checks its own refusal count.
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
