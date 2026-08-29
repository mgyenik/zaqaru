# Building the container runtime: kisal, the baker, and the seams

Status: **in progress — M0 through M5 built and green (2026-08-28); M6's
front end and the dynamic tier built (2026-08-29).**
[container-plan.md](container-plan.md) is the design authority this
plan implements; where the two disagree, the design doc wins and this
plan gets corrected. M0's verdicts get written into its section as the
gates run; milestone deviations fold back into the text per the
discipline section.

## What this delivers, and what it does not

**The deliverable is one file.** At the end of this plan, a bake
produces `container.wasm` — a statically-built CPython + Flask
hello-world image, flattened, indexed, transpiled and linked into a
single module — and a wasmtime-based host runs it: `curl` against the
mapped port returns `Hello, World!`, concurrent requests are served by
werkzeug's thread-per-connection model on kisal's green threads, and
writing `/iso/shutdown/requested` produces a SIGTERM, a clean guest
exit, and a status in the `/iso/shutdown/complete` payload. The
acceptance oracle is the empirical baseline itself: the syscall-visible
behavior of the run, diffed against the Linux strace that
[container-plan.md](container-plan.md) was designed from.

Three durable artifacts get built to make that happen, and all three
outlive the hello-world:

- **kisal**, the in-guest kernel: a Rust crate compiled to a
  `wasm32-unknown-unknown` staticlib and welded in at image link time,
  with its logic (index parsing, overlay, futex queues, epoll
  semantics, signal bookkeeping) unit-tested natively as ordinary Rust.
- **The baker**: the tool that flattens OCI layers into the
  blob + index segments, emits `image.wasm.o`, transpiles the image's
  ELFs, and drives the final `wasm-ld` link.
- **The zaqaru extensions**: the `syscall` rewrite and its generated
  thunk, the `save_machine`/`load_machine` helpers, the EH shim pair,
  and `%fs`-segment translation — all extensions of the existing
  thunk/emitter machinery, each landing with corpus coverage in the
  existing differential suite.

**The static tier is musl.** The image is built on musl-static
binaries — `python-build-standalone` ships a musl-static CPython, so
this is an off-the-shelf artifact, not a bespoke build. The reasons
are surface area, both kinds: musl's libc has no ifunc zoo, no
fortified AVX string-function selection, and a far smaller hand-asm
footprint than glibc, so both the instruction-translation surface and
the syscall surface shrink to something a milestone can grind through.
glibc is not being dodged — it arrives with the dynamic tier, where
the CPUID-curation and shadow-GOT designs exist precisely to meet it —
it is being *sequenced*. Fallback if musl-static CPython surprises us
(M0 checks it exists and runs; M6 is where its real surface bites): a
musl-static BusyBox is the interim breadth target, and the checkpoint
milestone shrinks rather than the tier changing to glibc.

**Explicitly not in this plan**, deferred to phase two with their
designs already written in the design doc:

- ~~**Dynamic linking**~~ — **built, 2026-08-29**; see the appendix at the
  end of this document and the worklog entry of that date. It was listed
  here as phase two on the argument that the static tier keeps it off the
  critical path, which was right about the sequencing and wrong about the
  size of what it defers: a sweep of `/usr/bin` found static to be 2.3% of
  what ships. It was brought forward for that reason, and turned out to be
  a week's worth of estimate and a day's worth of work — `ld.so` itself
  refuses five functions, all of them the `_dl_runtime_resolve` trio that
  `DF_1_NOW` guarantees never runs.

  The correction discovered while planning M5/M6 stands and is what made
  this cheap: the **linked-ELF front end is not deferrable** — a
  musl-static CPython is a linked executable, so consuming linked ELFs
  landed in M6, and the dynamic tier is that front end plus a base per
  file.
- **fork, the router, and fd hoisting** — the target workload never
  forks. The resume machinery fork needs is already built and tested;
  the router is not, and no milestone here creates one.
- **The browser host** — the design keeps the door open (the
  `/pending` idle shape, re-enterable `run()`), and M7 stubs that
  shape, but only the wasmtime host is built and tested.
- **Preemption on by default** — the poll emission lands with the
  translator work when convenient, but the Flask target blocks at
  syscalls constantly and does not need it; enabling and measuring it
  is phase-two hardening.
- **Metadata enforcement** — the baker preserves ownership, modes,
  and xattrs in full (per the design doc); kisal honors none of it
  yet.
- **Performance work of any kind.** Working first, then benchmark:
  the deferred-performance list in the design doc (clock-path cost,
  boundary copies, dispatcher cost at eval-loop scale) is measured at
  M11, not optimized before it.

One boundary of a different kind: the **isotope spec extensions** this
plan needs (ns-typed time paths, the kernel-events path, `/iso/net`,
console paths, exit status in `shutdown/complete`, `/iso/proc/spawn`)
are changes to the structfs repo, not this one. This plan treats them
as interface agreements: the host side implemented here is written
against the proposed paths, and the asks are tracked in the design
doc's mapping table.

## The spine

The order is dictated by the trace and by what blocks what — never by
what is interesting. The design doc's priority list said it once:
80% of the target's syscalls are filesystem, TLS blocks every libc
instruction, and concurrency arrives only when requests do. The
milestones follow that gradient, with two integration checkpoints
placed where they convert the most unknown-unknowns into loud errors
for the least work.

**Checkpoint one: M1, every seam exercised once — depth.** A
hand-written static guest (deliberately *no libc*, so it runs before
TLS exists) whose single `write(1, "hello")` crosses every layer this
project adds: translated `syscall` → generated thunk → Rust kisal →
ll-store mount → host. Nothing else is built, so a failure names its
seam. The EH shim links and validates here too, five milestones before
its first real throw (M6's `exit_group`) — toolchain risk retired at
the first opportunity, not discovered mid-scheduler.

**Checkpoint two: M6, syscall breadth without concurrency.**
`python -c 'print("hello")'` from a baked musl image needs the VFS
torrent, mmap/brk, TLS, and the long tail of small syscalls — but
zero threads, zero sockets, zero signals. It is the cheapest program
that exercises the *breadth* of the surface, and it is where
unimplemented-syscall and unimplemented-instruction loud errors get
ground out against a real binary instead of speculation. Everything
before M6 exists to make M6 attemptable; everything after it is
concurrency-shaped.

The dependency structure, stated so deviations are visible:

- **M0 → everything**: three toolchain facts nothing should be built
  on top of unverified.
- **M1 → everything after it**: all syscalls flow through its seams.
- **M2 (TLS) → any libc code at all**: musl keeps the thread pointer
  (and errno behind it) in `%fs` from startup, so even a
  single-threaded C hello-world dies without it. This is why M1's
  guest is libc-free by design.
- **M3 (baker + read-only VFS) and M2 are independent** — translator
  work and Rust/tooling work, parallelizable if wanted. M4 (overlay,
  synthetics) and M5 (memory) stack on M3 and M1 respectively.
- **M6 = M1+M2+M3+M4+M5 integrated** against a real interpreter.
- **M7 (threads) needs M1's EH shim and nothing from the VFS side**;
  it is ordered after M6 so the M6 grind happens with deterministic
  single-threaded execution — every failure a straight-line replay,
  never a scheduling artifact.
- **M8 (readiness) builds on M7's parking; M9 (the edge) on M8 plus
  host-side work; M10 (signals) on M7's TCB and timer machinery.**
- **M11 = the whole spine under load**, and the first place
  performance is measured at all.

Two properties of this ordering worth keeping deliberate. First, the
grind is front-loaded: M1–M6 is where unknown surface lives (real
binaries, real instruction sets), while M7–M10 is mostly kisal-internal
Rust whose logic is host-testable before it ever runs under emulation —
so risk falls as the plan proceeds, not the reverse. Second, every
milestone leaves permanent residue: corpus programs, native kisal
tests, and differential harnesses accrete into the standing suite, so
M6 and M11 are gates that stay closed behind us rather than
demonstrations that happened once.

## Milestones

### M0 — Toolchain gates

Five facts the whole plan stands on, none of them verified yet. Each
gate is a spike measured in hours, with a yes/no outcome recorded here
and a named reroute on "no" — the point is to move every "should work"
into "checked" before anything is built on top of it.

1. **structfs crates cross-compile.** `cargo build --target
   wasm32-unknown-unknown -p structfs-ll-store -p structfs-core-store`
   with async features off. Reroute on no: feature-gate patches
   upstream in the structfs repo (we own it); an architectural
   blocker is not expected — the dependency surface is
   serde/bytes/thiserror.
2. **A rustc object links with a zaqaru object and the call works.**
   A transpiled guest calling an `extern "C" fn add(i64, i64) -> i64`
   from a Rust staticlib, through `wasm-ld`, running under wasmtime.
   This checks the unglamorous things that actually break links:
   linking-metadata versions, the target-features section (rustc
   declares features; `wasm-ld` checks compatibility with ours), and
   symbol visibility conventions. Reroute on no: align the emitter's
   feature/metadata declarations — this gate exists precisely because
   that alignment is our code, not fate.
3. **`wasm-ld` links EH.** A relocatable object from our emitter
   carrying a tag symbol and throw/try opcodes, linked against a
   second object, producing a valid module. Tag symbols exist in the
   tool-conventions linking format and C++ wasm exceptions ride this
   path, so expectation is yes — but which EH *flavor* (the
   standardized `exnref`/`try_table` form vs. the legacy
   `try`/`catch` form) survives the link is part of the answer.
4. **wasmtime runs it.** The same module: A calls B, B throws, the
   frame between has no catch, the top-level catch runs and state is
   intact. The gate's output is concrete: which flavor, behind which
   engine flag, at which wasmtime version. Reroute on no: the
   host-unwind fallback already sketched in the design doc (the shim
   becomes a host trampoline; `run()` re-entry instead of an
   in-module catch) — a wider seam, same structure, decided *now* so
   M7 is designed against reality.
5. **musl-static CPython is real.** Fetch the `python-build-standalone`
   musl build, confirm it is genuinely static, run it natively, and
   strace a `print("hello")` to sanity-check its syscall surface
   against the design doc's baseline (no `io_uring`, no surprises).
   Reroute on no: the M6 checkpoint target shrinks to musl BusyBox,
   per the scope section — the tier does not change.

Acceptance: all five verdicts written into this section with the
observed versions and flags, and any reroute folded into the affected
milestone's text before M1 begins.

#### Verdicts — run 2026-08-28

Host: Linux 6.17, rustc 1.97.1, Ubuntu clang/LLD 18.1.3, wasmtime
48.0.1 (7bac2c277), `wasm32-unknown-unknown` target installed.

1. **structfs crates cross-compile — YES.** `cargo build --target
   wasm32-unknown-unknown -p structfs-ll-store -p structfs-core-store
   --no-default-features` succeeds in 5.5 s from a cold registry. Both
   crates already have `default = []` with `async` opt-in, so the
   feature-gate reroute is not needed. Dependency surface as expected:
   `bytes` 1.11, `thiserror` 2.0, `serde` 1.0, `collection_literals`.

2. **A rustc object links with a zaqaru object and the call works —
   YES.** `guest.c` calling `long add(long, long)` was transpiled
   (`--infer --signatures`) with a generated thunk object, and linked
   by stock `wasm-ld` against `librustside.a` — a `#![no_std]`
   `crate-type = ["staticlib"]` crate for `wasm32-unknown-unknown`
   defining `add`. `wasmtime --invoke compute 7 5` returns `24`. No
   linking-metadata, target-features or visibility complaint: rustc
   declares `+mutable-globals +sign-ext ...`, which is a superset of
   our two, and LLD accepts the union. The unglamorous risk this gate
   existed to find is retired.

3. **`wasm-ld` links EH — YES, for the parts of the format that are
   ours to get right.** Objects carrying a tag are linked across the
   object boundary by LLD 18 (a `try`/`catch` in `a.o` receiving a
   throw raised from `b.o`'s code). The wire facts our emitter now has
   to match, read out of clang's objects rather than out of the spec
   text: the symbol-table entry kind for a tag is **4** (`SYMTAB_TAG`,
   spelled `TAG (0x4)` by `llvm-readobj`), the code relocation for a
   tag immediate is **10** (`R_WASM_TAG_INDEX_LEB`, no addend), and a
   defining object emits a **tag section (id 13)** whose entries are
   `(attribute=0, type_index)`. LLD copies code bytes opaquely and
   only patches recorded relocation offsets, so the EH *flavor* in
   those bytes is ours to choose — see gate 4.

4. **wasmtime runs it — YES, in the standardized flavor, unflagged.**
   A three-frame test (`run` catches, `middle` has no catch, `deep`
   throws) using `try_table`/`throw` runs on stock `wasmtime 48.0.1`
   with no engine flag, unwinds straight through the catchless middle
   frame, and leaves a mutable global written before the throw intact
   on the catch side — which is the property the scheduler design
   rests on. The **legacy** `try`/`catch` form that clang 18 emits is
   *rejected* by the same engine ("legacy_exceptions feature required
   for try instruction"). Verdict with teeth: **the emitter emits
   `try_table`/`throw`, not the legacy pair.** That costs nothing —
   we generate the shim's code bytes ourselves and never consume
   clang's — and it means no host-unwind fallback is needed, so the
   M7 design keeps its in-module catch.

5. **musl-static CPython is real — YES.**
   `cpython-3.11.16+20260825-x86_64-unknown-linux-musl-lto+static-full`
   from `python-build-standalone` (release 20260825, 53 MB
   compressed): `file` reports *statically linked, with debug_info,
   not stripped* — which is exactly the unstripped artifact M6's code
   discovery was scoped against. It runs natively and
   `print("hello")` costs **491 syscalls, 27 distinct**.

   One finding folds forward into M3, and it is a correction rather
   than a detail. The plan's M3 text named "the four stat flavors plus
   `statx` (glibc's) and `newfstatat` (musl's)" and wrote `openat`
   throughout. This musl build calls neither: the trace is `stat` ×53,
   `fstat` ×14, `lstat` ×1 and `open` ×18 — the *legacy* numbers,
   because musl's `stat.c` uses them on x86-64 and its `open` is a
   real syscall rather than an `openat` wrapper. **M3 implements
   `open`/`stat`/`lstat`/`fstat` as first-class rows**, with
   `openat`/`newfstatat`/`statx` present because CPython and any glibc
   tier will want them, not because this trace does.

   The rest of the surface is as designed: `mmap` ×124 / `munmap`
   ×117 (M5), `rt_sigaction` ×65 and `rt_sigprocmask` ×5 (recorded at
   M6, live at M10), `getdents64` ×15, `fcntl` ×14, `read` ×11,
   `ioctl` ×11, `lseek` ×15, `close` ×12, `brk` ×2, and singletons of
   `arch_prctl` (M2), `set_tid_address`, `readlink`, `getcwd`,
   `getrandom`, the `getuid`/`getgid`/`geteuid`/`getegid`/`gettid`
   identity family, `write`, and `exit_group`. No `io_uring`, no
   `rseq`, no `clone`, no `futex` — a single-threaded interpreter
   start is exactly the M6-shaped workload the checkpoint assumed.
   The `mmap`/`munmap` volume is higher than the Flask glibc baseline
   and lands entirely on M5's interval surgery.

**One design deliverable rides along with the gates**, because gate 2
forces it: the **core-wasm lowering of the ll-store ABI**. The
featherweight WIT is component-level; a core module needs a concrete
calling convention for `ll_read`/`ll_write` (path and payload as
pointer/length pairs into linear memory, result and result-path
delivery, error encoding). It gets written down here as an interface
agreement with the structfs repo, chosen to align with what the
component canonical ABI would lower to — so wrapping the core module
as a featherweight Block later is mechanical, not a migration.

#### The lowering, settled

The WIT it lowers, unchanged:

```wit
read:  func(path: list<list<u8>>)               -> result<option<list<u8>>, string>;
write: func(path: list<list<u8>>, data: list<u8>) -> result<list<list<u8>>, string>;
```

Two imports from module `env`, and one export the guest owes back:

```wat
(import "env" "ll_read"  (func (param i32 i32 i32)))          ;; path, path_len, retptr
(import "env" "ll_write" (func (param i32 i32 i32 i32 i32)))  ;; path, path_len, data, data_len, retptr
(export "cabi_realloc"   (func (param i32 i32 i32 i32) (result i32)))
```

Every choice here is the canonical ABI's, not ours — which is the
whole point: the component wrapper later is a shim over the same
memory layout, not a translation.

- **`list<T>` is `(ptr: i32, len: i32)`**, `len` counted in elements.
  A `list<list<u8>>` therefore points at `len` records of eight bytes,
  each `(segment_ptr, segment_len)`; alignment 4.
- **Results travel through a return area**, because both results
  flatten to more than one value. The caller — kisal — supplies
  `retptr`, four-byte aligned, into its own memory. The host writes
  the result there and returns nothing.
- **`ll_read`'s return area is 16 bytes**: `[0]` outer discriminant
  (0 = ok, 1 = err); on ok, `[4]` inner discriminant (0 = none,
  1 = some) and `[8..16]` the `(ptr, len)` of the bytes; on err,
  `[4..12]` the `(ptr, len)` of the error string. The discriminants
  are one byte written into a four-byte slot, which is the canonical
  padding rule, not a convenience.
- **`ll_write`'s return area is 12 bytes**: `[0]` discriminant, then
  `[4..12]` a `(ptr, len)` that is the result-path's element array on
  ok and the error string on err.
- **The host allocates returned bytes in guest memory** through the
  guest's exported `cabi_realloc(old_ptr, old_size, align, new_size)`,
  which is how the canonical ABI already moves a `list` from host to
  guest. Kisal's implementation is a bump allocator over a per-call
  arena the syscall handler resets, so nothing leaks and no free ever
  crosses the boundary.
- **Errors are strings, and kisal maps them, never propagates them.**
  A store error becomes an errno at the syscall row that provoked it;
  the string reaches `/iso/log/error` and nothing else. There is no
  errno on this boundary and there never will be — that is the
  POSIX-lives-in-kisal rule applied to the downward face.

The one ask this adds to the design doc's mapping table: nothing.
This is a lowering of the interface that already exists, deliberately
introducing no new capability.

### M1 — The seams: hello, write(2)

One hand-written static guest, no libc, whose `write(1, "hello")`
crosses every layer this project adds. Each piece is deliberately
minimal; the milestone is the *seams*, not the surface.

**The `syscall` rewrite.** The translator learns exactly one new
instruction, and learns it as a call: `syscall` translates through the
existing `emit_transfer` machinery as a direct call to the symbol
`x86_syscall` — which means it reserves a return-address slot like any
call site, so under `--resume` every syscall site is a resume site
*for free*, with nothing extra to build now and everything M7 needs
already in place.

**The generated syscall thunk.** A new product of the thunk generator
(a sibling of `build_thunk_object`): defines `x86_syscall` in the
guest convention, marshals the Linux syscall ABI out of the register
globals — `rax` number, arguments from `rdi, rsi, rdx, r10, r8, r9`,
the `r10`-for-`rcx` twist included — into a typed call
`kisal_syscall(nr, a1…a6) -> i64`, stores the result to `rax`, zeroes
`rcx` and `r11` (the hardware clobbers them; musl's wrappers mark them
clobbered, so any value is conformant and zero is deterministic), and
pops the return-address slot the call reserved. `kisal_syscall` is an
undefined typed import the linker must resolve against the kisal
staticlib — the seam `wasm-ld` type-checks.

**Kisal v0.** The crate skeleton with its native test harness, a
syscall dispatcher whose table has one real row — `write` to fd 1/2,
forwarded to the console mount as
`ll_write(/iso/console/stdout, bytes)` over the core ABI from M0 —
and one policy: every other syscall is a **loud error naming the
syscall** (number and name), surfaced as a host-visible failure, never
a silent `ENOSYS`. Errno mapping and the `Blocked`-return plumbing
exist as types with one variant used.

**The host runner.** A minimal wasmtime embedder (test-support
grade): instantiates `container.wasm`, provides the two ll-store
imports over an in-memory mount table with a console store, calls the
guest entry. This is the seed of the real host; M1 keeps it small
enough to live beside the tests.

**Riders, linked but idle.** The EH shim pair (`run_thread` with its
catch, `kisal_yield` with its throw) is emitted, linked into the
module, and validated by instantiation — exercising gate 3/4's answer
inside the real link, five milestones before its first real throw
(M6's `exit_group`). The
`save_machine`/`load_machine` helpers are emitted with a round-trip
test (save, perturb every global through guest code, load, compare) —
M7's foundation laid while the emitter work is warm.

**The guest and its oracle.** The M1 guest is corpus-style hand
assembly using raw `syscall` — which means *it runs natively*: the
same `.s`, assembled and executed on Linux, produces the same bytes on
stdout. M1's acceptance is therefore differential from day one, not a
demo:

- The write guest passes native-vs-transpiled stdout comparison,
  through both control-flow modes, with `--resume` on and off.
- A second guest invoking an unimplemented syscall (`getpid`) fails
  with the loud error naming it — the policy has a test, not just a
  sentence.
- A deliberately mis-typed `kisal_syscall` signature fails at link
  time (interop-spikes style: the linker's complaint is the asserted
  outcome).
- The EH shim's presence does not perturb the module: instantiation
  and the write guest succeed with it linked in.
- `save_machine`/`load_machine` round-trips the full register file
  bit-exactly.

#### Built — 2026-08-28

**Reported green once before it was**, which is the first thing this
section has to say. The figure written here originally — "108 tests" —
was measured before M2 began and never re-checked; M2 then added a field
to a struct the kernel's own test crate constructs, so `cargo test
--workspace` did not *build*, and two snapshots were stale from the
seventeenth machine global. Neither was noticed because the claim was
made from memory of an earlier run. The standing suite is now 137 tests
and green: 21 seam tests in `tests/kernel_seam.rs`, 27 native kernel
tests in `kisal/tests/dispatch.rs`, 7 mount-table tests in
`runner/tests/mounts.rs`, and the 82 that were already here. The layout the plan called for is on disk:
`kisal/` (staticlib for the weld, rlib for its own tests), `runner/`
(the wasmtime host), and the translator extensions in `src/` —
`src/seam.rs` for the generated seam, plus tag support in the emitter
and the `syscall` rewrite in the translator.

What the acceptance list actually says now:

- The write guest passes native-vs-transpiled comparison across all
  four builds (both control-flow modes × `--resume` on and off), on the
  bytes delivered *and* the value returned. A second case sends a
  shorter write, so the length crossing the seam is the guest's
  argument rather than a constant that happens to be right, and a third
  sends to descriptor 2 and asserts the bytes reached `stderr`'s path
  and not `stdout`'s.
- The loud error names itself: `guest_getpid` stops the run and
  `/iso/log/error` holds `kisal: unimplemented syscall getpid (39)`.
  Its counterpart is asserted too — a descriptor with no backend
  returns `EBADF` rather than faulting, because the policy is about
  syscalls kisal has not implemented, never about calls that
  legitimately fail.
- A `kisal_syscall` taking one argument instead of seven is refused at
  link time, with the complaint naming a signature mismatch on the seam —
  and a correctly-typed kernel links, which is what makes the refusal
  evidence about signatures rather than about anything else that can make
  a link fail. `wasm-ld` on its own *warns* and links; `--fatal-warnings`
  is what refuses, so it is part of the recipe rather than a flourish.
- `x86_save_machine`/`x86_load_machine` round-trip all 412 bytes of the
  register file bit-exactly, under a pattern that makes every cell
  distinguishable from every other and from zero — so a cell either
  helper forgot is a mismatch rather than a coincidence. A second test
  reads the image after a real `write(2)` and finds the syscall's
  result in `rax`, the descriptor in `rdi`, the length in `rdx`, and
  `rcx`/`r11` zeroed.
- A guest with no `syscall` does not import the seam, so the
  instruction scan that declares the import cannot quietly tie every
  object to a kernel it does not need.

**Deviation, and it is the good kind: the riders are not idle.** The
plan said the EH pair would be "validated by instantiation". It is
validated by being *used*: `x86_run_thread` enters a continuation by
table slot under a `try_table`, and the test asserts that the catch
reports the yield and that the register globals are what the throwing
code left. Stated narrowly on purpose, because an earlier draft of this
paragraph overclaimed: the thrower is the *immediate callee* of the
frame holding the catch, so no unwind crosses an intervening frame, and
the only code between the load and the save is the `throw` itself, which
nothing could disturb. What is retired here is toolchain risk — the tag
survives `wasm-ld` with its relocations, and the engine accepts the
standardized form unflagged. Unwinding a real chain of transpiled frames
belongs to M7's first genuine block, which is the milestone that creates
one.
Two small pieces of real machinery made that possible instead of
scaffolding:

- **`x86_yield_slot`**, an exported accessor for a seam function's
  indirect-table slot. A linked module cannot be asked for a table
  index from outside, and M10 needs exactly this for `signal_dispatch`
  "at a reserved table slot". The mechanism exists now and has its
  first user.
- **`Container::install_continuation`**, which grows the exported
  function table by one and puts a host function in it. That is how a
  thread is started from outside the module, and it is what proves
  `x86_run_thread`'s *other* arm — a continuation that returns rather
  than yielding reports zero.

Both oblige the container link to carry `--export-table` and
`--growable-table` alongside `--export=cabi_realloc`. Those three flags
are part of the recipe from here on.

Other deviations from the plan's text, none of them structural:

- **The seam is `src/seam.rs`, not a function inside `src/thunks.rs`.**
  The plan called it "a sibling of `build_thunk_object`" and it is one,
  but the two differ in the way that matters: an interop thunk's
  signature is discovered, and the seam's is fixed by Linux. Its own
  module says so in one place.
- **The helpers are `x86_save_machine`/`x86_load_machine`**, matching
  `x86_resume`'s namespace, and their layout is a stated wire format
  (`zaqaru::seam::machine_image`): seventeen registers — the sixteen
  general-purpose ones and the `%fs` base — thirty-two XMM halves, five
  flags, 412 bytes. Both directions are generated from one
  walk over that layout, which is what makes the round trip mean
  something.
- **kisal names its imports into `env` explicitly**
  (`#[link(wasm_import_module = "env")]`) rather than leaving them
  undefined for `--allow-undefined` to sweep up. A link that turns a
  typo into an import fails at instantiation instead of at build time,
  which would give away the seam's whole point.
- **The kernel-language question in the design doc is settled by
  construction**: kisal is Rust compiled to `wasm32-unknown-unknown`
  and welded in at link time. The seam type-checks across that weld.

Two facts recorded rather than acted on, per working-first:

- The M1 container is **1.5 MB**, nearly all of it Rust's `std` panic
  and formatting machinery reached from the loud-error path. Fine now;
  the lever, if it ever matters, is `no_std` plus kisal's own
  allocator.
- kisal currently uses `std`'s allocator, which grows linear memory on
  its own. **M5 owns the collision this will have with the brk and
  mmap arenas** — its boot carve gives kisal a heap of its own, and
  that is the milestone where the arrangement stops being provisional.


#### What an adversarial review found, and what it changed

M1 was reported done and was not. Three reviewers were run against the
work — one hunting memory hazards, one auditing claims against code, one
mutating production code to see which tests noticed. What they found is
recorded here rather than quietly fixed, because the pattern matters more
than any single defect: **every one of these was a silent wrong answer,
in a project whose stated policy is that nothing fails silently.**

**Two defects that corrupted guest memory.**

- *The seam spent the guest's red zone.* A `syscall` was translated as a
  call in every respect, including handing the kernel a shadow stack
  derived from the guest's `%rsp` — which is correct for a call, whose
  callee may destroy the 128 bytes below it, and wrong for a syscall,
  which Linux preserves. Measured: 11 of 16 red-zone quadwords survived a
  `write`, against 16 natively, with the loss scaling with how deep the
  kernel went. The fix is in the design doc's scheduler section: the
  kernel gets a stack of its own, and the syscall's resume slot is
  reserved below the red zone rather than on top of it. The root cause is
  worth naming — the stack dance was copied from the interop thunk, where
  it is right, and never re-derived against what a `syscall` actually is.
- *Guest pointers were validated at 64 bits and then truncated to 32.*
  `write(1, buffer, 0x1_0000_0005)` returned 4294967296 — claiming four
  gigabytes written — and delivered nothing, which any libc retry loop
  believes. An `arch_prctl(ARCH_GET_FS)` at `0x1_0000_0000` walked past
  an explicit null check and wrote eight bytes at address zero. There was
  no bounds check of any kind and so no `EFAULT` path at all: an
  out-of-range access trapped and killed the whole instance where Linux
  fails one call. `kisal::memory` now does the arithmetic at full width
  against the guest's current memory size, and every pointer-taking row
  goes through it.

**Three more silent divergences.** Flags were left in locals across a
`syscall` on the ABI's authority that they are call-clobbered — true of a
`call`, false of a `syscall`, and decisive because the kernel *snapshots*
the register file from the globals, so a blocking thread would have
resumed with the wrong ones. An `%fs`-prefixed `lea` added the segment
base, which real hardware does not (gas even warns the prefix is
ineffectual while emitting it). And `%gs` — a deliberate loud error —
slipped through unexamined on the relaxed global-offset-table path, which
returns a symbol's address without ever building an effective address.

**Tests that passed with the feature broken.** Proven by mutation, and
each now has a negative control:

| mutation | before | now |
|---|---|---|
| syscall argument register `r10` → `rcx` | 12/12 passed | fails |
| clobber list `[rcx, r11]` → `[rbx, rbp]` | 12/12 passed | fails |
| `FLAG_OFFSET` shifted to alias `xmm15` | 12/12 passed | fails |
| loud error prints `number * 10` | passed | fails |

The causes were all the same shape: assertions that could not distinguish
the right answer from the default one. Nothing passed more than three
syscall arguments, so the `r10`-for-`rcx` twist — the single most
Linux-specific fact in the seam — was unverified; `rcx`/`r11` were
asserted zero after a syscall, which they were anyway because nothing
ever wrote them; the machine image's expected bytes were built from the
same constants the generator walks; and the loud-error check was
`contains("39")`, which `"390"` satisfies.

Worse, the test asserting that a mistyped kernel is refused at link was
passing while `wasm-ld` **linked** it — the test omitted the
`--fatal-warnings` the real recipe uses, and its assertion accepted the
warning text as evidence of refusal.

**And M2's stated acceptance had never run.** `tests/corpus/segment_base.s`
was written, documented, and referenced by no test; the
promotion-on-and-off harness added for it was used by nothing; there was
no `%gs` test. All three now run.

Two things this section should not claim. The review also reported that
M0's gate 5 was unreproducible and wrong about musl; that was checked and
rejected — the artifact, the extracted tree and the 491-line trace are on
disk, `strace` 6.8 names `newfstatat` explicitly and there are none in the
trace, and the binary carries `mov $0x4/$0x5/$0x6,%eax` in bulk against
five `$0x106`. The measurement and the M3 correction stand. And the
thread-local-storage work that had been started here was removed rather
than finished: it accepted `R_X86_64_TPOFF32` and translated it into an
address that is wrong for any program whose libc sets a real thread
pointer, which is a silent wrong answer where an "unsupported relocation
type 23" had been a loud one. Local-exec TLS in the relocatable pipeline
is a real gap and gets a design of its own; a half-built one that
corrupts is not a step towards it.

Explicitly not in M1: `exit_group` (the guest returns from its entry;
process exit semantics arrive with M6, where a real runtime demands
them), `read` on stdin, any second mount, any scheduling. The
milestone ends when a failure anywhere in the stack can name its seam
— that property is the deliverable.

### M2 — TLS: the %fs register

`x86_fs_base` becomes the seventeenth register: a weak `i64` global in
the machine model, promotion-eligible, included in
`save_machine`/`load_machine`. The translator adds the base into the
effective address of any `%fs`-prefixed memory operand — one add on
those instructions, nothing anywhere else. `%gs` stays a loud
translation error. Kisal's syscall table gains its second real row:
`arch_prctl(ARCH_SET_FS | ARCH_GET_FS)`, which writes and reads the
global — arriving, like all its writers, at a flush boundary by
construction.

**The differential guest is self-restoring, and that detail is the
milestone's subtlety.** A corpus C program with `__thread` variables
would drag in TLS relocations (`.tdata`, `TPOFF`) that the
relocatable-object pipeline has no reason to learn — the container
pipeline never sees them, because a linked static binary's TLS setup
is musl's own code operating on concrete addresses. So the corpus
guest is hand assembly that exercises the *mechanism* directly:
`arch_prctl(ARCH_GET_FS)` to save the incumbent, `SET_FS` to a
scratch buffer, `%fs`-relative loads and stores against it, then
`SET_FS` back before returning. Natively that sequence is legal
userspace — and the restore matters, because the native oracle runs
in-process, and leaving `fs` pointing at a scratch buffer would
destroy the test harness's own TLS out from under it.

Acceptance:

- The fs corpus passes differentially — both compilers, both
  control-flow modes, promotion on and off (the new global must ride
  the flush discipline like the other sixteen).
- A `%gs`-prefixed guest fails with the loud error naming the prefix.
- `save_machine`/`load_machine` round-trips `x86_fs_base` with the
  rest of the file.

Explicitly not in M2: TLS *relocations* in the relocatable-object
pipeline (no consumer), the stack-protector corpus flag
(`-fno-stack-protector` stays: a canary check needs a libc-initialized
TCB behind `%fs`, which arrives with M6's real binary — revisit
then).

#### Built — 2026-08-28

`x86_fs_base` is the seventeenth register: a weak `i64` global,
promotion-eligible, reloaded after every call because `arch_prctl` is a
syscall and a syscall is a call. An `%fs`-prefixed memory operand adds it
into the effective address and nothing else pays anything. `%gs` is a
translation error naming the prefix — including on the relaxed
global-offset-table path, which returns a symbol's address without
building an effective address and was therefore the one place a prefix
could pass unexamined. `lea` does *not* add the base: hardware ignores a
segment override where there is no access for it to apply to.

`tests/corpus/segment_base.s` runs differentially against native across
both control-flow modes and with promotion on and off, exercising a
store and a load through the base, a scaled-index form, the `lea` rule,
and `arch_prctl`'s `GET`/`SET` round trip — self-restoring, because the
native oracle runs in this process and leaving `%fs` pointing at a
scratch buffer would destroy the harness's own thread-local storage.
kisal's `arch_prctl` row is native-tested for `SET_FS`, `GET_FS`, the
`EFAULT` on an unwritable destination, `%gs` as a named fault, `CPUID`
faulting as a named fault, and `EINVAL` for sub-functions Linux does not
have either.

**One deviation, recorded because it was a defect and not a scoping
choice.** Local-exec TLS relocations were started here despite the line
above, and left in a state that accepted `R_X86_64_TPOFF32` and resolved
it to an address that is correct only for a guest whose `%fs` base is
zero — silently wrong for any program whose libc installs a real thread
pointer, which is every libc. That work was removed and the relocation is
a named error again. It is a genuine gap in the transpiler's documented
`gcc -c` pipeline (`docs/design.md`, structural gaps) and wants a design:
faithful variant-II offsets need the address of the end of the TLS block,
which `wasm-ld` cannot supply — in this pipeline's non-shared link LLD
relaxes the access to an ordinary absolute load and resolves `__tls_size`
to zero, so there is nothing to compute the block's end from — so the
block has to be laid out by a generated object over the whole link set,
the way the interop thunks already are. (An earlier version of this
paragraph said LLD "double-counts `__tls_base` in a non-shared link,
verified". The reproduction behind that used `-matomics` without
`--shared-memory`, which is the threads TLS model half-configured, not
this pipeline's configuration. The conclusion stands; the evidence for it
was wrong and is corrected in `docs/design.md`.)

### M3 — The baker and the read-only VFS

Two halves that meet at the index format, buildable in parallel with
M2 (tooling and Rust on one side, translator work on the other).

**The baker, v0.** Inputs: a `docker save` tarball *or* a plain
rootfs directory (the test-fixture path — most M3 tests bake a small
tree the native oracle can also walk). Work: apply layers in order
with bake-time whiteout and opaque-dir processing; resolve tar
hardlinks to shared inodes and compute `nlink`; preserve the full
metadata set (mode with setuid bits, uid/gid, timestamps with honest
provenance, uname/gname, xattrs byte-faithful) per the design doc's
inode record; lay out the blob (16-byte alignment, 4 K for
`MMAP_ALIGNED` candidates); emit `image.wasm.o` through the existing
emitter with the `__image_blob`/`__image_index` symbols. Explicitly
*not* in the M3 baker: ELF transpilation at bake — M3 images are
data-only fixtures, and the app is corpus C linked the normal way;
the baker learns to drive the translator in M6.

**Kisal's read-only file layer.** The mount table and the vnode walk
(component-at-a-time resolution: symlink splicing with the 40-hop
limit, `..`, `openat` dirfd relativity, trailing-slash and
`O_DIRECTORY` rules — POSIX in kisal, stores answering only
`lookup(dir, name)`); the fd table with OFDs (`dup`-shared offsets,
per-fd `O_CLOEXEC`); the `/img` store walking the index. The syscall
table grows the read-only torrent: `open` *and* `openat`, `close`,
`read`, `pread64`, `lseek`, `stat`/`lstat`/`fstat` — the legacy
numbers, which are what M0's musl trace actually calls — plus
`newfstatat` and `statx` for the callers that use them, `getdents64`
with real `d_type`, `readlink(at)`, `faccessat`, `fcntl`
(`F_GETFL`/`F_SETFL`/`F_DUPFD`/`F_*_CLOEXEC`), `dup`/`dup2`/`dup3`,
`getcwd`/`chdir`. The dirfd-relative and path-relative forms are one
resolution loop with a different starting vnode, so covering both
costs a row apiece, not a design.

Acceptance:

- **Native index tests**: golden bakes of fixture trees — lookup hits
  and misses, `nlink` on hardlinks, symlink targets, xattr
  byte-fidelity, dirent ordering and `d_type` — as ordinary
  `cargo test` against the kisal crate.
- **The differential harness seed**: corpus C guests (directory walk,
  stat-field dump, symlink chains, seek/offset behavior, negative
  lookups) run natively against a fixture tree *and* under kisal
  against the bake of the same tree; outputs compared. This harness
  is the strace-diff oracle's ancestor and stays for every later
  milestone.
- **Allocation-free is asserted, not narrated**: a counting allocator
  wraps kisal's global allocator in tests, and the stat/lookup path
  shows zero allocations.
- A path resolving through a missing mount, an over-limit symlink
  loop, and the `ENOTDIR`/`ENOENT`/`ELOOP` cases match native
  errno-for-errno.

#### Built — 2026-08-28

**The baker, v0 — the directory path.** `baker::bake_directory` walks a
rootfs tree into a blob and a packed index: hardlinks resolved to shared
inodes with computed `nlink`, directories' `nlink` counted as two plus
subdirectories, mode with setuid bits, uid/gid, timestamps, extended
attributes byte-faithful, 4 KiB alignment for `MMAP_ALIGNED` candidates,
and `baker::object::emit` producing the `__image_blob`/`__image_index`
segments. A bake is reproducible byte for byte.

**Kisal's read-only file layer.** The mount table and the vnode walk; the
fd table with open file descriptions; and the read-only rows: `open`,
`openat`, `close`, `read`, `pread64`, `lseek`, `stat`, `lstat`, `fstat`,
`newfstatat`, `statx`, `getdents64`, `readlink`, `readlinkat`, `access`,
`faccessat`, `faccessat2`, `fcntl`, `dup`, `dup2`, `dup3`, `getcwd`,
`chdir`, `fchdir`, `getxattr`/`lgetxattr`/`fgetxattr`,
`listxattr`/`llistxattr`/`flistxattr`, and `EROFS` for the six xattr write
forms. Standard input, output and error are open before the guest runs,
and `write` resolves through the descriptor table like every other row.

Acceptance, against the four bullets above:

- **Native index tests**: `baker/tests/bake.rs`, twenty of them, over
  lookup hits and misses, hardlink `nlink`, symlink targets, xattr
  byte-fidelity including values and listings larger than the reader's
  first buffer guess, dirent ordering and `d_type`, and a damaged index.
  `tests/image_object.rs` adds what only exists after `wasm-ld`: the blob
  keeps its page alignment, the symbols resolve, and the bytes in the
  running module parse back as the image that was baked.
- **The differential harness**: `tests/filesystem_differential.rs` runs
  the same C program under the real kernel and under kisal, in all four
  build configurations, comparing every record — mode, link count, size,
  ownership, modification time, every errno, every byte read at every
  offset, symlink targets, `access` for `F_OK`/`X_OK`/`R_OK` and a garbage
  mode, the `ELOOP` boundary at forty traversals, `statx` field by field,
  and two directory listings with their `d_type`s. Six exclusions, each
  argued in the file's header rather than discovered by a disagreement.
- **Allocation-free is asserted**: through `Kernel::dispatch`, over six
  hundred real syscalls, hits and misses, alongside a second test that
  plants an allocation and confirms the counter sees it.
- **Errno-for-errno**: `ENOTDIR`, `ENOENT` and `ELOOP` are compared
  against the real kernel in the differential. The "path resolving through
  a missing mount" case has no counterpart: there is one filesystem, and a
  mount point with nothing attached is not a state Linux has either. What
  the mount table is checked on instead is crossing, `..` out of a mounted
  root, per-mount device numbers, and its refusals.

**The `docker save` tarball input**, which closes M3: `baker::tar` (ustar,
GNU and PAX headers, base-256 numeric fields, long-name records, and the
`SCHILY.xattr.*` records that carry extended attributes), `baker::json`
(enough to read a manifest's layer order and refuse anything else),
`baker::layers` (the stack applied in order, with `.wh.` whiteouts and
`.wh..wh..opq` opaque directories resolved at bake time so no runtime ever
learns that layers exist), and gzip decompression, because that is what
modern `docker save` writes.

Both front ends — a rootfs directory and a layer stack — now produce a
`baker::tree::Tree` and share everything below it, so the inode-construction
rules exist once. A test bakes the same tree both ways and compares.

The flattening is checked against `docker export`'s own flattening of the
same image: 524 paths agreeing mode for mode, with the seven differences
being files the container runtime injects. That check needs a daemon, so it
is `baker/examples/image_differential.rs` rather than a test; the suite's
own archives are built with `tar`.

**Deviations and defects** are in [worklog.md](worklog.md)'s M3 sections,
which carry what four adversarial reviews found: the mount table had been
dropped from scope without a word anywhere, four tests could not fail,
the runner's `ll_read` was unreachable from any test, and every
memory-safety defect was a 32-bit narrowing that is harmless on the host
the tests run on and wrong in the module that ships.

### M4 — Overlay and the writable world

The writable world, in two pieces: the overlay, and the synthetic
mounts a real libc probes before it does anything useful.

**The overlay.** Upper memory store in kisal's heap; copy-up on first
write-open of a lower file; runtime whiteouts for unlinks of lower
entries; directory listing as the sorted merge of upper over lower
minus whiteouts; `mkdir`/`rmdir`/`unlinkat`; `O_APPEND`,
`truncate`/`ftruncate`; `utimensat` with faithful mtimes — that one
is load-bearing, because CPython validates `.pyc` files by comparing
source mtimes, so sloppy timestamps silently defeat the bytecode
cache. `flock` as an in-guest lock table on OFDs. Every overlay
operation leans on the standing invariant — atomic unless it
explicitly waits — so `rename` needs no locking story at all.

One semantics decision made Linux's way on purpose: **directory
rename across layers returns `EXDEV`**, exactly as kernel overlayfs
does with `redirect_dir` off. Userspace already handles it (`mv`
falls back to copy); inventing better-than-overlayfs semantics here
would mean behavior no real container has ever been tested against.
File rename covers its three cases (upper→upper in place;
lower-source copy-up then whiteout; replace-existing).

**Synthetics.** `/dev`: `null`, `zero`, `urandom` — and `urandom` is
where the boot entropy lands: kisal's CSPRNG seeds once from
`/iso/random/bytes/32` at startup, serving `/dev/urandom` now and the
`getrandom` syscall row when M6 wants it. `/proc`: the minimal set —
`/proc/self/exe` (symlink to the baked binary's path), with
`/proc/self/maps` explicitly deferred to M5 (it renders the VMA tree,
which does not exist yet) and everything else a loud error until M6's
grind demands it. `ioctl(TCGETS)`: `ENOTTY` everywhere, *including
the console* — v0 decides stdio is not a tty, which is the honest
answer for a container writing to a pipe; CPython responds by block-
buffering, and the image sets `PYTHONUNBUFFERED=1` rather than kisal
pretending to be a terminal.

Acceptance:

- Differential write corpus vs. a native tmpdir: create/write/read
  back, append, truncate, unlink-then-stat, mkdir/rmdir, readdir
  merge with entries in upper, lower, and both, whiteout suppression,
  all three file-rename cases — errno-for-errno.
- The `EXDEV` directory-rename case asserted against native kernel
  overlayfs behavior (fixture: a real overlayfs mount where
  available, golden expectations where not).
- A `.pyc`-shaped mtime round-trip: write, `utimensat`, stat back,
  bit-exact.
- `/dev/urandom` reads draw from the seeded CSPRNG and change between
  boots with different seeds, deterministically replay with the same
  seed — the record/replay property tested at the smallest possible
  scale.

#### Built — 2026-08-28

**The synthetic mounts.** `/dev` with `null`, `zero`, `full`, `random` and
`urandom`, each with the device number Linux gives it; `/proc` with
`self/exe`. Both are built at boot as ordinary images in the baked format —
using `kisal::image`'s own writers, so there is no second definition of the
layout — and attached over the directories the image provides. That means
resolution, `stat` and `getdents64` walk them without knowing they are
synthetic; only *reads* differ, dispatched on the inode's own type and device
number the way a real VFS dispatches to a driver.

`/proc/self/exe` is a symlink whose target the kernel holds. Nothing sets it
until M6's `execve` knows the answer, and reading it before then is a named
fault rather than an empty string — a program that believed its own
executable was called `""` would go wrong somewhere far from here.

**Entropy.** `kisal::random` is ChaCha20 in its RFC 8439 form, checked
against the specification's own test vector, seeded once at boot from
`/iso/random/bytes/32`. A container whose host mounted no `/iso/random` has
no entropy and every request for some is refused by name — never filled with
zeros, which is the one answer that is both plausible and catastrophic. Two
boots with the same seed produce the same bytes in the same order, which is
the record-and-replay property at the smallest scale it can be tested at.

**`ioctl(TCGETS)` is `ENOTTY` everywhere, including the console**, as the
scope above decides. The rest of `ioctl` is a named fault: it is a thousand
unrelated calls behind one number, and `EINVAL` for an unimplemented one
would say "that request was malformed" about a request that was not.

**The overlay.** `kisal::overlay` is an upper layer in the kernel's heap over
the image: copy-up on the first write-open, whiteouts for deletions of lower
names, and merged directory listings. A node number carries its layer in its
high bit, so a vnode is still one `u32` and every part of the kernel that
holds one is unchanged. The merge is a cursor over two sorted sequences
rather than a materialised list, which keeps `stat` and `getdents64` free of
allocation; a directory's size and link count are stored and refreshed when
its entries change, as every filesystem does, rather than derived on every
resolution.

Copy-up happens at `open`, not at the first write, and the reason is worth
recording: the index gives a *directory* a parent pointer and leaves the
field meaningless for everything else, because a file can have several names
and a directory cannot. So a file can only be copied up by the name it was
reached through — which is in hand at `open` and gone by the time a
descriptor is all that is left. Kernel overlayfs copies up at open for the
same reason.

The rows: `open(O_CREAT)`, `write`, `pwrite64`, `O_APPEND`, `O_TRUNC`,
`truncate`, `ftruncate`, `mkdir`, `mkdirat`, `rmdir`, `unlink`, `unlinkat`,
`symlink`, `symlinkat`, `rename`, `renameat`, `renameat2`, `utimensat`,
`chmod`, `fchmod`, `fchmodat`, `flock`, `fsync`, `fdatasync`, `link`,
`linkat`.

The upper layer holds real hard links, and `nlink` counts names — which is
what lets an unlinked file's bytes be given back when no name and no
descriptor reach it, at the `unlink` or at the last `close`. Without that a
container that churns temporary files holds every byte of them for as long
as it runs.

Acceptance, against the four bullets above:

- **The differential write corpus** is `tests/write_differential.rs`, running
  `tests/corpus/write.c` natively against a `/tmp` directory and under kisal
  against the overlay over a bake of that same directory, in all four build
  configurations. Create, write, `pwrite` past the end, both truncates,
  append semantics, `O_TRUNC`, copy-up of an image file, whiteout and
  recreate, the `mkdir`/`rmdir`/`unlink` refusals, symlink create and read,
  every rename case, and the merged listing with its `d_type`s — errno for
  errno and byte for byte.
- **The `EXDEV` directory rename** is asserted against *measured* kernel
  overlayfs behaviour rather than golden expectations. A Debian container's
  root filesystem is kernel overlayfs with the image as its lower layer, so
  `perl -e rename` inside one answers the question directly: a lower
  directory is `EXDEV`, an upper directory is fine, a lower file is fine, and
  a non-empty upper directory is fine.

  The first version of that measurement missed a case, and the review caught
  the gap: a lower directory that has been *copied up* — because something
  was created inside it — is still `EXDEV` on overlayfs, and this allowed it.
  The condition is whether the directory still reads through to the layer
  below, not whether it has been copied up. Measured and asserted.
- **The `.pyc`-shaped mtime round-trip** is in the differential: an explicit
  `utimensat` and the `stat` that reads it back, compared to the nanosecond,
  along with `UTIME_OMIT` and the `EINVAL` for a nanosecond field that is
  neither real nor special.
- **`/dev/urandom`** is tested natively and inside a container: the same seed
  replays byte for byte, a different seed diverges, and a container with no
  `/iso/random` mount is refused rather than answered with zeros.

**Not built, and deliberately**: `RENAME_NOREPLACE`, `RENAME_EXCHANGE` and `RENAME_WHITEOUT`, each
of which promises an atomicity this has not built; extended attributes on
copied-up files, which nothing can write and which the overlay reports as
absent rather than reading a stale reference. All three are named faults or
stated answers rather than silent approximations. `/proc/self/maps` waits for
M5's VMA tree, as the scope above says.

### M5 — Memory: brk, mmap, the VMA tree

The design doc's mmap section, implemented in its v0 shape — the
milestone is mechanical because the decisions are already made; the
work is the interval surgery and its tests.

Built: the boot carve (`__heap_base` → kisal heap, brk arena with its
`ENOMEM` ceiling, mmap arena above); the 4 K page fiction with
`EINVAL` on unaligned `munmap`/`mprotect`; chunked `memory.grow`; the
free pool with high-water-mark zeroing on reuse; the VMA tree with
`MAP_FIXED` atomic replacement, `MAP_FIXED_NOREPLACE`, hint honoring,
partial-unmap splitting; **eager-copy file mappings from both `/img`
and the overlay** (v0: no aliasing, per the design doc's demotion);
`mprotect` as record-only splitting; `MADV_DONTNEED` as real zeroing
and `MADV_FREE` as its eager implementation, everything else in the
`madvise` family recorded; `mremap` (in-place extend, `MAYMOVE`
allocate-copy-unmap); `brk`; `/proc/self/maps` rendered from the VMA
tree with backing names.

Acceptance:

- Differential mmap corpus vs. native: anonymous maps read as zeros,
  data survives round-trips, partial `munmap` splits observable
  through subsequent maps, `MAP_FIXED` replaces, hint behavior, the
  `EINVAL` alignment cases — errno-for-errno where semantics are
  guest-visible. (Addresses themselves are not compared; the *layout
  rules* are, via `/proc/self/maps` structure.)
- **The `MADV_DONTNEED` test is the star**: dirty a range, advise it,
  read zeros — the allocator contract that the design doc's
  correction exists to protect, plus the reuse path (map, dirty,
  unmap, re-map the same range, read zeros).
- The ld.so carving *shape* as a pure-VMA test — extent map then
  `MAP_FIXED` sub-maps with file offsets — against a baked fixture
  ELF's bytes, asserting the segment-copy rules, milestones ahead of any
  loader running: the dynamic tier inherits a tested substrate.
- An mmap-backed file read: map a blob file, compare bytes against
  `read(2)` of the same file, both against native.
- `/proc/self/maps` output round-trips through a corpus guest that
  reads it (the `pthread_getattr_np` consumer arrives in M7; the
  format is locked here).

Explicitly not in M5: aliasing (flagged optimization, designed,
off), writable `MAP_SHARED` (nothing creates one), guard-page
enforcement (nothing can).

#### Built — 2026-08-28

**The boot carve.** The arenas are cut from the top of whatever the module
already occupies — the linker's data, the shadow stack, and anything the
kernel's own allocator has taken. A `brk` arena with a configured ceiling
below, everything else above. Past the ceiling the break does not move and
the answer is where it still is, which is what glibc reads as "the heap is
full, use `mmap` from now on"; an errno there would diverge from a path
every libc takes.

**The 4 KiB page fiction**, in bookkeeping alone: lengths round up,
`munmap`/`mprotect`/`madvise` refuse an unaligned address, and the real
growth happens in amortised chunks when a reservation crosses the current
size. Wasm's own page is sixteen times larger and the guest never learns it.

**The free pool and the high-water mark.** Memory never shrinks, so `munmap`
returns ranges for address reuse rather than to the host — with the
obligation that a reused range handed out as fresh anonymous memory reads as
zeros. Freshly grown memory already does, so a high-water mark divides the
space and only a range below it is filled.

**The VMA tree**, and the interval surgery every row performs on it:
`MAP_FIXED` atomic replacement, `MAP_FIXED_NOREPLACE`, hint honouring,
partial-unmap splitting, `mprotect` as record-only splitting, `mremap`
in-place and `MAYMOVE`. Eager-copy file mappings from both the image and the
overlay, with the tail of the last page zero-filled. `MADV_DONTNEED` as real
zeroing and `MADV_FREE` as its eager implementation, everything else in the
family recorded. `/proc/self/maps` rendered from the tree, with the backing
file's name found by searching for it rather than kept at `open` — which
would put an allocation on a path the design promises is free of them.

Acceptance:

- **The differential mmap corpus** is `tests/memory_differential.rs`, in all
  four build configurations. Addresses are not compared — the two sides lay
  out their address spaces differently and always will — so what is compared
  is the rules: zeros in a fresh mapping, data surviving a round trip,
  partial unmap leaving what it did not cover, `MAP_FIXED` replacing,
  `MAP_FIXED_NOREPLACE` refusing, every alignment and argument refusal
  errno-for-errno, a file mapping against `read(2)` of the same file, and
  the loader carving sequence landing the right bytes.
- **`MADV_DONTNEED`** is tested both ways round: dirty a range, advise it,
  read zeros; and map, dirty, unmap, re-map the same range, read zeros. The
  second is the obligation an address space that never shrinks has to meet.
- **The loader carving shape** is a pure-VMA test against a baked fixture's
  bytes — extent map, then `MAP_FIXED` sub-maps at their own file offsets,
  with a deliberate wrong-offset case to show the check can fail. Several
  milestones before a loader exists to run it.
- **An mmap-backed file read** compares the mapping against `read(2)` of the
  same file, both against native.
- **`/proc/self/maps`** is read by a corpus guest inside a container, and
  the format is checked line by line: address order, `start-end perms offset
  dev:dev inode path`, and a line that brackets the address the guest was
  given, with the length and protection it asked for.

**Two things the differential settled that reading the manual did not.** A
page *entirely* past a file's end is not backed on Linux and touching one
raises `SIGBUS` — found by the native run taking the signal. Wasm has no
faults, so kisal answers zeros there and no implementation of it could do
otherwise; what the corpus checks instead is the zero-fill of the last,
partial page, which is the portable guarantee. And `mmap` *ignores* unknown
protection bits where `mprotect` refuses them — measured, `mmap` accepts
even `0x80000000` while `mprotect` answers `EINVAL` to `0x40`, and accepts
`PROT_SEM` while doing nothing with it.

Explicitly not in M5, as the scope above says: aliasing (designed, flagged,
off), writable `MAP_SHARED` — a named fault rather than a mapping whose
writes vanish — and guard-page enforcement, which nothing can do.

### M6 — Checkpoint: static CPython says hello

The heavyweight milestone, per the scope correction: the checkpoint
itself is cheap, but reaching it requires the linked-ELF front end and
kisal's exec path. Three parts, then the grind.

**The linked-ELF front end** — zaqaru learns to consume a complete
static executable. Code discovery from `.symtab` plus `.eh_frame`
FDEs, the entry point, and init arrays (M6 deliberately uses the
*unstripped* `python-build-standalone` artifact, which ships with
symbols; stripped-binary discovery is hardening for later, not a gate
here). Operands resolve to what they already are — concrete
addresses passing through with no symbolization — and the function
bodies emit through the existing machinery, guest convention and
`--resume` included (the container pipeline always builds with resume
on: site maps work identically in linked mode). Two pieces move
forward from the dynamic tier because any linked binary needs them:
the **static exec map** (vaddr → function-table slot, from the
discovered function list) and the **discriminating indirect call** —
in linked mode there are no relocations to turn function pointers
into table slots, so every function-pointer value is a vaddr and
every `call_indirect` goes through the map. CPython is function-
pointer-dense, so this lookup is on a hot path; v0 accepts that and
M11 measures it. The named front-end risk: jump-table recovery
currently leans on relocations, and linked mode must recover the same
dispatch shapes from absolute addresses — the one genuinely novel
analysis in this milestone.

**Kisal's exec path.** `PT_LOAD` segments copied from the blob to
their virtual addresses (a fixed-base region the mmap arena is carved
around — address-faithful by construction, since linear memory is
ours); the initial stack built with argv, envp, and a real auxv —
`AT_PHDR`/`AT_PHNUM` (musl's own TLS init reads the program headers
to find `PT_TLS`), `AT_PAGESZ`, `AT_RANDOM` from the CSPRNG,
`AT_SECURE=0`, and *no* `AT_SYSINFO_EHDR`, which is what makes the
clock syscalls arrive as syscalls; entry via its table slot.
`exit_group` lands here and **activates the EH shim for real** —
earlier than M7 expected: the syscall dispatcher returns
`Exit(code)`, the thunk throws, the boot-level catch receives it,
kisal writes the status into `/iso/shutdown/complete`, and returns to
the host. The first genuine throw in the system unwinds a real
CPython stack deep in transpiled frames, with nothing on
them the flush invariant does not cover.

**The baker completes.** It learns to drive the translator over the
image's ELF and to run the whole assembly: one `bake` invocation from
image to `container.wasm`, first time end to end.

**The grind.** The long tail, ground out against the real binary with
loud errors as the worklist: `uname` (fixed strings), `prlimit64`
(synthetic limits), `sched_getaffinity` (one CPU, honestly),
`getrandom`, `rseq` → real `ENOSYS`, `set_tid_address`/
`set_robust_list` (recorded), `clock_gettime`/`gettimeofday` via
`/iso/time`, `nanosleep` as the degenerate single-threaded case of
the blocking wait, `getpid`/`getuid` family (fixed),
`readv`/`writev`, and — recorded now, delivered in M10 —
`rt_sigaction`/`rt_sigprocmask`, because CPython installs its
handlers at startup and must not be refused. Plus the instruction
grind: whatever musl's SSE2 string functions and CPython's `-O2`
output demand from the design doc's gap list (`rol`, `bswap`,
`popcnt`…), surfaced one loud error at a time, exactly like the SSE
campaign.

Acceptance:

- The ladder: musl BusyBox `echo`/`cat`/`ls` first (breadth at low
  instruction diversity; no `sh` — it forks), then
  `python -c 'print("hello")'` — correct output, exit 0 through
  `/iso/shutdown/complete`.
- **The strace-diff harness graduates to the real oracle**: the same
  invocation strace'd natively and syscall-logged under kisal,
  sequences normalized (addresses and fd numbers abstracted, benign
  divergences — the vDSO-less clock calls — documented in the
  normalization, not ignored silently) and diffed.
- Determinism: two runs with the same seed produce bit-identical
  stdout and syscall logs — the record/replay property at real-binary
  scale.

### M7 — Threads: the scheduler on the resume machinery

The scheduler from the design doc's process-and-thread section,
mostly kisal-internal Rust — the logic host-testable before it ever
runs under emulation, which is what makes this milestone lower-risk
than its size suggests.

Built: the TCB (the `save_machine`/`load_machine` target from M1,
plus fs base, mask, `clear_child_tid`, run state); the run loop with
its catch, replacing M6's degenerate boot-level version; the
`Blocked`-return protocol wired end to end — Rust handlers return
`Blocked{reason, completion}`, the generated shim saves the machine
and throws, the catch schedules — so the rule that **wasm EH never
crosses Rust frames** is now load-bearing, not latent;
`clone3(CLONE_THREAD)` with the fabricated chain (clone-site resume
ID pushed on the caller-provided stack, `rax` = 0, `fs` from the
`tls` argument) and a loud error for unexpected clone flags
(`CLONE_VFORK` and friends are phase two); thread exit with the
`clear_child_tid` write and futex wake that `pthread_join` blocks on;
the futex rows — `WAIT`, `WAKE`, `WAIT_BITSET`, and
`REQUEUE`/`CMP_REQUEUE`, which condvar broadcast turns into and a
naive table forgets; the timer heap, with `nanosleep`/
`clock_nanosleep` moving onto it; and the single `/iso` blocking wait
in its real multi-thread form, with the `/pending` browser shape
present as an untested code path, per scope.

Acceptance:

- A pthread corpus, differential against native on *outputs* (never
  schedules): create/join, detached exit, mutex contention, condvar
  wait/signal/broadcast (the `REQUEUE` path observed, not assumed),
  and join-after-exit exercising `clear_child_tid`.
- Determinism as a feature test: the same corpus run twice under
  kisal produces identical interleaving-sensitive output — the
  countdown-free v0 scheduler switches only at syscalls, so runs must
  be bit-stable.
- A repeated-suspension test in the fork_resume style: a thread
  blocking deep under many frames, hundreds of block/resume cycles,
  results exact — the dispatcher-remainder machinery under sustained
  load rather than the single resume the existing tests prove.
- Native Rust unit tests for the run queue, timer heap, and futex
  buckets (including bitset masking) — scheduler logic proven before
  emulation touches it.
- **The integration star: CPython's `threading` module** — threads
  doing pure compute joined by the main thread, no sockets needed.
  The GIL is a futex workout, and this is the first time the resume
  machinery juggles multiple live chains under a real runtime.

### M8 — Readiness: epoll, poll, pipes, loopback

The design doc's readiness primitive and everything in-guest that
sits on it — no host edge yet: M8 is entirely RAM, which keeps its
differential tests hermetic.

Built: the waitable trait (`poll_mask()` + wait queue, woken on
transitions) and its implementors — `pipe2`, `socketpair`,
`eventfd2`, loopback TCP with the port table and the
connect/accept short-circuit, `AF_UNIX` stream with path binding
through the VFS (which makes the nscd-probe `ENOENT` behavior a test
rather than a hope); the half-close matrix (`SHUT_WR` →
drain-then-EOF at the peer, writes to a closed ring → `EPIPE` — with
the SIGPIPE *raise* recorded but deferred to M10, where delivery
exists); `FIONREAD`; epoll — `create1`/`ctl`/`wait`,
**OFD-registered interest**, level, edge (from kisal's cache
transitions), `EPOLLONESHOT` — plus `poll` and `select` over the same
scan; the socket rows (`recvfrom`/`sendto`/`recvmsg`/`sendmsg`,
`shutdown`, `getsockname`/`getpeername`,
`setsockopt`/`getsockopt` per the design doc's recording rules);
`SO_RCVTIMEO`/`SNDTIMEO` as parked waits racing the M7 timer heap;
`O_NONBLOCK` returning `EAGAIN` off readiness.

Acceptance:

- The half-close matrix as a table-driven differential corpus —
  every (`SHUT_RD`/`SHUT_WR`/`close`) × (read/write, drained/
  undrained) cell against native, errno-for-errno, for pipes,
  socketpairs, and loopback TCP alike.
- An epoll-semantics corpus against native: level re-arm behavior,
  edge single-shot on transition, `EPOLLONESHOT` re-arm, and **the
  dup footgun asserted** — a registered fd closed while a dup
  survives still fires, because interest lives on the OFD.
- Producer/consumer threads over a pipe and over loopback: M7's
  scheduler and M8's readiness composing, with blocking, `EAGAIN`,
  and timeout paths each forced at least once.
- `select` and `poll` agreeing with epoll on the same scenarios (three
  frontends, one scan — divergence means the scan lied to someone).
- **Integration: a Python echo pair** — client and server threads over
  in-guest loopback, `socket.settimeout()` exercised, one guest, no
  host edge. This is werkzeug's substrate minus the outside world.

### M9 — The edge: /iso/net broker

The first milestone where the host runner does real work: the
`/iso/net` broker paths from the design doc's mapping table,
implemented against real host sockets, and kisal's bridge from broker
handles to socket vnodes.

Host side: `listen` registration driven by the mount config's port
mappings (`host 8080 → guest 5057`, `docker -p` as data); accepted
host connections and completed outbound connects surfacing as events
through the single kernel wait; `conn/{j}` handles with `rx`/`tx`
data movement and `ctl` for shutdown/close; buffering with a cap so
tx backpressure is real. Kisal side: the readiness bridge updating
the guest-side cache and waking waiters **off cache transitions**
(the `EPOLLET` correctness rule from the design doc, now under an
edge that can actually race event batches); host-edge socket vnodes
indistinguishable from M8's loopback ones above the backend line;
nonblocking `connect` as `EINPROGRESS` → writability +
`SO_ERROR` on the completion event. Policy rows: an unmapped guest
listener is loopback-only (not an error — container semantics);
outbound `connect` with no `/iso/net` mount fails `ENETUNREACH` —
capability refusal wearing an honest errno.

Acceptance:

- Host `curl` reaches a guest C corpus server through a mapped port;
  the guest fetches from a host-local test server outbound;
  both directions concurrently.
- The no-mount refusal and the unmapped-listener loopback-only
  behavior asserted as tests — the capability model is mount config,
  so the config's two settings each have a test.
- Readiness-bridge edge cases forced: rx data arriving while the
  guest is mid-`epoll_wait`, tx backpressure producing `EAGAIN` then
  writability, a peer reset surfacing as `EPOLLHUP`/read-zero/`ECONNRESET`
  in the order native produces them (differential against a native
  socket pair driven the same way).
- **Integration: `python -m http.server` serves a host `curl`.** No
  Flask yet, no signals delivered (its handlers were recorded at M6) —
  the pre-Flask rung: a real Python server, a real HTTP exchange,
  through every layer except signal delivery.

### M10 — Signals

The design doc's Signals section, built on M7's machinery. The
dispositions and masks recorded since M6 become live.

Built: the pending sets and routing (thread-directed via `tgkill`,
process-directed to an unblocked thread, `kill` to self in-process —
cross-process waits for the router in phase two); delivery points
wired at syscall completion, the scheduler's resume step, and
wait-interruption; **chain surgery through `signal_dispatch`** — a
kisal Rust function with the resume-body signature at a reserved
table slot, the `rt_sigframe` built on the guest stack (or the
`sigaltstack` under `SA_ONSTACK`) with a faithful `ucontext` from the
TCB and the interrupted continuation's resume ID stored in the frame,
handler resolution through the exec-map discrimination, the
post-handler entry restoring the possibly-modified ucontext and mask
and yielding the continuation ID; `EINTR` and `SA_RESTART` in
Linux's order (completion written before surgery; restart as a
kernel-internal re-issue from the saved arguments); the synchronous
SIGPIPE raise turning on at M8's recorded spot, with default-death
exit status `128+SIGPIPE`; `setitimer(ITIMER_REAL)` on the timer
heap; default dispositions (terminate → status through
`/iso/shutdown/complete`, SIGCHLD ignored); and
`/iso/shutdown/requested` polled at the kernel wait and synthesized
into a process-directed SIGTERM.

Acceptance:

- The handler corpus, differential against native: flag-setting
  handler; a handler that *blocks* (writes a full pipe — the chain
  parked mid-handler and resumed, the design's centerpiece exercised
  directly); nested delivery (a handler's syscall interrupted by a
  second signal); `EINTR` on a blocked read interrupted by `SIGALRM`;
  the same with `SA_RESTART` resuming the read; `sigaltstack`
  delivery; SIGPIPE default death with the shell-visible `141` exit
  status.
- Mask semantics: `sa_mask` and `SA_NODEFER` observed through
  nested-delivery attempts, errno-and-order against native.
- **Integration, twice.** CPython's `set_wakeup_fd` path: a
  synthesized SIGINT becomes `KeyboardInterrupt` in a running script —
  the C handler's pipe write from handler context, end to end. And
  the shutdown path: `/iso/shutdown/requested` → SIGTERM → a Python
  `signal.signal(SIGTERM, …)` handler runs → clean exit with the
  handler's chosen status in `shutdown/complete`. That second test is
  M11's shutdown acceptance, landed one milestone early.

### M11 — Target: Flask, served

The target, and deliberately the *smallest* milestone: if M11 is
where problems surface, an earlier milestone's acceptance was too
weak, and the fix belongs there.

**The image.** A rootfs assembled from the musl-static CPython plus a
pip-installed Flask environment — Flask, werkzeug, jinja2, and
friends are pure Python, so the install is files, not native code
(the AOT deal from the design doc, demonstrated in the pleasant
direction). `PYTHONUNBUFFERED=1` per M4's not-a-tty decision. One
`bake` produces `container.wasm`.

**The run.** The host runner with a port mapping: `curl` returns
`Hello, World!`; N parallel `curl`s all return 200 through
werkzeug's thread-per-connection on kisal's green threads;
`/iso/shutdown/requested` → SIGTERM → werkzeug's shutdown → status
in `/iso/shutdown/complete`.

Acceptance:

- The responses, exactly.
- **The strace-diff against the original baseline** — the trace that
  produced the design doc's empirical section, normalized per the
  M6 rules, compared against the kisal run's syscall log. Divergences
  are each classified: benign-and-documented (the vDSO-less clocks,
  the loopback short-circuit's invisible internals) or a bug.
- Record/replay at full scale: a session's `/iso` boundary taped
  (connections, clock, seed), the tape replayed as a mount, the run
  bit-identical — the two-function nondeterminism inventory cashed
  in.
- **The first performance numbers, measured and recorded, not
  acted on**: requests/second and latency vs. the same app native;
  startup time; the syscall-path microcosts (clock read, stat,
  loopback round-trip) — closing the working-first loop and handing
  phase two its priority list with data instead of guesses.

## Testing discipline

Four layers, cheapest first, every one permanent once landed:

1. **Native kisal unit tests** — the index parser, vnode walk,
   overlay, VMA surgery, run queue, timer heap, futex buckets, epoll
   scan: ordinary `cargo test` on the host, milliseconds, the
   inner-loop falsifier for all kernel logic. Anything testable here
   is tested here first; emulation is for what only emulation can
   check.
2. **Differential corpus guests** — the house backbone extended:
   small C or assembly programs run natively and under kisal,
   syscall-visible behavior compared errno-for-errno and
   byte-for-byte. Where a fixture exists on both sides (a directory
   tree and its bake, a socket pair and its rings), the native run
   *is* the oracle; nothing is asserted from memory of what Linux
   does.
3. **The strace-diff harness** — the integration oracle from M6 on:
   the same invocation strace'd natively and syscall-logged under
   kisal, sequences normalized and diffed. The normalization rules
   are a versioned artifact, not tribal knowledge: every abstraction
   (addresses, fd numbers) and every tolerated divergence (the
   vDSO-less clocks) is written in the harness with the reason
   attached. An undocumented divergence is a failure by definition.
4. **Determinism tests at every level** — same seed, bit-identical
   run — because the scheduler and CSPRNG designs promise it, and a
   promise without a test decays.

One standing policy, the **loud-error policy**: an unimplemented
syscall, instruction, clone flag, or ioctl is a named error carrying
the identity of what was missing — never a silent `ENOSYS` — except
where `ENOSYS` is itself the Linux-conformant answer (`rseq`), which
is then a deliberate row, with a test.

Separately, and as a technique rather than a policy: a harness that
claims to detect a class of failure is worth more when something has
shown it can fail. `tests/call_boundary_state.rs` has negative
controls, the strace normalizer is checked against an injected
divergence, and the counting allocator against a planted allocation.
Where that has been done it is recorded with the milestone; it is not
a bar every test has to clear.

## Repository and crate layout

This repo becomes a small workspace:

- **`src/` (zaqaru)** — the translator extensions land in the
  existing crate: the `syscall` rewrite, the generated syscall thunk
  and machine helpers, EH opcode emission, `%fs` translation, and the
  linked-ELF front end. All of it ships with corpus coverage in the
  existing differential suite, same as every prior campaign.
- **`kisal/`** — the kernel crate: builds natively for its unit tests
  and as a `wasm32-unknown-unknown` staticlib for the weld. **Deviation,
  taken at M1 and not recorded until M3's review found it:** its
  `[dependencies]` is empty. The plan said it would depend on the structfs
  crates by path, and M0 gate 1 exists to establish that they
  cross-compile unchanged so the kernel's downward face could be the real
  `LLReader`/`LLWriter` "rather than a re-implementation". A
  re-implementation is what shipped — `kisal::abi` hand-writes the
  canonical ABI lowering of the two `ll-store` imports. The reason is that
  the guest-side face is two imported functions and two return areas,
  which is less code than the dependency's own glue would be, and the
  kernel has to be `no_std`-clean for the wasm staticlib. The cost is that
  the lowering is written twice, once here and once in the runner, and a
  disagreement between them is a silent wrong answer — which is exactly
  the defect M1's review found in the `option` discriminant. The
  layout is now pinned by tests on both sides. Revisit when structfs cuts
  releases.
- **`baker/`** — the bake tool: OCI flatten, index emission,
  translator driving, the final link. Owns the fixture-bake test
  support.
- **`runner/`** — the wasmtime host: the two ll-store imports, the
  mount table and its config, the net broker, the console, the event
  wait. Starts as test support in M1 and graduates to a binary.

What goes to the **structfs repo** instead: the isotope spec
extensions from the design doc's mapping table (ns-typed time paths,
the kernel-events path with a deadline, `/iso/net`, console paths,
exit status in `shutdown/complete`, `/iso/proc/spawn`), and the
core-wasm lowering of the ll-store ABI from M0 — interface
agreements, proposed there, consumed here. Wrapping the runner's core
module as a featherweight Block is phase-two work that the M0 ABI
alignment keeps mechanical.

## Risks and unknowns

Ranked by which milestone hits them first, each with where its answer
comes from:

- **M0**: the five gates themselves — each is a risk converted into a
  spike with a reroute. The wasmtime-EH gate is the one most likely to
  say "not yet" (engine support is recent and flag-gated); its reroute
  is fully designed.
- **M6, the concentration of unknowns**: jump-table recovery without
  relocations (the one novel analysis — if it resists, the dispatcher
  mode is the always-correct fallback for affected functions at a
  measured cost); the volume of instruction gaps musl and CPython
  surface (bounded by the loud-error worklist — the risk is schedule,
  never correctness); symtab-dependent code discovery (mitigated by
  choosing the unstripped artifact; stripped hardening deferred).
- **M6, first real throw**: EH behavior unwinding thousands of
  transpiled frames — gate 4 proves the mechanism, M6 proves it at
  depth.
- **M7 onward**: dispatcher-mode cost at eval-loop block counts — the
  design doc's 1–2% is from small kernels; M7's sustained-suspension
  test gives the early signal, M11 the real number. If it is bad, the
  selective structured-continuation mitigation is already designed.
- **M11**: the `/iso` hot-path cost (clock, boundary copies) —
  deferred by doctrine, measured here; the levers (cached coarse
  clock, rings behind paths) are named in the design doc.
- **Standing**: structfs/isotope interface churn — we own both sides,
  so this is coordination, not risk, but the mapping table is the
  contract and drift between the repos is caught by the M0 ABI
  document, not by debugging.

## Wall-clock and iteration discipline

The rules that keep half-days from becoming weeks, stated once and
binding:

- **Cheapest falsifier first, always.** The inner loop of any change
  is the narrowest test that can prove it wrong: a kisal unit test
  (milliseconds), one corpus guest (seconds). The full differential
  suite, the strace-diff, and real-image bakes run at milestone gates
  and after refactors that touch shared machinery — never after every
  edit.
- **Fixture bakes stay small.** M3–M5 test images are trees of
  dozens of files baked in milliseconds; the real CPython image is
  baked at M6+ gates and cached between runs. A full bake in an inner
  loop is a process failure, not a cost of doing business.
- **Timeouts are sized to measured durations** (2–3× observed), never
  round ceilings. The first run of anything new measures; the second
  run has a justified timeout.
- **Long runs are declared before they start** — expected wall time
  and what a shorter run cannot answer — and anything over ~2 minutes
  runs in the background with its cost stated.
- **A milestone is done when**: its acceptance list is green, its
  tests are landed in the permanent suite, its verdicts and
  deviations are folded back into this plan and the design doc (the
  design doc wins conflicts, so a deviation discovered while building
  is a design-doc edit first), and the loud-error worklist it
  generated is either empty or explicitly moved to a later
  milestone's text. "Works on my machine today" closes nothing.

---

## Appendix — M6 progress (appended 2026-08-28)

Appended rather than folded into M6's section above, so that what the
plan said before the work and what the work found stay separately
readable.

### Built and green

**The linked-ELF front end**, complete:

- Code discovery from `.symtab` *and* `.eh_frame` FDEs, with the
  stripped case included rather than deferred — reading the CIE
  augmentation properly to find the pointer encoding gave it for free.
  A zero-size symbol takes its extent from its unwind entry; an extent
  no symbol covers becomes a function named after its address.
- The entry point, `PT_LOAD` segments, and `section_at`.
- Operands resolving to what they already are, with one correction the
  plan did not anticipate: a program-counter-relative operand resolves
  to a *section offset*, because that is what the decoder's program
  counter is, and the address is that offset plus where the loader put
  the section. Call targets need no change at all.
- The static exec map and the discriminating indirect call.
- Jump-table recovery from absolute addresses — the milestone's named
  novel analysis. It works, and it exposed a second half the plan did
  not name: the recovered table's entries have to be *rewritten*, and
  in linked mode those bytes reach the guest through the image rather
  than through a module data segment. They travel as image patches the
  bake applies (`baker::program`).
- A linked input contributes no module data segments at all. The loader
  places `PT_LOAD`; a second copy at a `wasm-ld`-chosen address would
  leave every operand pointing at the first one.

**Kisal's exec path**: program headers parsed, segments placed, the
initial stack built with argv, envp and the auxv the plan specifies
(`AT_PHDR` from `PT_PHDR` or the covering segment and refused if
neither, `AT_PHNUM`, `AT_PAGESZ`, `AT_ENTRY`, `AT_BASE` = 0,
`AT_RANDOM` from the seeded CSPRNG, `AT_SECURE` = 0, and no
`AT_SYSINFO_EHDR`), entry through its table slot, `exit_group` through
the EH shim to a boot-level catch and out to `/iso/shutdown/complete`.

**One correction to the address-space design.** The design doc's
"the main binary is never loaded" holds for the relocatable tier only.
A linked program is loaded, at its own low virtual addresses, and the
module's data — the image blob first — occupies those addresses by
default. Carving the arenas around the region cannot fix it, because the
region is below the module's data rather than above it. So the bake
places the module's data above the program (`baker::layout`,
`--global-base`), and kisal checks the result against `__global_base`
at load. This is a change to the plan's assumptions, not to its
milestones.

### Not yet built

- The acceptance ladder. `tests/boot.rs` runs a *corpus* program end to
  end — a real one, with a hand-written `_start`, argv and auxv read off
  the stack, and an exit status out through the store — but not musl
  BusyBox and not CPython.
- The baker driving the translator over an image's ELFs. The two pieces
  that pass needs (`baker::layout`, `baker::program`) exist and are
  tested; nothing yet calls them from a `bake` invocation.
- The grind: `uname`, `prlimit64`, `sched_getaffinity`, `getrandom`,
  `rseq` → `ENOSYS`, `set_tid_address`/`set_robust_list`,
  `clock_gettime`/`gettimeofday`, `nanosleep`, the `getpid`/`getuid`
  family, `readv`/`writev`, and the recorded `rt_sigaction`/
  `rt_sigprocmask`. Plus the instruction gap list.
- The strace-diff harness as the real oracle, and the determinism check.

### What running it end to end found

Two defects that no narrower test could have reached, both fixed:

1. **The exec map wrote table slots as constants.** Objects number their
   own table entries from one and the linker renumbers them on merge, so
   the constants named whichever object won that number — the seam,
   whose `kisal_yield` holds the first slot. Entering the program threw
   instead of running it. The slots are relocations now.
2. **Nothing applied the image patches.** The guest ran correctly until
   its first `switch` and then dispatched through an unrewritten table.

And one obligation the design had named that nothing was meeting: the
catch has to restore the shadow-stack pointer, because the seam's own
restore never runs for a syscall that leaves. Harmless at M6, which
throws once; it would have bitten M7 on the second thread.

## Appendix — x87: soft emulation (appended 2026-08-28)

The design lives in container-plan.md's "x87 and MMX" subsystem
section; this is the build sequence. It sits inside M6's grind — `fld`
and friends are in the refusal tail of the static binaries the grind
runs on — but it is scoped to the full instruction set, because the
target is any binary; the grind decides order, not scope.

**The deliverable is the `x87/` workspace crate** plus, in a second
step, the translator lowering (`src/translate/x87.rs`, mirroring
`vector.rs`'s outcome shape) and the symbol plumbing beside
`syscall_entry`. Crate layout:

- `f80.rs` — the representation, packing, classification.
- `arith.rs` — extF80 add/sub/mul/div/sqrt, `fprem`/`fprem1` with the
  partial-result protocol, round-to-int; RC and PC honored, C1 =
  rounded-up, sticky exception flags returned.
- `convert.rs` — f32/f64/i16/i32/i64, both directions.
- `compare.rs` — `fcom`/`fucom` families, `fxam`, `ftst`.
- `state.rs` — `X87State`: TOP, tags, FSW/FCW, stack faults with
  masked responses, the env and fnsave images.
- `ops.rs` — instruction-level semantics on `X87State`; what the
  helpers call.
- `transcendental.rs` — `fscale`/`fxtract` exact; `f2xm1`/`fyl2x`/
  `fyl2xp1`/`fpatan` f64-backed v1 (via the `libm` crate — pure-Rust
  musl algorithms; wasm32-unknown-unknown has no libm to link).
- `ffi.rs` — wasm32-only: the static state and the `x87_*` symbols,
  including `x87_save`/`x87_load`/`x87_image_size`/`x87_reset`.
- The tier table, in-crate: per op, bit-exact / correctly-rounded /
  f64-backed / not-yet — the tracker full emulation drives to done.

**Testing.** Three layers, native-first like kisal's:

1. Unit tests per module: exact cases, specials, the masked-response
   matrix, env-image byte layouts.
2. The host-FPU oracle (`x86_64` native only): property tests pushing
   random 80-bit patterns through `asm!`-wrapped hardware instructions
   and demanding bit-identical results and matching FSW exception
   bits — every data-path op, all four RC values, both PC settings,
   biased generation for denormals, zeros, infinities, NaN payloads.
   Transcendentals compare in ulps with exact-case assertions, since
   Intel and AMD disagree in the low bits and bit-matching is
   incoherent. Constants (`fldpi` et al.) verify against the host in
   every rounding mode — the internal values are more than 64 bits
   wide and RC-sensitive, which the oracle catches for free.
   Iteration counts sized to keep the whole oracle suite in seconds,
   measured, per the wall-clock discipline.
3. Corpus differentials, once the lowering exists: a `long_double.c`
   (parse/format/arithmetic/casts/fenv shapes), an `fprem` asm corpus
   file mirroring musl's `fmodl` loop since no compiler emits `fprem`
   from C, and the fenv shapes the hello histogram says are live.

**Order of work.** The extF80 core with the oracle first — it is the
risk. Then state machinery, loads/stores/converts/compares; then the
lowering and the corpus differentials; then env images; then the
exact "transcendentals" and the f64-backed four. Gate: the x87 rows
leave the grind's refusal tail, and a static binary doing
`strtold`/`printf("%.21Lg")` round-trips runs differentially clean.

**Named integration points, so they are not rediscovered:** M7's
context switch calls `x87_save`/`x87_load` beside
`x86_save_machine`/`x86_load_machine`; M10's sigframe uses
`x87_render_fxsave`/`x87_load_fxsave`; `execve` calls `x87_reset()`.
Later rows on the full-coverage list, ordered by evidence, none
deniable by appeal to a workload: `ffreep` (glibc emits it) in the
live set; `fsin`/`fcos`/`fsincos`/`fptan` at the transcendental
target; MMX (guaranteed by x86-64 CPUID, cannot be curated away);
`fxsave`/`fxrstor` on the sigframe render; unmasked-exception
delivery through kisal's signal machinery, deferred-reported at
helper entry, which is faithful because hardware defers too.

## Appendix — the dynamic tier (appended 2026-08-29)

Listed above as phase two and built early, for the reason recorded there.
What it delivers: `gcc -O2 hello.c` — a position-independent executable,
`ld.so` and `libc.so.6` — bakes into one container and runs, with the loader
executing as ordinary translated guest code. `tests/dynamic_boot.rs` is the
gate, its widest case a program that uses the library rather than calling
into it once: `qsort` through a callback from the executable, an allocator,
formatted output, a libm call, and a file written and read back, all
byte-identical to the same binary run natively.

Built as `container-plan.md` designs it, with one correction to that
document (folded in there): **the shadow GOT is an optimisation, not a
prerequisite.** Its generic fallback is the discriminating indirect call the
linked-ELF front end already has, so a cross-DSO call resolves through the
exec map with nothing added.

The pieces, and where they live:

- `baker::dynamic` — `PT_INTERP` and `DT_NEEDED` resolved against the tree
  the image is made from, transitively. Not `/etc/ld.so.cache`: it names
  files the search path also names, and reading it would make a bake depend
  on the host's cache being right about the host's filesystem.
- `baker::layout::DYNAMIC_BASE` — where bases are assigned, high enough that
  a library's text does not sit among its own integer constants. See
  `code-discovery.md`, which carries the measurement.
- `ObjectFile::parse_at` and `merge` — one translation unit for every file,
  because the exec map has to span them.
- `kisal::exec` — the program and its interpreter both loaded, an auxv
  saying where each went, control to the loader.
- `kisal`'s `mmap` — a translated ELF is mapped at its prelink base, read
  from a new index region. Mapping an *untranslated* file executable is the
  loud error the design names.

**Not built, and not blocking:** the shadow GOT; `dlopen` (untried; baked
libraries should work by the same path); `/etc/ld.so.cache` regeneration
(nothing needed it, since the loader finds the files at the paths the bake
placed them). **Not measured:** breadth. Three dynamic files read closely
and fifteen more through a parse spike is not a population.
