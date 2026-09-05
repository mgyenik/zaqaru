# docs

- [architecture.md](architecture.md) — how zaqaru works: the artifact, the
  machine, the kernel, the host boundary, the bake, and the determinism
  that falls out of them.
- [fidelity.md](fidelity.md) — where the kernel differs from Linux, read
  out of the code: what it refuses by name, what it records and ignores,
  and what is not built. Every entry names the program shape that would
  notice.
- [performance.md](performance.md) — what a container costs, where the
  time goes, how to measure it without fooling yourself, and what was
  tried and rejected.
- [time-travel-debugger.md](time-travel-debugger.md) — the design of
  `web/`, a browser page that replays a recorded container run and seeks
  to any instruction. The container is an isotope store served through
  the Server Protocol, so introspection is reads of its paths; a stepping
  export, an exact stop and host-side snapshots do the rest. Built; the
  document says what is not.
- [archive/](archive/) — the design and planning documents the project
  was built from, and the write-ups of measurements that settled
  questions. Dated records, not current descriptions: they describe
  approaches that were later replaced and use a working vocabulary of
  milestones and gates that appears nowhere else.
