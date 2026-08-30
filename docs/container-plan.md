# Running a container image: the syscall layer

Status: **draft** — under active discussion; sections will grow as designs
are settled. Nothing here is built unless it names the test that proves it.

## The goal and the bet

Take an OCI container image — a real one, unmodified binaries, static or
dynamic — and run its process tree under wasm, with zaqaru as the
translation layer and a Linux-personality kernel supplied *in the guest*.

The bet, in one sentence: **`syscall` is rewritten into an ordinary typed
wasm call at the seam the thunk generator already owns, and the kernel is
just more code linked into the module.** The kernel is named **kisal**
(Akkadian *kisallu*, the temple courtyard — the enclosed ground
everything in the house crosses to get anywhere), keeping the Akkadian
register of *zaqāru*. The consequences that make the rest of this
document possible:

- A syscall costs a wasm call, not a trap or a context switch. The
  transition overhead that dominates emulator designs is gone.
- The kernel is testable natively, linkable by `wasm-ld`, and interposable
  at link time — a wrong signature at the seam is a link error.
- The flush discipline means that at every syscall the complete machine
  state is in the register globals and linear memory. That is not a hope;
  it is tested (`tests/call_boundary_state.rs`, adversarial state swap with
  negative controls). Everything below leans on it.
- With `--resume` on, the guest stack is a serialization of the suspended
  frames (`tests/fork_resume.rs` proves a checkpoint resumes in a fresh
  instance without re-execution). Blocking, scheduling, fork and
  checkpointing are all the same mechanism.

The cost, named once and accepted: **no isolation between guest and kernel,
or between guest threads.** Everything shares one linear memory and wasm
has no faults to hook. `mprotect` is bookkeeping; guard pages and RELRO are
silent. This is a different threat model from a trapping sandbox (gVisor
traps *because* it distrusts the guest). Here the sandbox boundary is the
wasm module boundary, and everything inside it is one trust domain.

## What is already built

- **Checkpoint state**: at any call boundary, machine state is entirely in
  globals + linear memory — verified across both compilers, `-O0`–`-Os`,
  both code models, both control-flow translations.
- **`--resume`**: call sites store resume IDs in the return-address slots
  they already reserve; every function gets a resume body (the dispatcher
  over the call-split graph, entered at any post-call block, with an
  epilogue arm for tail-call sites); a weak `x86_resume` driver walks the
  chain. Fork is demonstrated end to end: snapshot a parent at a call,
  copy memory + globals into a fresh instance, set `rax` to the child's
  reply, resume; the child matches a full-run oracle and provably never
  re-executes the prefix. Ordinary output is byte-identical with the flag
  off.

## The empirical baseline: what a real app actually calls

Rather than design against the whole syscall table, we traced the target:
a Flask hello-world (CPython 3.11, glibc, dynamic) from interpreter start
through serving three HTTP requests to shutdown, under `strace -f`.

**4,742 syscalls in the CPython process tree; ~55 distinct.** (gVisor
implements ~211. A hello-world web app needs a quarter of that.)

Reproduce: a PEP-723 script with `flask` as its dependency, `app.run()` on
localhost; `strace -f -o trace.txt uv run --script app.py`, then curl it.
Slice the trace to the python PIDs — `uv`'s own syscalls are not the
guest's.

### The distribution

- **~80% is filesystem**: `stat` 1,226, `read` 587, `fstat` 626, `lseek`
  558, `openat` 382, `close` 359, `getdents64` 92 — CPython crawling its
  module path for `.pyc` files — plus 289 `ioctl`s that are all `TCGETS`
  isatty probes. All of it servable from an in-guest VFS with zero host
  crossings.
- **Memory**: `mmap` 100, `brk` 62, `mprotect` 20, `munmap` 13. Four mmap
  flavors only: anonymous private RW (allocators), anonymous `MAP_STACK`
  (thread stacks), file-backed `PROT_READ` private (`locale-archive`,
  5.7 MB; two read-only `MAP_SHARED` of `gconv-modules.cache`), and 13
  file-backed `PROT_EXEC` maps — the dynamic loader mapping libpython and
  extension `.so`s. **Nothing creates writable-then-executable memory.**
  Every executable byte exists in an image file at build time; AOT
  translation of this workload is closed.
- **Concurrency is threads, not fork.** `clone3` with
  `CLONE_VM|CLONE_THREAD` — werkzeug spawns a thread per connection.
  `futex` 41 (`WAIT_BITSET`±`CLOCK_REALTIME`, `WAIT_PRIVATE`,
  `WAKE_PRIVATE`×37). `fork` never appears.
- **Network**: `socket`/`bind`/`listen`/`accept4`/`recvfrom`/`sendto`/
  `shutdown`/`getsockname`/`setsockopt`, `epoll_create1`/`ctl`/`wait` per
  connection thread, `poll` on the listener, `FIONBIO`. Werkzeug's
  shutdown plumbing is `pipe2`/`eventfd2`/`socketpair` — in-guest objects.
- **Signals**: `rt_sigaction` 131 (63 are glibc probing SIGRT numbers) —
  table writes. Delivery only matters for SIGINT/SIGTERM here.
- **Identity/misc**: `getpid`, `gettid`, `uname`, `getcwd`, `getrandom`,
  `prlimit64`, `sched_getaffinity`, `set_robust_list`, `rseq`,
  `set_tid_address`, `fcntl`, `dup2` — mostly recordable no-ops or
  constants; `rseq` can honestly return `ENOSYS` (glibc falls back).
  `madvise` (`MADV_DONTNEED` ×7, `MADV_FREE` ×1) is *not* in this
  bucket — see the mmap section; `DONTNEED` has visible zeroing
  semantics that allocators rely on.

### Invisible in the trace but mandatory

- **The clock.** `clock_gettime` never appears because glibc uses the
  vDSO. We control the auxv: omit `AT_SYSINFO_EHDR` and glibc falls back
  to the real syscall path. The vDSO problem dissolves into an ordinary
  syscall to implement.
- **TLS.** `arch_prctl(ARCH_SET_FS)` ×3. Every glibc access to
  `%fs:0x28` and CPython's thread state is fs-relative addressing, which
  the translator currently does not handle at all (the corpus compiles
  with `-fno-stack-protector` precisely to sidestep it). Settled design:
  `x86_fs_base` is a seventeenth register — an `i64` global, eligible for
  promotion, saved and restored through the TCB at context switches. An
  fs-prefixed memory operand adds the base into the effective address;
  one extra add on those instructions, nothing anywhere else. All three
  writers arrive at flush boundaries by construction (`arch_prctl` is a
  syscall, `CLONE_SETTLS` lands in the child's TCB before it runs, the
  scheduler swap is between threads), so no new discipline is needed.
  glibc's init then works unmodified — it allocates the pthread struct,
  points fs at it, and writes its own canary. `%gs` stays a loud
  translation error until something real needs it. Mechanically small;
  blocks everything glibc, so it is early in the order.

### Notable absences

No `setitimer`/`timer_create` (itimers matter for other workloads, not
this one), no `mremap`, no `msync`, no `io_uring`, no writable
`MAP_SHARED`, no self-modifying or JIT-generated code.

## The host boundary: structfs

**Syscalls do not bubble up — resources do.** Every syscall is handled
in-guest; what varies is the backend behind the object it touches.
`read(2)` is a pure guest operation on a `.pyc` file and a
host-crossing one on an external socket. The host-backed backends are
exactly four: time, entropy, external I/O edges, and instance creation.
Everything whose state the kernel owns — files, pipes, memory, threads,
futexes, signals, process identity — never crosses.

**Decision: the entire host interface is structfs**
(`~/git/structfs`) — the boundary is the `ll-store` pair, exactly as
featherweight's WIT already defines it and promises never to change:

```
ll_read(path: list<list<u8>>)         -> option<list<u8>>
ll_write(path: list<list<u8>>, bytes) -> result-path
```

Two byte-typed imports, path-routed, instead of a dozen typed ones. The
guest kernel keeps its POSIX face upward and speaks one
`LLReader`/`LLWriter` pair downward; structurally it *is* a
featherweight Block (import ll-store, export run). What the typed
imports had — per-function link-time type checking — moves into the
mount table; what the two-import boundary buys is that **the ABI never
changes again**: a new capability is a new mount, not a new import.
This is the interface of the final form, or close to it; performance
concerns are deliberately deferred — get everything working first, then
benchmark (candidates for attention are listed at the end).

The host services live under `/iso/`, per the isotope spec
(`~/git/structfs/isotope/spec/`). The mapping, with a column
distinguishing what the spec defines today from what we would propose
as extensions — this table doubles as the list of asks against isotope:

| kernel need | path | isotope status |
|---|---|---|
| `clock_gettime` (vDSO omitted from auxv) | `/iso/time/now`, `/iso/time/monotonic` | in spec (ISO 8601 / counter); **extension**: ns-typed `monotonic_ns`, `realtime_ns` |
| entropy (boot-seed an in-guest CSPRNG) | `/iso/random/bytes/{n}` | in spec |
| the single blocking wait | blocking read on an events path | **shape in spec** — `/iso/server/requests` blocks, `…/pending` polls; **extension**: a kernel-events path with a deadline |
| browser-idle poll variant | non-blocking `…/pending` read | in spec |
| external sockets | broker/handle pattern: write `/iso/net/connect` → handle `/iso/net/conn/{k}`; `…/rx`, `…/tx` | **extension** (pattern precedented by the structfs HTTP broker) |
| guest stdio | `/iso/console/{stdin,stdout,stderr}` | **extension** |
| kernel diagnostics | `/iso/log/{debug,info,warn,error}` | in spec |
| `docker stop` → SIGTERM to init | read `/iso/shutdown/requested` | in spec |
| process exit | write `/iso/shutdown/complete` | in spec; **extension**: carry the exit status in the payload |
| fork / spawn | write `/iso/proc/spawn` — payload is parameters only; the host embedder owns the calling instance and snapshots memory + globals itself | **extension** |

The blocking semantics fit is exact, not approximate. Isotope specs
both a blocking read ("the runtime suspends that Block until the
operation completes", 06-protocol) and a non-blocking `/pending`
variant — which are precisely the two portable shapes of the single
blocking point: natively, the kernel issues the blocking read only when
*no* thread is runnable, so blocking the one wasm thread is correct by
design; in a browser, the kernel takes the `/pending` path and returns
from `run()` idle, and nothing ever blocks the Worker.

Three properties get stronger through this boundary:

- **The nondeterminism inventory is two functions.** Time, entropy,
  external I/O, spawn — all of it enters through `ll_read`/`ll_write`
  on `/iso/`. Checkpoints are consistent by construction.
- **Record/replay is a store transformation.** Interpose a recording
  store on the host side and the tape of a run *is* a structfs store;
  replay is mounting the tape where the live host was. With the
  deterministic countdown scheduler, container execution is fully
  reproducible, and the tape is inspectable with existing structfs
  tooling (the REPL).
- **Capability control is mount configuration.** What a container may
  touch is exactly what is mounted under its `/iso/` — no net mount, no
  network — decided in data, not code. Isotope's namespace model does
  the seccomp-profile job for free.

Known costs, deferred deliberately until it all works: per-call path
routing and a `Bytes` allocation on a hot path like the clock (static
path constants, cheap prefix routing, and a scheduler-tick-cached
coarse clock are the mitigations to measure); send/recv copies at call
granularity (the ring-buffer escape hatch now lives *behind a path* — a
store whose contract is a shared ring — so the interface survives a
payload-path change); and one integration note: zaqaru emits core wasm
modules while featherweight hosts components — a mechanical wrapping,
not a design conflict, but real work.

## The process and thread model

### Processes: one instance each, kernel replicated

**Process = wasm instance. Thread = a control block inside one.** The
alternatives die quickly and the reasons are worth recording. A kernel
in its own instance serving process instances cannot share data
structures across memories, so every syscall becomes a host-mediated
copy — which forfeits the syscall-as-wasm-call bet. Many processes in
one memory cannot fork, because guest pointers are absolute linear
offsets and the child needs the parent's addresses. So each process
instance carries its own copy of the kernel — code *and* state. The
replication is a feature at fork time: the fd table, VMA tree and
signal tables are snapshotted with everything else, so inheritance is
POSIX-correct by construction. And the target workload is one process:
the whole cross-process layer defers until fork or exec actually runs.

Cross-process edges go through a deliberately dumb host-side **router**:
pid allocation, exit/wait notification (child exit → event → parent's
`wait4`/`SIGCHLD`), cross-process `kill` routing, and object transport.
It holds no POSIX semantics; those stay in the kernels.

The process operations:

- **`fork`** — the built machinery: `write /iso/proc/spawn`. Only the
  calling thread survives in the child (POSIX); the child kernel drops
  the other TCBs, which is a table cleanup here rather than the torment
  it is for pthread implementations.
- **`vfork`/`posix_spawn`** — what real code overwhelmingly does (the
  trace's only fork-shaped call was a `CLONE_VFORK`): a fast path with
  no snapshot at all. Parent suspends; the child is instantiated fresh
  from the exec'd binary's pre-transpiled module with fd dispositions
  applied. Exact vfork semantics for free.
- **`execve`** — in a closed image: instantiate pre-transpiled module X
  and build the initial stack (argv, envp, auxv — where the vDSO is
  omitted and `AT_RANDOM` planted).
- **The fork tax**: POSIX shares open-file-description state across
  fork, and pipes must actually connect — resolved by fd hoisting,
  next section.

### Fork's fd hoisting

Fork's copy semantics and POSIX's sharing semantics point in opposite
directions. Fork copies the instance, and kisal's state is *in* the
copied memory — so the child inherits a **copy of every kernel object
behind its fds**: rings, offsets, accept queues. For most state that
is exactly right (it is why kernel-per-instance makes fork clean). But
POSIX says fork'd fds share the *open file description*, and an OFD is
live cross-process state: a pipe's ring (parent writes, child reads —
private copies silently break the fundamental shell pattern), a file
offset (`{ read a; cat; } < file` — even a read-only image file has
this one cell of shared mutable state), a listening socket (prefork
workers all `accept` the same listener), a connected
socket/socketpair/eventfd. After the snapshot there is no coordination
channel between the copies, so anything genuinely shared must move
*out of both of them* before the copy.

**Hoisting = at fork time, migrate an object's authoritative state to
a router-owned object, leave a proxy vnode behind, and take the
snapshot after.** For a pipe: create the router object
(`write /iso/proc/objects {"type": "pipe", …}` → handle), transfer the
ring's unread bytes, swap the local vnode's backend to a broker
proxy — **which is the host-edge socket backend, reused wholesale**
(`…/rx`, `…/tx`, readiness via the kernel wait): hoisted objects are
indistinguishable from `/iso/net` connections, no second plumbing.
Then snapshot: the child is born holding the proxy. The router
refcounts *ends* — reader refs and writer refs across processes —
because EOF means all writer ends everywhere closed, and SIGPIPE means
all reader ends are. The ordering is load-bearing: hoist, then
snapshot.

Hoisting is type-graduated — each object pays for what is actually
shared:

- **Byte-stream objects** (pipes, socketpairs, connected sockets,
  eventfds): hoist fully; content is actively shared.
- **Listening sockets**: hoist — and this is the moment the **port
  table's authority moves to the router**, since the netns spans
  processes from here on. This is where the prefork-server bill
  lands: hot `accept` through the router, the named cost of
  multi-process networking.
- **Regular files**: split. Content stays local on both sides (the
  image blob is immutable and identical in each instance); only the
  **offset cell** hoists, so `read(2)` on a fork-inherited file is a
  router fetch-add plus a local copy. Post-fork opens are fully
  private and fast, which protects the stat/read torrent completely.
- **Locks split along POSIX's own line**: `flock` is OFD state and
  rides the hoisted OFD; `fcntl` locks are per-process and *not
  inherited* — the child clears them, conformant and free.
- **cwd, fd flags, `O_CLOEXEC` bits**: per-process or per-fd; copied,
  never hoisted.

The dominant real pattern escapes entirely: `posix_spawn`/vfork+exec
takes the no-snapshot fast path with explicit fd dispositions, and the
usual survivors are stdio — already `/iso/console`-backed, already
host-side, nothing to move. The common case hoists zero objects. Plain
`fork` without exec is the expensive path, and note the trap:
`O_CLOEXEC` does not help it — cloexec clears at *exec*, so plain fork
hoists cloexec-marked fds too, since the child holds them until it
execs, if ever.

One honest corner: **inherited epoll instances.** Linux shares the
interest list across fork at the OFD level — the famous footgun.
Hoisting one means a router-side epoll whose interests mix hoisted and
private objects: a complexity knot with almost no legitimate users. v0
position: using an inherited epoll fd from the child after plain fork
is a loud, documented error, revisited if something real trips it —
a divergence stated on the box, in the same spirit as `dlmopen`.

Why the router and not "the parent serves the child": lifetimes. A
parent that exits must not tear down the pipe its orphaned child still
reads. The neutral home for shared state is the thing that outlives
both — the router, exactly as the kernel is on Linux.

Summary property: hoisting is **fork-local and type-graduated**.
Nothing pays until a fork happens; at fork each object pays according
to how much of it is genuinely shared (all of a pipe, one integer of a
file, none of the image content); and a workload that never forks
never executes a line of it.

### The crux: how a thread blocks

A blocking syscall is a wasm call from deep inside the thread's wasm
stack; running another thread requires getting off that stack. Three
options, two of them dead: **nested scheduling** (run the next thread on
top of the blocked one's frames) grows the stack with every switch and
only shrinks LIFO — unbounded on a server workload. **Asyncify-style
flag checks** after every call pay on the hot path everywhere, forever —
rejected by the same logic that rejected entry parameters for fork.

What remains: **throw.** The kernel, deciding to block, saves the
globals into the TCB and throws a wasm exception; a catch in the
scheduler loop at the bottom of the thread's execution receives it. The
unwind destroys the thread's wasm frames — and destroys nothing of
value, because the flush discipline already put every live value in
globals and memory, and the guest stack with its resume-ID chain *is*
linear memory. The wasm frames were, at that moment, pure redundancy.
Waking the thread later is `x86_resume`.

**Block = throw the wasm stack away; the guest stack is the
continuation. Wake = the fork machinery, applied to a thread.** This is
CheerpX's continuation-by-exception trick minus the hard part: nothing
needs capturing on the way down, because the ordinary calling
convention materialized the continuation in advance.

What this is not: not Asyncify (nothing instrumented on the normal
path — the throw is zero-cost until thrown), not JSPI (no host
suspension), not the stack-switching proposal (no engine feature beyond
standard exception handling). Open verification: wasm EH support in
wasmtime specifically (browsers have shipped it for years). Fallback if
absent: unwind to the host and re-enter through the run loop — same
structure, uglier seam.

**The invariant everything rests on, stated once: control changes hands
only at points where the flush discipline has run.** Syscalls are such
points by construction — verified adversarially by
`tests/call_boundary_state.rs` — and preemption polls are emitted with
a flush ahead of the yield. The kernel never sees half-flushed state.
Any future switching mechanism must first prove it sits on a flush
boundary.

### The scheduler, end to end

The instance's wasm stack at any instant:

```
host → kernel run() → [catch] → x86_resume → current thread's frames
```

One thread's frames at a time, ever — the bound the throw design buys.
The run loop is guest kernel code, the instance's real export:

```
run():
  loop:
    if runqueue is empty:
        block on /iso events (earliest timer deadline)   # the single blocking point
        turn events into runnable TCBs; continue
    tcb = runqueue.pop()
    copy tcb's register file → the globals   # ~53 values + fs_base
    try:
        x86_resume()                         # the thread runs here
        # returned: thread ran off its chain → exit path
    catch Yield:
        pass                                 # TCB already parked; loop
```

**TCB contents**: the register-file snapshot, `fs_base`, guest stack
region, tid, sigmask, sigaltstack, `clear_child_tid` (thread exit must
write 0 there and futex-wake it — `pthread_join` depends on this),
robust-list pointer, run state, wait-queue and timer linkage.

**The fast path — most syscalls never touch the scheduler.** A
translated `syscall` is a wasm call into the kernel's dispatch; the
flush discipline already parked the registers in the globals. The
kernel reads `rax` and the argument registers, does the work — a `stat`
against the VFS, a `read` from an image file — writes the result into
`rax`, and returns. No throw, no switch; the 80% filesystem torrent
costs a function call each.

**The stack a syscall runs on is not the guest's, and that is a
correctness rule rather than tidiness.** A `syscall` looks like a call
here and is translated as one, but the two differ in exactly one place
that matters: the SysV ABI lets a callee destroy the 128 bytes below
`%rsp` — the red zone — and the Linux kernel preserves them. Compilers
depend on the difference; gcc at `-O2` keeps a leaf function's locals
in the red zone across an inline `syscall` without ever moving `%rsp`.
A kernel that allocated its frames downward from the guest's `%rsp`
would walk through those locals silently, to a depth set by how deep
the kernel happened to go — and with no faults in wasm, nothing would
say so. So:

- **kisal runs on a stack of its own**, and the seam points the
  linker's `__stack_pointer` at it for the duration of the call. The
  guest's stack is never touched by kernel code at all, at any depth.
- **the translated `syscall` reserves its resume slot below the red
  zone**, not on top of it — 136 bytes, which is the slot plus the 128
  it must step over, and which leaves `%rsp`'s alignment parity
  identical to a call's.
- **the resume driver gives back what the *site* reserved**, not a
  fixed eight bytes. The driver is generic and the site is the thing
  that knows its own frame, so the size is marked in the resume ID and
  the driver reads it back out. Every later slot in a chain is popped
  by a resume body's own `ret`, and what a `ret` returns is always its
  caller's call slot, so only the first pop is ever variable.

Giving the kernel its own stack settles a second question the throw
design would otherwise raise. When a blocking syscall throws, the
seam's restore of `__stack_pointer` never runs — the unwind skips it.
With a stack borrowed from the guest, the catch would have to have
saved that pointer somewhere to put it back. With a fixed region it is
one store of a constant, so the catch in `run()` simply resets it, and
the abandoned kernel frames below it are abandoned in a region nobody
else uses. Verified by `tests/kernel_seam.rs`, which fills all sixteen
red-zone quadwords, issues a syscall, and requires all sixteen to
survive — against a native oracle running the same `.s`, in every
control-flow mode with resume on and off.

The mirror image is worth stating because the code looks the same: the
*interop* thunk, which bridges a guest call out to an ordinary wasm
function, hands the callee a stack pointer derived from the guest's
`%rsp` and is right to do so. A foreign call is allowed to eat the red
zone. A syscall is not.

**Blocking**, step by step. When the thread cannot proceed (futex value
matches, socket buffer empty, `epoll_wait` with nothing ready):

1. Globals → TCB. The guest stack top already holds the syscall site's
   resume ID, pushed by the rewritten syscall's own slot reservation:
   the continuation is fully materialized before anything else happens.
2. The kernel records a **completion** on the wait object — a recipe,
   not a result: "on wake, write 0 to saved `rax`" (futex); "on
   readiness, copy the arrived bytes into the guest buffer, then the
   count to saved `rax`" (recv). The syscall finishes later, against
   saved state, without the thread running.
3. The TCB parks on the wait object (futex bucket, socket queue, timer
   heap).
4. Throw. The unwind destroys the redundant wasm frames; the catch in
   `run()` picks the next thread.

**Waking** is always kernel code running anyway — another thread's
`futex(WAKE)` inside its own syscall, or the run loop processing a
host event from the `/iso` wait. Execute the completion, move the TCB
to the run
queue. The woken thread has returned from its syscall without executing
an instruction.

**Resuming**: load the TCB into the globals, call `x86_resume`. The
driver pops the syscall site's ID and enters that frame's resume body
at the post-syscall block, exactly as if the call had just returned
with `rax` bearing the completion value. Inherited frames run their
remainders dispatcher-shaped (measured cost below); fresh calls are
full-speed structured code; blocking again from inside a resume body
composes, because resume bodies' call sites push resume IDs too — built
and tested in `tests/fork_resume.rs`.

**Starting and exiting** use the same door. A `clone3(CLONE_THREAD)`
child's TCB gets the parent's registers at the syscall, `rax` = 0,
`fs` = the `tls` argument, `rsp` = the caller-provided stack with the
clone site's resume ID pushed on top — its first schedule is an
ordinary `x86_resume` into glibc's clone wrapper post-syscall, which
calls the start routine and exits; the fabricated chain bottom is never
popped. The process's first thread is the one special case: the kernel
builds the initial stack and calls the ELF entry directly, since a
function entry needs no resume machinery. Thread exit is the `exit`
syscall: write `clear_child_tid`, futex-wake the joiner, free the TCB,
throw with nothing to reschedule.

So the model is three reuses of one mechanism: **block, preempt and
spawn are all "the continuation is data — throw the redundant wasm
stack away"; wake, start and fork's child are all `x86_resume`.**

### Preemption: yield points as self-checkpoints

Cooperative-at-syscalls is right for the target (the GIL means CPython
threads hit futex and I/O constantly), but the CheerpX lesson
(webvm#228: a compute-bound loop with no syscalls hangs their whole VM,
and their architecture has no lever against it) says the door to
involuntary preemption must be designed in.

The insertion problem is a coverage property, not a halting proof: we
never need to detect an infinite loop, only to guarantee **no unbounded
execution path avoids a poll**. Soundness comes from a dumb sufficient
rule — a poll on every CFG back-edge and every function entry that sits
in a call-graph cycle or makes indirect calls (recursion is a loop
through the call graph) — and performance comes from *deleting* polls
that are provably unneeded, where being wrong costs cycles instead of
hangs:

- entry polls for functions outside every call-graph cycle: gone
  (almost all of them);
- back-edge polls on provably bounded loops (the counted
  `memcpy`-shaped loops compilers emit): gone;
- polls dominated by a syscall or another poll in the same loop body:
  gone;
- hot inner loops: decimated — decrement a countdown and test the flag
  every N iterations, trading preemption latency for amortized cost.

The poll is one load and one almost-never-taken branch; the taken path
flushes the registers (the translator knows the promotion map), pushes
a fabricated resume ID naming *this frame's resume body at this loop
header* onto the guest stack, and throws. Sound because the flush runs
first; possible with no new machinery because the resume body's
`br_table` already covers every block of the split graph — loop headers
included — and the driver's first pop exactly cancels the fabricated
push. Preemption is the thread forking itself at a loop header.

Prior art places the potholes: JVM safepoints are this exact scheme
(with a page-protection trick we cannot use — ours is an explicit
branch, strictly costlier, hence measured before enabled). Go ran for
years on cooperative entry polls, starved on tight call-free loops, and
fixed it in 1.14 with signal-based *async* preemption — a door closed
to us twice over: wasm has no signals, and at an arbitrary instruction
the registers live in unflushed locals the checkpoint invariant cannot
cover. We are structurally confined to Go-pre-1.14, with two advantages
Go lacked: polls are placed at translation time with the full x86 CFG
in hand, and placement is chosen per loop, not per function.

Chosen default: the countdown is a plain guest-side counter, so
scheduling is **deterministic** — same inputs, same interleaving —
keeping the host boundary the complete nondeterminism inventory and
record/replay intact. A host-poked flag is lower-latency and stays
available as a debug override.

### The cost of having been suspended, measured

A resumed frame's remainder runs in its resume body — dispatcher
shape — until the frame returns, because wasm structured control flow
cannot be entered mid-nest (there is no instruction that jumps into the
interior of a `block`/`loop` nesting; that is *why* the dispatcher
exists). Fresh calls are fast; the degradation is confined to
remainders of frames live at suspension and dies at each return. A
server thread suspends constantly, so this was the design's open
performance question. It is now measured: `benches/kernels.rs` builds
both control-flow modes permanently (same correctness gate), and at
`-O2` under wasmtime/Cranelift the dispatcher costs:

| kernel | gcc | clang |
|---|---|---|
| `bench_integer` | +0.7% | +1.1% |
| `bench_memory` | +1.2% | +0.2% |
| `bench_float` | +2.1% | +0.8% |
| `bench_calls` | −1% (noise) | **+45%** |

Seven of eight cells at 1–2%; one real outlier on clang's deep-recursion
kernel. Verdict: post-resume degradation is a non-problem at this
evidence level, and the mitigation stays unbuilt. The mitigation, for
when a profile demands it: the remainder from a resume point is itself
a single-entry CFG, so the transpiler can emit it as its own
*structured* continuation function — an AOT analogue of a JIT's OSR
entry compilation — selectively, for statically hot sites only, with
the dispatcher body as the universal fallback. Two honest limits on the
measurement: small kernels (dispatcher cost should grow with block
count — a `br_table` over the hundreds of blocks of CPython's eval loop
is unmeasured), and one engine (a browser's tiering JIT may treat the
flat loop differently).

### Why not real threads, and when to revisit

Instance-per-thread over one shared memory is tempting: per-instance
globals give each thread its own register file automatically, futex
maps onto `Atomics.wait`, and blocking in a host import just blocks a
host thread — deleting the entire throw/resume dance. The wall is
memory ordering: **x86 is TSO; wasm shared memory is weak.** Translated
plain stores would lose ordering that real lock-free code — glibc
internals, CPython's own atomics — silently relies on, and making that
sound means fencing or atomicizing ordinary memory traffic: a
translation-wide cost and audit. A correctness wall, not a preference
(and plausibly part of why CheerpX is single-threaded too). Green
threads first. Revisit trigger: a workload that is genuinely
parallel-compute-bound *and* a decision on the TSO-emulation cost; the
GIL keeps that trigger far away for the Python world.

Sharp edges, named: no guard pages means guest stack overflow silently
corrupts (a debug-mode red-zone check at function entry is the cheap
partial answer); `sched_getaffinity` honestly reports one CPU; signal
delivery interacts with all of this — the full design, chain surgery
through a resume-compatible `signal_dispatch`, is in the Signals
section.

## Kisal's VFS and resource model

The trace made the filesystem 80% of all syscall traffic, so this layer
is where the working-first doctrine earns or loses its keep. The design
has three commitments: guest file content never leaves the guest
(bundled at build time), POSIX semantics live in exactly one place
(kisal's resolution loop, never in stores), and whether an operation
touches the host is decided by the mount table at boot, never
per-operation.

### The bundle is a linked object

The baked image is just another relocatable wasm object, in the same
LLVM linking format everything else uses. A bake tool flattens the OCI
layer stack into two data segments — `__image_blob` (file contents) and
`__image_index` (everything else) — and the final artifact is one link:

```
wasm-ld  app.wasm.o…  kisal.wasm.o  image.wasm.o  -o container.wasm
```

One module *is* the container. Kisal finds the image through link-time
symbols, not magic addresses; the linker places it. The bake pass is
also where the dynamic-linking design lives (see "Dynamic linking and
ld.so") — it walks the layers and, for every ELF it finds: transpiles
it in linked-ELF mode at a bake-assigned **prelink base**, records the
static exec map (address → function-table slot), reserves the shadow
GOT beside the real one, sets `DF_1_NOW` in `.dynamic`, and
regenerates `/etc/ld.so.cache` against the flattened tree — so
bundling and code translation are one pass over the same tree.

Consequences:

- **Boot and multi-process are cheap on native for free.** wasmtime
  instantiates memories copy-on-write from a prepared memory image
  (memfd-backed), so the ~125 MB of image bytes are physically shared
  across every process instance and instantiation is an mmap, not a
  memcpy. The OS page cache's economics without building one.
- **Fork does not copy the image.** The image region is written by
  nobody — a guest write there is a wild pointer under any semantics —
  so the spawn payload says "instantiate fresh (data segments
  repopulate the image region), then overlay the snapshot *minus* that
  region." Child cost drops to the process's mutable footprint. The
  immutability is unenforced (`mprotect` is bookkeeping; that is the
  standing threat model); a debug-mode checksum over the region is the
  honesty check.
- **The memory floor is the rootfs size.** A `python:3.11-slim`-shaped
  image is ~125 MB uncompressed, permanently resident. Fine natively,
  tolerable in browsers (CheerpX budgets 700 MB). The escape hatch for
  genuinely huge images is already in the mount table's nature: an
  image mount *could* be an `/iso`-backed store streaming lazily from
  the host — same store interface, different mount config, at the price
  of file reads crossing the boundary. Default stays baked-in.

**Blob layout.** Contents aligned to 16 bytes normally; files the baker
identifies as mmap candidates (ELFs) aligned to 4096 — this serves the
*flagged* zero-copy aliasing optimization (see the mmap section; v0
eager-copies all file mappings and does not rely on it), which needs
page-aligned mapping addresses congruent with file offsets.
Page-aligning *everything* would waste ~2 KB × ~35k files ≈ 70 MB. No
compression in memory: the image is RAM-resident and `read(2)` copies
straight out of it, so compression would force decompress-on-read; the
`.wasm` file on disk gzips fine for transport.

### The index format

One contiguous, versioned, little-endian structure; a header pointing
at four regions:

```
header      magic, version, region offsets/counts
inodes      fixed-size records, one per inode (hardlinks share one)
dirents     per-directory sorted entry arrays
strings     names, symlink targets, uname/gname
xattrs      variable-length blocks, referenced from inodes
```

It is designed around what the strace showed: the `stat` torrent
includes a lot of *misses* (CPython probing its module path), so
negative lookups must be as fast as hits. Lookup is a binary search per
component; `stat` is an index multiply and field copies — no parsing,
no allocation. Squashfs-lite, tuned for pointer-walking in RAM rather
than block decompression.

**The inode record** (fixed-size, 64 bytes packed). The baker preserves
metadata completely; whether kisal *honors* any given field is a
separate, deferred decision — and deferral is exactly why preservation
must be total, since nothing can later enforce what the bake threw
away.

| field | size | notes |
|---|---|---|
| `mode` | u32 | `st_mode` verbatim: type + permission bits **including setuid/setgid/sticky** — preserved, not (yet) honored |
| `uid`, `gid` | u32 × 2 | numeric, from the tar header (PAX-extended where present) |
| `nlink` | u32 | computed by the baker *after* whiteout processing; tar hardlink entries resolve to a shared inode record (`python3` and `python3.11` are one inode, `nlink = 2`) |
| `size` | u64 | |
| `mtime_sec`, `mtime_nsec` | i64 + u32 | tar gives seconds, PAX can give nanoseconds — preserve what is there, zero what is not; `ctime`/`atime` are not in tar, the baker sets both to mtime and the format does not pretend otherwise |
| `payload` | u64 | union by type: blob offset (regular), string ref (symlink target), `rdev` major/minor (device nodes — base images ship `/dev` entries; preserved, honored or not) |
| `xattr_ref` | u32 | offset into the xattr region, 0 = none |
| `uname_ref`, `gname_ref` | u32 × 2 | tar's *symbolic* owner names, which numeric ids do not capture; kisal ignores them, tooling and faithful re-export want them |
| `flags` | u32 | baker-derived: `MMAP_ALIGNED` (blob placement page-congruent — eligible for the flagged aliasing optimization; unused by v0), `EXEC_TRANSPILED` (ELF with an exec-map entry) |

**Xattrs, binary-faithful.** Tar carries xattrs as PAX records
(`SCHILY.xattr.*`); the two kinds that matter for a later enforcement
decision are `security.capability` — a packed binary struct, the thing
that lets `ping` work without setuid — and `user.*`. The region stores
them exactly as byte strings: count, then `(name_ref, value_len,
value_bytes)` per entry. No interpretation at bake time;
`getxattr`/`listxattr` serve them verbatim from day one, and if kisal
ever honors file capabilities the bits are sitting there un-mangled.

**Dirents** are per-directory sorted arrays of `(name_ref,
inode_index, d_type)`, with `d_type` precomputed from the target's
mode — `getdents64` emits real `DT_REG`/`DT_DIR`/`DT_LNK`, and
CPython's importer uses `d_type` to skip stats, so this directly shaves
the torrent.

**Whiteouts are a bake-time concept only.** `.wh.` files and
opaque-directory markers are layer-stack artifacts; none survive into
the index. The overlay kisal runs at *runtime* has its own whiteout
mechanism for the container's own unlinks. Bake-time whiteouts flatten
history; runtime whiteouts are live state; the two never mix.

**Size**: ~35k inodes × 64 B ≈ 2.2 MB, plus dirents and strings —
4–6 MB of index against ~125 MB of blob. The fork skip-region treats
both segments as immutable together.

**Format philosophy: packed binary as the data plane, structfs as the
view.** The structfs-native alternative — metadata as CBOR/JSON
records — would be pleasingly uniform and would put a decode and an
allocation inside every one of the 1,226 stats. So the format is this
packed binary, and the `/img` store *presents* it: reading
`/img/usr/bin/python3/meta` renders a structfs `Value` map (mode, uid,
xattr names, the lot) on demand, for the REPL, for tooling, for
diffing two bakes. The hot path walks bytes; the inspectable path
speaks structs. Same data, one copy, two faces.

### Resolution: POSIX lives in kisal, never in stores

The layering, top down — the discipline is *where POSIX stops*:

1. **fd table** (per process): fd → open file description.
   `dup`/`dup2` share OFDs, so offsets are shared exactly as POSIX
   demands; `O_CLOEXEC` lives on the fd, not the OFD.
2. **The vnode walk.** `.`, `..`, symlink following (40-hop limit),
   `openat` dirfd-relativity, `O_NOFOLLOW`, trailing-slash rules — all
   in kisal's resolution loop, component by component, never delegated.
   Structfs paths have no `..` and no symlinks; asking stores to
   emulate them would smear POSIX across every backend. The walk asks
   stores one dumb question — `lookup(dir, name) → entry` — and kisal
   splices symlinks, crosses mount points, and enforces the rules
   itself. Stores stay dumb; POSIX stays in one place.
3. **The mount table**, longest-prefix — and a distinction that only
   became visible with structfs: **there are two trees, and the guest
   sees only one.** Kisal's internal structfs tree contains
   everything — image store, overlay upper, synthetics, and `/iso`.
   The guest's POSIX namespace is a mapping onto subtrees of it, and
   `/iso` is deliberately not in the mapping: a container app calling
   `stat("/iso")` gets `ENOENT`. The courtyard's service gate is not an
   address in town.
4. **Vnode type dispatch**: regular, directory, symlink, chardev,
   pipe, socket, epoll, eventfd — each an ops table. Stores back the
   file and directory ops; the rest are kernel objects.

The initial POSIX namespace for a container:

| mount | backing |
|---|---|
| `/` | overlay(upper: memory store, lower: image store) — the whole rootfs CoW-writable, which is exactly container semantics |
| `/proc`, `/sys` | synthetic stores over kisal's own state (`/proc/self/maps` renders the VMA tree, `/proc/self/stat` the TCB; the minimal set glibc probes) |
| `/dev` | synthetics: `null`, `zero`, `urandom` (the in-guest CSPRNG), `tty` (`ENOTTY`-flavored honesty for the ioctl torrent) |
| volumes (optional) | a POSIX path backed by an `/iso` subtree — the bind-mount equivalent, off by default, and the one way filesystem traffic ever crosses the boundary |

### The overlay

Copy-up on first write-open of a lower file; whiteout entries for
unlinks of lower files; directory listing is a sorted merge of upper
over lower minus whiteouts. Upper-file content lives in kisal's heap
and is *not* stably addressed, so upper files could never be aliased
even under the flagged optimization; under v0 every file mapping is an
eager copy regardless (see the mmap section), which also makes the
copy-up/mmap interaction trivial: an existing `MAP_PRIVATE` copy of a
lower file is untouched by a later copy-up, and `MAP_PRIVATE`
explicitly leaves post-map file-write visibility unspecified, so that
is conformant.

An atomicity gift worth stating as an invariant: **kisal operations
are atomic by construction unless they explicitly wait.** Green
threads switch only at blocking syscalls and emitted polls, and
kisal's own code contains neither mid-operation — so `rename` over the
overlay (copy-up, whiteout, link) is atomic without a single lock.

### Internal vs. external is a mount-time decision

Kisal never decides internal-vs-external while serving a syscall; the
mount table decided at boot. Resolution lands in a store; the store is
RAM-backed or `/iso`-backed; every subsequent fd operation goes where
the vnode points, oblivious. The only fd types that ever cross:
console chardevs (stdio ↔ `/iso/console`), external sockets (the
`/iso/net` broker — loopback, `socketpair`, pipes never cross), and
opted-in volume mounts. Everything else — the entire stat/read
torrent, overlay writes, `/proc`, `/dev/urandom` — is RAM all the way
down. "Resources, not syscalls," made structural.

The blocking wrinkle, recorded honestly: `/iso`-backed stores are
*synchronous* ll-store calls, so a slow host store (a large volume
read) stalls the whole instance for its duration, not just the calling
thread. Working-first doctrine accepts this; the fix, when a workload
cares, is the broker pattern applied to volume stores —
write-then-read-handle with the thread parked in between, same as
sockets.

Cheap honesty at the edges: `flock`/`fcntl` locks are in-guest state
(their semantics across a future fork ride on the router's
fd-hoisting); `O_DIRECT` and `fsync` are cheerful no-ops (there is no
device to sync); `ioctl(TCGETS)` on non-ttys returns `ENOTTY`.

## Subsystem designs

### futex

The classic shape, in-guest: a wait-queue map keyed by linear-memory
address. `WAIT` re-checks the value (atomic against the guest because the
scheduler only switches at syscalls and emitted polls, and the kernel is
neither), blocks the thread; `WAKE` moves up
to `n` waiters to the run queue; `WAIT_BITSET`/`WAKE_BITSET` store and
filter on the mask; `CLOCK_REALTIME` timeouts go on the timer heap that
feeds the idle-kernel host wait.

### mmap

**The address space.** A simplifying fact first: the main binary is
never "loaded." zaqaru resolves every operand to symbol+addend, so the
app's data sections are placed by `wasm-ld` and there are no ELF
virtual addresses to honor for the main program — ELF loading exists
only for what the dynamic loader maps at runtime, and those addresses
come from kisal's allocator like any other mmap. The space kisal
manages is linear memory above `__heap_base`, in arenas:

- **brk arena**: contiguous, with a configured ceiling. glibc
  allocates via `brk` until it fails; `brk` past the ceiling returns
  `ENOMEM` and glibc falls back to mmap — exactly what it expects.
- **mmap arena**: everything above, gap-searched. Thread stacks are
  not special — they are guest `mmap(MAP_STACK)` calls, as the trace
  shows.

Wasm memory grows only, in 64 KB pages — both invisible to the guest.
Kisal maintains the **4 KB page fiction** in bookkeeping alone:
lengths round to 4 K, `munmap`/`mprotect` reject unaligned arguments
with `EINVAL`, and `memory.grow` happens in amortized chunks when a
reservation crosses the current size. Because memory never shrinks,
`munmap` returns ranges to a free pool for address reuse — with one
semantic obligation: a reused range handed out as fresh anonymous
memory must read as zeros, so kisal keeps a high-water mark (above it,
`memory.grow` already zeroed; below it, `memory.fill(0)` on
allocation).

**The VMA tree.** An ordered map keyed by start address: `{start,
len, prot-as-recorded, flags, backing, name}`, backing one of
Anonymous, ImageBlob{inode, offset}, OverlayFile, or Reservation
(`PROT_NONE`). Every operation is interval surgery: `mmap` finds a gap
or honors a hint; `MAP_FIXED` **atomically replaces** what it overlaps
(split the old VMAs, install the new); `munmap` punches holes;
`mprotect` splits and records; `MAP_FIXED_NOREPLACE` returns `EEXIST`.
The tree is ordinary kisal state, so fork snapshots it for free — and
`/proc/self/maps` is a rendering of it, which is not decoration:
glibc's `pthread_getattr_np` reads it for stack bounds.

The sequence that must work precisely is ld.so's carving dance from
the trace: map the whole file, then `MAP_FIXED` each segment over it
at its vaddr offset —

```
mmap(NULL, 185256, PROT_READ, MAP_PRIVATE|MAP_DENYWRITE, fd, 0)      = base
mmap(base+0x4000, 147456, PROT_READ|PROT_EXEC, MAP_FIXED|…, fd, 0x4000)
mmap(base+0x9c000, 16384, PROT_READ,           MAP_FIXED|…, fd, 0x28000)
```

The extent map copies file bytes into the arena; each `MAP_FIXED`
splits the extent VMA, installs a segment VMA, and copies that
segment's file range to that offset (for text/rodata of a well-formed
`.so`, file offset and vaddr offset coincide and the copy is already
right; the RW data segment genuinely re-copies). For an
`EXEC_TRANSPILED` inode the extent mmap returns the **prelink base**
the bake assigned (see "Dynamic linking and ld.so"), so the static
exec map is already correct — the `PROT_EXEC` fixed map just flips the
module's live bit, and `munmap` flips it back. `mmap(PROT_EXEC)` of
anything *without* that flag is a loud error — untranslated bytes
cannot run, and pretending otherwise is a silent hang later.

**The flavor rules, v0:**

- Anonymous private: reserve + zero (eagerly — no faults means no lazy
  zeroing). `MAP_NORESERVE`, `MAP_STACK`: recorded, no-ops.
  `PROT_NONE` reservations (glibc's thread-stack extents): VMAs with
  no obligations.
- **File-backed, every kind: eager copy.** Image files `memcpy` from
  the blob, overlay files from the upper store. POSIX leaves post-map
  visibility of file writes unspecified for `MAP_PRIVATE`, so the copy
  is conformant, not a shortcut.
- Writable `MAP_SHARED` stays deferred *and named*: it needs dirty
  tracking or write-back at `msync`/`munmap`, and nothing in the
  target workload creates one. `msync` is a no-op until then.
- `mprotect`: split, record, enforce nothing (threat model above) —
  with the aliasing exception below if that optimization is ever on.
- `mremap`: absent from the trace but glibc `realloc` uses it. Trivial
  in a flat space: extend in place when the adjacent gap is free; else
  with `MREMAP_MAYMOVE`, allocate + `memory.copy` + unmap; else
  `ENOMEM`.
- `brk`: bump pointer within its arena.

**`madvise` is not a no-op** — a correction discovered by checking the
trace rather than assuming. `MADV_DONTNEED` (×7 in the trace, glibc's
arena-free operation) has *visible* semantics on Linux: subsequent
reads of anonymous memory see zeros. Kisal must `memory.fill(0)` the
range. `MADV_FREE` (×1) is the lazy cousin; eager zeroing is a
conformant implementation. Everything else (`MADV_HUGEPAGE`,
`WILLNEED`, `SEQUENTIAL`, …) genuinely is record-and-ignore.

**Zero-copy aliasing: designed, demoted, flagged.** An earlier draft
made blob aliasing the default for image-backed maps. Writing out the
semantics exposed the hole: aliasing hands the guest an address inside
the shared immutable blob, and `mprotect(+PROT_WRITE)` on a
`MAP_PRIVATE` mapping is legal on Linux regardless of how the fd was
opened — after which a write must not be visible through the "file,"
but the file *is* the blob, shared with every other mapping, and with
fork's skip-region assumption. The mapping cannot be migrated (the
guest holds pointers), so the only exits are corrupting the shared
blob — a real correctness break, worse than the accepted no-protection
model — or refusing the `mprotect`, a visible Linux divergence. Hence
v0 copies. The optimization, when measured cause appears: alias
`MAP_SHARED` from `O_RDONLY` fds unconditionally (POSIX itself permits
denying later `PROT_WRITE` there), alias R/RX `MAP_PRIVATE` behind a
flag with `mprotect(+W)` answered by `EPERM` and a loud kisal log, and
keep a global off-switch as the always-conformant mode. The baker's
4 K alignment of ELF blobs and the `MMAP_ALIGNED` flag serve this
optimization and cost little; v0 does not rely on them.

**Guard pages**, honestly: a `PROT_NONE` guard VMA is recorded and
unenforced like all protection; an unmapped gap would not help either,
since nothing faults. Stack overflow walks into the neighbor
silently; the debug-mode canary remains the only real mitigation.

### Dynamic linking and ld.so

The hardest item on the empirical list. Tier one stands: **static
first** — build the image's binaries statically (a static CPython is a
normal artifact), every executable byte in one ELF transpiled ahead of
time, and this whole section vanishes. For an image we build ourselves
that is the correct move, not a dodge. What follows is the general
tier: unmodified dynamic images, glibc's `ld.so` running as ordinary
guest code.

The orienting fact: ld.so's work is almost entirely *data* work — GOT
slots, `.data` pointers, TLS offsets, computed as `base + offset` and
written to memory. In the flat-memory model those values are honest
linear-memory integers, so the entire relocation machine
(`R_X86_64_RELATIVE`, `GLOB_DAT`-to-data, `.data.rel.ro`, COPY
relocations) runs unmodified with zero help. The thorn is confined to
the values that name *code*. Working that thorn produced two
architecture decisions and one mechanism.

**Decision: the container pipeline transpiles linked ELFs, not `.o`
files.** A dynamic main executable must be *findable by ld.so*: it
reads `AT_PHDR`, walks the exe's `PT_DYNAMIC`, patches its GOT, and
COPY-relocates into its `.bss` — all at ELF virtual addresses. So the
exe's data must live at those addresses, which rules out letting
`wasm-ld` place it. Linked-ELF mode consumes complete ELFs: all
internal references are already-baked concrete addresses that pass
through *with no symbolization at all* — the flat-memory dividend, and
precisely the problem (pointer/literal ambiguity) that killed a decade
of static-rewriting work, absent by construction. Data segments are
placed address-faithfully; only code addresses need translation. The
relocatable-object mode stays as the interop and testing pipeline.

**Decision: prelink at bake.** Every ELF in the image is assigned a
fixed load base at bake time and transpiled *at* that base, so its
internal data references are concrete — no per-reference
`module_base +` arithmetic. Kisal's `mmap` of an `EXEC_TRANSPILED`
inode simply returns the baked base: ld.so asks for "anywhere," gets
the same address every run, and its `MAP_FIXED` carving lands where
the bake assumed. This makes the **exec map fully static** — the
address → function-table-slot table for every module is built at bake;
`dlopen`/`dlclose` flip a live bit, nothing moves. It also makes load
addresses deterministic, feeding the record/replay story.

"Every ELF in the image" means the **tree, not the executable's
`DT_NEEDED` closure**, and the difference is load-bearing: `dlopen`
names files no closure reaches — an extension module is linked *from*
by nobody — and a distribution CPython's `lib-dynload/` is 47 of them
before the first third-party package. So the bake sweeps the image for
ELFs, translates each at its assigned base, and merges all of them into
the one translation unit, because the exec map must span anything a
function pointer can ever name. The unit of translation is the image.
(The built tier walks the closure only; the sweep is the recorded gap,
in the build plan's Python appendix.)

One hardening the forced placement owes. Kisal answers the loader's
"anywhere" by rewriting the request to `MAP_FIXED` at the prelink base
— and a `dlopen`-only module's range sits *empty from boot until the
import*, potentially the whole run. `MAP_FIXED` is atomic replacement,
so anything that had wandered into the range would be silently paved
over. The layout makes that impossible by construction (the arenas are
carved above everything the bake placed), but "by construction" is a
claim to assert, not narrate: the rewrite must carry
`MAP_FIXED_NOREPLACE` semantics — refuse loudly if the prelink range is
occupied — so the failure mode is a named error instead of memory
corruption. Costs, named
plainly: no ASLR (we are not pretending to be a hardened kernel), and
each DSO loads at most once per process — true of ld.so anyway;
`dlmopen` namespaces are a loud out-of-scope.

**Mechanism: the shadow GOT.** A cross-DSO call is `call foo@plt` →
`jmp *GOT[n]`, an indirect transfer whose operand is a code address
ld.so wrote, not a table slot. Three layers — and a correction from
building it, stated first because reading the three in order leaves the
opposite impression: **only the middle layer is needed for a cross-DSO call
to work, and it is machinery that already exists.** In linked mode every
indirect transfer is an exec-map lookup, and a GOT slot holds an address the
bake translated at, so the generic fallback *is* the discriminating indirect
call and nothing had to be built. The shadow array is a cache in front of
it: worth having, not a prerequisite, and now with a working baseline to
measure against.

- *Discrimination is free*: table slots are small integers, mapped
  code addresses sit megabytes up in the arena; one bounds check
  distinguishes them.
- *The generic fallback*: address → static exec map → (module,
  offset) → the offset must be exactly a transpiled function entry
  (mid-function entry is a loud error, same discipline as everywhere)
  → slot → `call_indirect`. Correct for everything, including what we
  cannot predict.
- *The fast path*: the bake reserves a parallel array beside each
  module's GOT. The translated PLT-shaped call loads `GOT[n]`,
  compares against `shadow_addr[n]`; on hit, `call_indirect
  shadow_slot[n]` — two loads and a compare, no range lookup; on miss,
  generic fallback, then fill the shadow. This one cache uniformly
  absorbs eager binding, ifunc results, and **symbol interposition**:
  `LD_PRELOAD` just writes a different address into `GOT[n]`, the
  shadow misses once, refills, and interposition *works* — a
  correctness feature, not merely speed.

Two glibc landmines are defused deliberately rather than survived:

- **Lazy binding is turned off at bake.** `_dl_runtime_resolve` is the
  hairiest asm in userspace — it XSAVEs vector state, an instruction
  family that should never be on any path. The baker sets `DF_1_NOW`
  in every module's `.dynamic` (belt: kisal also injects
  `LD_BIND_NOW=1`). Eager binding at load; the resolver never runs;
  XSAVE never needs translating — and eager prelinked binding is what
  makes the shadow GOT mostly warm from the start.
- **CPUID is a translation-surface control knob.** ld.so and glibc's
  ifunc resolvers ask CPUID to select memcpy/strlen implementations.
  Kisal-era translation implements CPUID and *curates the answer*: a
  baseline x86-64 without AVX, so the resolvers deterministically
  select the SSE2 paths the corpus already covers instead of AVX-512
  code. Ifunc itself is unremarkable: ld.so calls the transpiled
  resolver during relocation, writes the returned address to GOT, and
  the shadow GOT consumes it; with CPUID fixed at bake, ifunc
  selection is deterministic too.

The rest rides existing machinery. TLS: `TPOFF` values are fs-relative
offsets consumed by the `x86_fs_base` translation; ld.so's static-TLS
layout computation and `__tls_get_addr` are ordinary transpiled code;
TLSDESC's call-through-GOT is another shadow-GOT customer. One honest
caveat on "ordinary": the *dynamic*-TLS half — DTV growth when a
`dlopen`ed module carries `__thread` — is the same guest code down a
path nothing has yet executed, since the boot-time libraries get static
TLS. The dlopen corpus includes a `__thread` variable in the loaded
module for exactly that reason. Process
start: kisal builds auxv (`AT_PHDR`, `AT_ENTRY`, `AT_BASE`, no vDSO,
`AT_RANDOM`) and enters ld.so at its entry — a known table slot in a
prelinked module, so the "jump" is a `call_indirect` with fabricated
initial state. `/etc/ld.so.cache` is a baked file the baker
regenerates against the flattened layers so the cache matches the
image. Code bytes are still copied to their mapped addresses, so
anything that *reads* code (ld.so checking ELF headers) sees the
truth.

Limits, on the box: **no runtime-installed code.** `dlopen` of
anything baked: full ld.so path, works. `dlopen` of a file that
arrived at runtime — `pip install` fetching a native wheel — hits
`mmap(PROT_EXEC)` on a non-`EXEC_TRANSPILED` inode: loud error.
Pure-Python installs are just files and work fine; native extensions
must be baked. That is the AOT deal.

One dependency stated plainly: `dlopen`'s *error* path leaves by
`longjmp` (`_dl_catch_exception`), so a `dlopen` that fails inside
ld.so — a missing `DT_NEEDED` of an extension module, or the
name-probing `ctypes` does routinely — needs the setjmp/longjmp design
(its own section below). The happy path never longjmps — the `setjmp`
on entry is save-only, and save is ordinary code — so imports of baked
modules do not wait on it. Until it is built, a failing `dlopen`
presents as an exec-map miss on the saved-continuation value rather
than as a `dlerror`, and the dlopen corpus triggers that once
deliberately so the shape is on record.

And the labeled grind: transpiling ld.so and glibc themselves —
hand-written entry asm, self-relocation, jump-table-rich string
functions. The loud-error discipline surfaces missing instructions one
at a time, exactly like the SSE campaign, with CPUID curation
shrinking the surface. Code discovery on stripped-but-linked ELFs
leans on `.dynsym` + `.eh_frame` FDEs + init arrays + PLT parsing
(glibc annotates even its asm with CFI, which is why
`.eh_frame`-as-oracle has been in the design from the start); a
coverage gap is a *bake-time* error, the right time to find one.
Testable end to end with a hand-built `dlopen` corpus under the
differential suite.

### Sockets and epoll

The empirical surface is tiny — the trace shows one `AF_INET` stream
listener, `SO_REUSEADDR`, and two `AF_UNIX` connects that are glibc
probing `/var/run/nscd/socket` and taking `ENOENT` (which the VFS
produces before socket code is even involved). No netlink appeared,
because `getaddrinfo("127.0.0.1")` short-circuits on numeric hosts —
a deferral, not an absence: real hostname resolution fires glibc's
`check_pf` netlink dance, and the fix has direct precedent (gVisor's
minimal `AF_NETLINK` answering `RTM_GETLINK`/`RTM_GETADDR` for `lo`
plus one interface). Named for later, with DNS itself (outbound UDP 53
through the broker on native hosts; an `/iso/net/resolve` path for
browser hosts, where UDP does not exist; baked `/etc/hosts` for static
cases).

**One readiness primitive under everything.** Every waitable kisal
object — stream buffer, listener, datagram queue, eventfd, epoll
instance itself — implements one interface: `poll_mask() →
EPOLLIN|EPOLLOUT|EPOLLHUP|…` plus a wait queue woken on state
transitions. This is Linux's own `poll_wait` design. Blocking
`recvfrom` parks on the object's queue with a completion recipe;
`poll(2)` builds a transient waiter across objects; epoll holds
persistent interest; nested epoll-on-epoll falls out, because an epoll
instance is just another waitable whose `EPOLLIN` means "ready list
non-empty." Kisal being a single actor deletes the hard parts of
Linux's version — no wakeup races, no thundering herd, no
`EPOLLEXCLUSIVE` — while observable semantics stay identical.

**No packets, anywhere.** Kisal has no TCP state machine, no IP layer,
no checksums, because nothing ever becomes a packet. Three backends
behind one socket vnode:

- *In-guest, short-circuited*: `connect(127.0.0.1:P)` looks up P in
  the port table, finds the listener, creates a connected pair of ring
  buffers, and queues one end on the accept queue — `SOCK_STREAM`
  semantics over reliable in-memory pipes. `AF_UNIX` is the same
  machinery keyed by VFS path (`bind` creates the filesystem node,
  which also makes the nscd-probe behavior automatic). UDP loopback is
  per-socket datagram queues; `pipe2`/`socketpair`/`eventfd2` are the
  same family in simpler shapes. The correctness effort goes into the
  half-close matrix: `shutdown(SHUT_WR)` marks the ring EOF so the
  peer reads drain-then-zero; writing to a peer-closed ring raises the
  *synchronous* SIGPIPE from the signals design (or `EPIPE`); `close`
  drops a refcount, and a ring with undrained data still delivers it.
  `FIONREAD` is a ring-length read.
- *Host-edge, via the `/iso/net` broker* (isotope extensions):
  `write /iso/net/connect {host, port}` → handle `/iso/net/conn/{j}`;
  data via `…/rx` and `…/tx`; `…/ctl` for shutdown/close. Listeners
  invert: the **mount config** — not the guest — declares port
  mappings (`host 8080 → guest 5057`, exactly `docker -p`, living
  host-side as data). When the guest binds a mapped port, kisal
  registers `write /iso/net/listen`; inbound connections arrive as
  events the kernel wait returns, materialized as accepted in-guest
  vnodes bridged to `conn/{j}`. Unmapped listeners are loopback-only;
  no `/iso/net` mount means no network at all —
  capability-as-mount-config doing the firewall's job. Nonblocking
  `connect` gets faithful semantics free from the broker's async
  shape: `EINPROGRESS`, then the completion event sets writability and
  `SO_ERROR`, exactly what callers poll for.
- *The readiness bridge*: host events (rx data, tx room, incoming
  connection) arrive in batches from the single kernel wait and update
  a guest-side readiness cache; kisal wakes wait queues off cache
  **transitions**. That word carries `EPOLLET`: edge-triggered guests
  must see edges derived from kisal's cache transitions, not raw host
  events — a host "readable" while the cache already says readable is
  not a transition and must not produce an ET wake.

**epoll semantics, the famous ones on purpose.** Level-triggered
entries re-poll their mask at report time; edge-triggered entries arm
on transition and disarm on report; `EPOLLONESHOT` disarms until
re-armed. And interest is registered **on the open file description,
not the fd** — so the infamous "closed fd still fires because a dup
survives" behavior is faithfully present, because real software and
test suites depend on it.

**Blocking and timeouts.** `recvfrom` on an empty ring parks with the
recipe "on `EPOLLIN`, copy up to n bytes, `rax` = count" — executed at
wake against saved state, per the scheduler design.
`O_NONBLOCK`/`FIONBIO` (per-OFD) consult readiness and return `EAGAIN`
immediately. `SO_RCVTIMEO`/`SNDTIMEO` are a parked wait racing a
timer-heap entry to the same completion — load-bearing, because
`socket.settimeout()` guards every werkzeug connection, and CPython's
internal `sock_call` retry loop exercises both the nonblocking-poll
path and `EINTR` restart, tying this section to signals. `setsockopt`
is mostly recording: `SO_REUSEADDR` (the one observed call) affects
port-table rebind rules and nothing else; `TCP_NODELAY`/`SO_KEEPALIVE`
are recorded no-ops with a straight face, there being no TCP to tune;
`SO_RCVBUF`/`SNDBUF` honestly size the rings.

**The multi-process honesty clause.** The port table and listening
sockets are netns state, and a netns spans processes — but processes
are separate instances with separate kisal states. Single-process
containers: port table lives in kisal, done. The moment fork exists,
fd hoisting (process model, above) applies to sockets: listeners and
connected sockets hoist to router-backed objects and the port table's
authority moves to the router. Contained inside fork like every other
cost of it — but this is the subsystem where the bill is largest (a
prefork server's hot accept path runs through the router), so it is
written down rather than discovered.

### Signals

**State and routing.** Per process: the disposition table
(handler/`SIG_DFL`/`SIG_IGN`, `sa_flags`, `sa_mask` — dispositions are
process-wide per POSIX) and a process-pending set. Per TCB: the
thread's mask, thread-directed pending set, and `sigaltstack`.
Thread-directed signals (`tgkill`) go to that TCB; process-directed to
any thread with the signal unblocked, staying process-pending if all
block it. Cross-process `kill` arrives via the router;
`/iso/shutdown/requested` is synthesized into a process-directed
SIGTERM to init. Standard signals are a pending *set*; RT signals get
a real queue with `siginfo`.

**Delivery points** are dictated by the standing invariant: kisal may
deliver only where the flush discipline has run — syscall completion,
the scheduler before resuming a thread, wait-interruption for blocked
threads, and preemption polls. No async delivery at arbitrary
instructions, ever (same closed door as async preemption, same
reason: unflushed locals). A consequence worth having: `SIGALRM`
latency on a compute-bound thread is bounded exactly by the
preemption-poll interval — the two designs interlock.

**Delivery is chain surgery.** The naive design — kisal calls the
handler as a nested wasm call — dies on "what if the handler blocks?":
handlers legitimately call `write` and friends, and a nested call
would put kisal's delivery bookkeeping on the stack the block-throw
unwinds. So handler invocation is not special. Kisal provides one
function with the resume-body signature — `signal_dispatch`, at a
reserved table slot — and delivery to a thread (whose chain top is,
by construction, some resume ID) is:

1. Build a **real `rt_sigframe`** on the guest stack — or the
   altstack under `SA_ONSTACK` — with a faithful `ucontext` from the
   TCB snapshot (GPRs, flags, `fs`), `siginfo`, and the interrupted
   continuation's resume ID *stored in the frame* (not inferred from
   stack adjacency — which is what makes altstack delivery clean).
2. Push a fabricated resume ID naming `signal_dispatch`.
3. Set ABI argument registers in the TCB (`rdi` = signum, `rsi` =
   &siginfo, `rdx` = &ucontext), `rsp` below the frame.

On the thread's next resume, the driver enters `signal_dispatch`,
which: swaps in the handler-time mask (old mask into `uc_sigmask`,
where Linux puts it — `sa_mask` and `SA_NODEFER` fall out for free);
resolves the handler value to a table slot by the same discrimination
as the shadow GOT (small integer = slot, exec-range address = map
lookup); pushes a return slot carrying a second `signal_dispatch`
entry (the post-handler point); and calls the handler as an ordinary
guest-convention call. When the handler returns, `signal_dispatch`
reads the frame back **including guest modifications to the
ucontext** (Linux applies them at sigreturn; so do we), restores the
TCB and mask, pops the frame, and yields the interrupted
continuation's ID from the frame to the driver.

Everything hard becomes free. A handler that **blocks** just parks
the thread — its chain is `[original][frame][post-handler][handler
frames]`, fully resumable like any guest computation. **Nested
signals** during a handler's own syscalls are surgery-on-top,
depth-bounded by masks exactly as on Linux. glibc's restorer and
`rt_sigreturn` never run: the frame's `pretcode` holds a plausible
value for introspection but is never consumed, because the translated
`ret` pops kisal's slot.

**`EINTR` and `SA_RESTART`, in Linux's order.** For a thread blocked
in a wait when an unblocked signal lands: cancel the wait, write
`-EINTR` into the saved `rax`, *then* do the surgery — so the handler
runs first, sigreturn restores (including that `rax`), and the
guest's post-syscall code sees `EINTR` after the handler. Linux's
ordering, bit for bit. `SA_RESTART` is cleaner here than on Linux:
kisal holds the syscall's saved arguments in the TCB, so the
sigreturn step, seeing a restart note, re-issues the syscall
kernel-internally and yields with the real result — no `ERESTARTSYS`
dance, because there is no return path to sneak it through.

**Defaults and synchronous signals.** SIGTERM/SIGINT default:
terminate — all TCBs torn down, exit status = signal, reported in the
`/iso/shutdown/complete` payload. SIGCHLD default: ignore.
**SIGPIPE** matters and is easy — it is *synchronous*, raised inside
the `write` syscall on a closed pipe where kisal already stands:
default → death; handled/ignored → `write` returns `EPIPE`.
SIGSTOP/SIGCONT park and unpark all TCBs; job control beyond that is
deferred. `sigsuspend`/`ppoll`/`epoll_pwait` mask-swap atomicity is
trivial here — kisal is the only actor, so there is no race window.
`signalfd` is absent from the trace; CPython's real mechanism is
`set_wakeup_fd` writing a pipe from the C handler, which this design
runs as an ordinary blocking-capable handler.
`ITIMER_VIRTUAL`/`ITIMER_PROF` synthesize from scheduler-charged
virtual time — imprecise, and said so.

**Hardware signals cannot exist, and that is a divergence, not a
shrug.** No faults means no SIGSEGV/SIGBUS: a wild pointer corrupts
silently instead of dying loudly, and address 0 is mapped, so null
derefs read garbage rather than crashing. Partial mitigations,
recorded: the baker and kisal keep the low 64 KB permanently
unallocated so null-page *writes* at least land on nothing, and a
debug-mode translator flag can emit explicit checks. **SIGFPE** is
subtler: x86 `div` by zero raises a catchable signal; translated
`i64.div` *traps*, and a wasm trap is not a catchable exception —
instance death. A flag-gated zero-check in the div translation
(branching to a kisal raise) buys conformance for a per-div branch;
off by default, named.

### setjmp/longjmp: the saved PC is already a continuation

Formerly the parked thorn; now a design, and it is assembly of
existing parts in the same sense the saturated tier is. The
load-bearing fact is verified in the code rather than hoped
(`src/translate.rs`, `ResumeSites`): under `--resume`, **every call
site stores a resume ID in its return-address slot**, and the chain of
those slots on the guest stack is a serialization of the frames above
any call.

**Work item zero, with a cost to measure first: the baker does not
build with resume on.** The plan has always said the container
pipeline builds resume-on; the built baker does not — `with_resume` is
called only from `src/main.rs` behind the flag, so every container
baked to date carries sentinels in those slots, not IDs. This is a
debt M7 already owes (threads cannot exist without it); longjmp moves
the bill forward rather than creating it. The price is a second body
per function — a code-section and engine-compile doubling at a
six-figure function count for CPython — and it gets measured before
anything below is built on it.

**setjmp needs nothing at all.** It is ordinary code: it stores the
callee-saved registers, `%rsp`, and the word at `(%rsp)` into the
`jmp_buf` — and that word *is the resume ID of its caller's
continuation*, a materialized, enterable continuation saved by code
that has no idea that is what it is doing. glibc's pointer mangling
(`xor %fs:0x30; rol $0x11` at save, the inverse at restore) is pure
integer arithmetic on the stored value, translated faithfully on both
sides, so it round-trips exactly; musl does not mangle. No shim, no
name-matching — which matters, because a stripped static binary has no
names to match.

**longjmp is one new arm plus machinery that exists.** It restores the
callee-saved registers and `%rsp` from the buffer (ordinary translated
stores into the globals, `val` into `rax`), then does `jmp *value`
with a resume ID as the operand. Today that reaches the exec map,
misses, and dies in `kisal_no_function_at`. The pieces of the fix:

- **Discrimination in the miss path, at zero hot-path cost.** The
  check lives where the exec-map lookup already fails: a missed value
  carrying the resume-ID tag is a longjmp; one without it stays the
  loud error. Function-pointer calls — the hot case — hit the map and
  never see the check. The one requirement is that the stored form of
  an ID be disjoint from every address, and the bit accounting is
  tighter than "a free bit": the slot holds bits 0–31, the entry
  bits 32–62 (`RESUME_ENTRY_MASK` is 31 bits), and `RED_ZONE_RESERVED`
  is bit 63 — so the tag comes out of the *entry field*, narrowing it
  to 30 bits, which is still absurd headroom. It is still free at the
  store: an ID is not one relocated constant but a relocated `i32`
  table index OR'd with a plain `i64` constant carrying the entry and
  flags (`src/translate.rs:1571`), and the tag rides that second
  constant exactly as `RED_ZONE_RESERVED` already does at syscall
  sites. The resume driver masks one more bit.
- **Frame abandonment is the blocking-syscall leave path, verbatim.**
  The wasm frames between longjmp and setjmp's caller must go, and the
  system owns exactly one tool that discards guest frames: the throw
  the seam raises when a syscall blocks, caught at `x86_run_thread`.
  The longjmp arm runs after the flush every transfer performs, and
  calls a seam-shaped helper: kisal sets the current thread's
  continuation to the given ID and returns the leave sentinel (a
  one-field kernel row — a *blocking syscall with a continuation
  override*), the helper throws, the catch schedules.
- **Re-entry is resumption, verbatim — once something loops.** The
  driver enters the saved ID; setjmp's call site has a resume entry
  because every call site does; the continuation reloads the machine
  from the globals, where longjmp's own translated code just put
  everything. The frames below setjmp's caller re-materialize lazily
  from the guest stack exactly as after any block. One thing must
  exist first: today's catch is single-shot — a throw with no exit
  recorded panics ("no scheduler to have parked it",
  `kisal/src/lib.rs`) — so pre-M7 this design needs the **degenerate
  boot loop**: the catch distinguishes "exited" from "runnable with a
  continuation" and loops back into `run_thread` for the latter. A few
  lines, subsumed by M7's real run loop; unnamed, the first longjmp
  unwinds straight out of the container.

**The continuation-override is load-bearing — do not "simplify" it
away.** The tempting deletion: longjmp restored `%rsp`, so read the ID
from the return slot at the setjmp site instead of carrying it through
the `jmp_buf`. That slot is *reused by every later call the same frame
makes at the same stack depth* — `if (setjmp(env)) return 1; g();`
writes `g()`'s call-site ID over setjmp's at the identical address,
and every real longjmp has this shape, because the caller must have
made the call that led to it. At longjmp time the slot holds the
continuation "as if the pending call returned" — entering it is
silently wrong control flow, the one failure class with no detector.
The `jmp_buf`'s saved word is the *only* surviving record of the
setjmp-site continuation, which is exactly why hardware setjmp saves a
PC at all. The stack drives everything *above* that first entry; the
first entry comes from the buffer.

**The cost model matches the workload.** A setjmp that never longjmps
is free — and that is the overwhelming case: libjpeg and libpng take
one on *every decode call* and longjmp only on corrupt input, and
`dlopen` takes one on every call (`_dl_catch_exception`) and longjmps
only on failure. An actual longjmp costs about one blocking syscall,
which is honest — it is semantically a context switch.

**Why it matters more than the Flask target suggests**: `dlopen`
returning `NULL` is normal control flow in real Python —
`ctypes.util.find_library` and the packages wrapping C libraries
*probe* by dlopening candidates and catching the `OSError` — and
setjmp/longjmp is the C ecosystem's exception mechanism (libjpeg,
libpng, Lua, Fortran runtimes). For "the target is any binary" it is
not exotic.

Edges, named now:

- **`siglongjmp` out of a handler composes only because delivery
  splices.** A throw must never cross a Rust frame, so this design
  works with M10's chain-surgery delivery — the handler is spliced
  into the chain and entered by the driver, never called from a kisal
  frame — and would break under a delivery that calls. This is the
  splice rule's second client. The mask restore is `rt_sigprocmask`
  from translated code, an existing row.
- **A stale `jmp_buf`** (longjmp after the saving frame returned) is
  UB natively and stays UB here: the entered continuation reads
  garbage slots and dies in a loud miss soon after — better than
  native offers.
- **`__longjmp_chk`'s** fortify check compares guest `%rsp` values,
  both faithful; it works untouched.
- **Resume off** (the relocatable/testing pipeline): slots hold the
  sentinel and the miss stays loud. The constraint costs nothing —
  containers always build with resume on — but it is a constraint.

Tests, corpus-ladder style: a same-frame round trip; longjmp across N
frames with distinguishable callee-saved values checked after; the
canonical idiom — setjmp, then a *call* from the same frame that
longjmps back — which is the test that kills the read-the-slot
simplification above (the slot holds the pending call's ID, and the
run must take the setjmp arm, not the returned-normally arm); longjmp
in a tight loop — **the wasm stack must not grow**, which is the throw
earning its keep and the test that kills the naive
call-the-continuation design (that one leaks a frame chain per
longjmp); a longjmp whose target frame was itself entered by a resume
body — resume bodies' call sites push IDs too, so it should compose,
but it is the case where "the frames re-materialize lazily" does the
most work, and it gets a test rather than an inference; both libcs,
since one mangles and one does not; and the gate that reopened this
design — `dlopen` of a missing library returning `NULL` with
`dlerror` text, differential against native. It also unblocks the
parked setjmp case in the test ledger.

### x87 and MMX

The target is any x86-64 binary, so the scope is the full x87
instruction set — with MMX, which aliases its register file. What any
one workload needs decides only the *order* things get built, never
whether they do. The evidence for the order: a `gcc -static -O2` hello
carries `fldt`, `fld`, `fxam`, `fucomi(p)`, `fnstsw`, `fnstenv`/
`fldenv` (glibc's `feholdexcept`), `fxch`, `fnstcw`, `fabs`, `fwait` —
fenv machinery and `__printf_fp`'s long-double classification, no
arithmetic. musl leans much harder: `floatscan.c` and `fmt_fp` do
their arithmetic in `long double` C, so compilers emit the whole data
path — loads and stores at all three widths, the arithmetic family
with their pop variants, integer converts through the `fnstcw`/`fldcw`
truncation dance (baseline x86-64 has no `fisttp`) — and musl's
x86-64 `expl`/`logl`/`atanl`/`fmodl` are hand-written x87 asm using
`f2xm1`, `fyl2x`, `fyl2xp1`, `fpatan`, `fscale`, `frndint`, and
`fprem` with the `fnstsw`-test-C2 partial-remainder loop.

**Soft emulation, in a fourth body.** A workspace crate — `x87/`, Rust
compiled to a `wasm32-unknown-unknown` staticlib like kisal — defines
one typed helper per instruction shape, and the translator lowers each
x87 instruction to one call, following `translate_syscall`'s pattern:
undefined symbols the staticlib defines, signature disagreements
caught at link. The property that makes this cheap is that **helper
calls are intrinsics, not calls**, with respect to every existing
discipline:

- They never touch the register-file globals — the crate is Rust and
  cannot name them, and by policy it never calls the seam accessors.
  No flush before, no reload after; promotion is untouched.
- They never block, never syscall, never throw. Not resume sites,
  invisible to `--resume` and the scheduler.
- Their effects on modelled state go through the translator's normal
  paths: `fucomi` returns packed ZF/PF/CF for the translator to unpack
  into its promoted flag storage; `fnstsw ax` returns the status word
  and the translator writes AX. `fcmovcc`'s condition is evaluated
  translator-side from the promoted flags and passed as an argument.

Operands: f32/f64/integer memory operands pass **by value** — the
translator emits the load or store with its existing addressing
machinery, so every addressing semantic stays in one place and the
helpers stay pure. Only the 80-bit and environment-image operands pass
**by address**; the crate shares linear memory exactly as kisal
already proves works.

**State: zero new wasm globals.** Everything lives in one static in
the crate's own data segment: FCW, FSW (TOP, C0–C3, sticky exception
flags), the abridged tag byte (bit *i* = physical register *i*
occupied, the FXSAVE convention), and eight registers stored as
explicit-significand pairs — `{ significand: u64, sign_exponent: u16 }`,
the hardware's own explicit-integer-bit format. TOP is **runtime
state, deliberately**: resolving `ST(i)` at translation time is the
classic FP-stack allocation problem and it loses here — x87 state
crosses function boundaries (a `long double` return *is* a nonzero
entry depth), crosses joins at different depths, and `fincstp`/`fxch`
make depth path-dependent. Runtime indexing costs an AND on a path
that is cold by construction.

FCW is honored, not decorative: rounding control steers every
rounding, and precision control is implemented in the arithmetic core
(it is how extended hardware computes correctly-rounded doubles).
Exception *flags* are recorded faithfully in FSW, including ES;
exception *traps* are not modelled in the first version — everything
behaves as-if-masked, and an `fldcw` that unmasks is recorded but
arms nothing. The upgrade has a designed shape, because x87 traps are
*deferred* on real hardware too — reported at the next x87 instruction
or `fwait` via ES, not at the faulting one. So the check is a cheap
test at helper entry, and delivery is SIGFPE through kisal's signal
machinery at the existing check points; imprecise delivery is faithful
delivery here. Real binaries do arm these (`gfortran -ffpe-trap`,
debugging builds), so the FSW/ES bookkeeping is scrupulous from day
one and `fwait` becomes "check ES" the moment delivery exists.

`fnstenv`/`fldenv`/`fnsave`/`frstor` are in scope from the start — the
histogram shows glibc's fenv on the live path — rendering the 28-byte
protected-mode env image (FIP/FDP as zeros, FXSAVE-precedented,
nothing in either libc reads them) and the 108-byte fnsave layout,
including `fnstenv`'s side effect of masking all exceptions in the
live FCW, which is the entire reason `feholdexcept` uses it.
`fxsave`/`fxrstor` wait for the FXSAVE render the signal frames need
anyway; the instruction form is a two-writer render — the crate fills
the x87 portion, generated code fills XMM/MXCSR from the globals —
the one place the crate and the seam cooperate on a single image.

**Save, load, reset.** Fork and snapshot are free: the state is linear
memory, so the existing memory snapshot carries it and `--resume`
changes not at all. The crate exports `x87_save(ptr)`/`x87_load(ptr)`
and `x87_image_size()` for M7's context switch — deliberately a
*second* image beside `x86_save_machine`'s, because that layout is
zaqaru-generated and this one is crate-owned, the same ownership
argument the seam makes about `set_rsp`. `x87_reset()` is the FNINIT
state (FCW `0x037F`, all tags empty) for `execve`; the static
const-initializes to the same, so boot needs nothing. M10's ucontext
fpstate gets `x87_render_fxsave`/`x87_load_fxsave`, filled genuinely
rather than zeroed, since the real state is on hand.

**Precision: true 80-bit for the data path, and why.** The registers
hold genuine extended precision, and add/sub/mul/div/sqrt/`fprem`/
`frndint`, the converts, and the compares are real extF80 softfloat —
not f64-backed. Two reasons: musl's `floatscan`/`fmt_fp` were compiled
against `LDBL_MANT_DIG == 64` and their correct rounding assumes that
precision exists; and the project's entire testing methodology is
bit-exact differential comparison against native runs — f64 backing
would collapse every long-double differential into tolerance mush.
The implementation is smaller than it sounds with `u128` on hand, and
it has a luxury most softfloat authors lack: **the host has a real
x87**, so the crate's native tests drive millions of random 80-bit
patterns through `asm!`-wrapped hardware ops and demand bit-identical
answers — every op, all four rounding modes, both precision-control
settings, denormals, pseudo-denormals, NaN payloads.

The genuinely transcendental ops (`f2xm1`, `fyl2x`, `fyl2xp1`,
`fpatan`, and later `fsin`/`fcos`/`fsincos`/`fptan`) are the one place
bit-exactness is not even well-defined: Intel and AMD disagree in the
low bits, and Intel revised its own error bounds in the 2014 `fsin`
episode — there is no "the hardware" to match. The defensible target
is correctly-rounded-or-nearly (≤1 ulp of extended), which is better
than hardware and just as legal; the eventual mechanism is
double-double cores on the extF80 primitives. The first version
computes them f64-backed with exact special cases, and the oracle
*measures* the divergence in ulps rather than assuming it acceptable.
Everything that merely looks transcendental but is exact — `fprem`,
`fprem1`, `fscale`, `frndint`, `fxtract`, `fsqrt` — gets real extF80
treatment from the start, including `fprem`'s partial-result protocol:
C2 on incomplete reduction, quotient bits in C0/C3/C1, which is what
`fmodl`'s loop is made of and what the trig ops reuse for |x| ≥ 2⁶³.

**MMX** is part of the scope, not a courtesy: it is architecturally
guaranteed on x86-64, so CPUID must report it — it cannot be curated
away like AVX — and random binaries ship hand-written MMX. The
explicit-significand register layout is what makes it cheap: `mm0..7`
*are* the significands, an MMX write sets the exponent field to
all-ones and the tag to valid, `emms` empties the stack — field
access, not bit surgery. It sequences after the scalar core because
the core is its prerequisite, not because it is optional.

**The tier table is the roadmap.** Every helper carries its current
tier — bit-exact, correctly-rounded, f64-backed, or refused — in one
table in the crate, which the differential harness reads to choose
bit-exact versus tolerance comparison per op. "Full emulation" is
that table driven to done, a checklist rather than archaeology.

## Priority order

Dictated by the trace, not by difficulty:

1. **VFS** — the baker, the index, and kisal's resolution loop: 80% of
   all traffic, pure guest, unblocks interpreter start.
2. **TLS (`%fs`) translation** — small, but blocks literally everything
   glibc.
3. **Threads on resume + futex** — the actual concurrency model.
4. **mmap** (eager-copy VMA layer) and `brk`.
5. **Sockets/epoll + the single host wait.**
6. **Signals** — chain-surgery delivery via `signal_dispatch`; the
   Flask path needs SIGTERM/SIGINT, `EINTR`, and `set_wakeup_fd`'s
   pipe write from a handler.
7. **fork** — built first, needed last. Not wasted: the resume machinery
   it forced into existence is the scheduler.
8. **Dynamic linking** (linked-ELF mode, prelink, shadow GOT) — kept
   off the critical path by tier one: the first end-to-end target is a
   statically-built image, and the dynamic tier lands after it works.

## Open questions

- ~~Wasm exception handling in wasmtime~~ — **settled, and the
  fallback is not needed.** wasmtime 48.0.1 runs the standardized
  `try_table`/`throw` form with no engine flag, unwinds through a frame
  that has no catch, and leaves mutable globals written before the
  throw intact on the catch side. It *rejects* the legacy `try`/`catch`
  pair that clang 18 still emits, so the emitter emits the
  standardized form — which costs nothing, since we generate the shim's
  bytes ourselves. `wasm-ld` 18 links a tag across objects (symbol kind
  4, relocation `R_WASM_TAG_INDEX_LEB` = 10, tag section id 13). All of
  it is exercised inside our own link by `tests/kernel_seam.rs`, which
  raises the yield under the catch and asserts the register globals are
  what the throwing code left. The unwind there crosses no intervening
  frame — that waits for M7's first real block.
- The poll's measured cost: the dispatcher gap is measured; the
  back-edge poll itself (load + branch, decimated) is not yet. Measure
  on `bench_integer` — the tight-loop worst case — before preemption
  defaults on.
- Dispatcher cost at scale: the 1–2% number comes from small kernels;
  a `br_table` over the hundreds of blocks of something like CPython's
  eval loop is unmeasured, and it is the frame that suspends most.
- Writable `MAP_SHARED`: dirty tracking vs. write-back-on-sync, when
  something needs it.
- Metadata enforcement: the bake preserves everything (ownership,
  setuid bits, `security.capability`); whether kisal ever *honors* any
  of it — permission checks, capability grants — is deliberately
  undecided. The preserved bits keep every option open.
- ~~setjmp/longjmp~~ — **designed**; see "setjmp/longjmp: the saved PC
  is already a continuation". setjmp is ordinary code; longjmp is a
  tagged constant, one arm in the exec-map miss path, a one-field
  kernel row, and the existing leave/resume machinery. What remains is
  the build and its corpus ladder, not a design pass.
- The syscall dispatch itself: direct call per rewritten site when the
  number is a constant in `rax` (it almost always is), `br_table` in the
  kernel otherwise — measure whether the distinction matters.
- ~~Kernel language and residence~~ — **settled by construction.**
  kisal is Rust compiled to `wasm32-unknown-unknown` as a staticlib and
  welded in by `wasm-ld` at image link time, which keeps syscalls at
  wasm-call cost and makes the seam's signature a link-time check. The
  structfs crates cross-compile to that target unchanged, so the
  kernel's downward face can be the real `LLReader`/`LLWriter` rather
  than a re-implementation, and its logic is unit-tested natively as
  ordinary Rust.
- The deferred performance list, in one place (working first, then
  benchmark): clock reads through path routing on a hot path; per-call
  `Bytes` allocation at the boundary; send/recv copy granularity; the
  core-module-in-a-component wrapping for featherweight.
- Isotope spec asks, from the mapping table: ns-typed time paths, a
  kernel-events path with a deadline, the `/iso/net` broker, guest
  console paths, exit status in the `shutdown/complete` payload, and
  `/iso/proc/spawn`.
