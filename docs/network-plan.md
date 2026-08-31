# The network: sockets in the arena, and the edge at `/iso/net`

Status: **built** — written 2026-08-30 as a design and implementation
plan, before any socket code existed; N0 through N5 were built the same
day and the demo answers. What is *not* built is named in section 12 and
in the pothole list: egress above all — a guest connecting anywhere off
`127.0.0.0/8` gets `ENETUNREACH`, which is the truthful answer for a
namespace holding only `lo` and not a working outbound path — along with
`SCM_RIGHTS`, `MAP_SHARED` across `fork`, and DNS. A concurrency defect
found afterwards is recorded in [performance.md](performance.md): four
clients at once measure *worse* throughput than one, which no arrangement
of the pump explains.

The text below is the plan as written.

`container-plan.md`'s "Sockets and epoll"
section remains the design authority for socket *semantics* (the readiness
model, the epoll rules, the half-close matrix, the no-packets decision);
this document is the plan for building them under the interpreter, where
several of that section's costs are repealed, and section 11 lists the
amendments to fold back. The demo it aims at: an OCI image carrying
**nginx + gunicorn + django**, baked to one `.wasm`, served by
`zaqaru-run`, answered by `curl` — and later by a browser harness
supplying the same `/iso` API.

## Outline

1. **The demo, and what it proves** — why this stack, and what it
   exercises that nothing so far has.
2. **Two networks, and only one crosses the host boundary** — the
   load-bearing split: loopback is kernel state, the edge is a stream the
   host terminates. No packets, anywhere.
3. **The loopback arena** — a socket is a pipe with addressing: the
   objects, the port table, `AF_UNIX`, and the prefork bill the
   interpreter repeals.
4. **Readiness, and where sockets plug in** — one new arm in
   `readiness_of`, one generalized transfer, and nothing else, because
   the rules were built for this.
5. **The edge: the `/iso/net` protocol** — the store paths, the event
   stream, the pump, and how determinism survives a network.
6. **Waiting without spinning** — the blocking read, which also retires
   the timeout spin; what `Deadlocked` becomes when a listener exists.
7. **The wasmtime runner** — `NetStore`, `-p`, the live console, and
   how a server shuts down.
8. **Running the demo** — the commands, the `curl`, and the whole path
   of one request traced end to end.
9. **The syscall surface, priced by the stack** — what nginx, gunicorn
   and CPython actually call, row by row, with the known potholes named.
10. **The browser harness, briefly.**
11. **Amendments to `container-plan.md`.**
12. **Gates and milestones** — N0 through N5, each with acceptance and a
    negative control.
13. **Risks and open questions.**
14. **Pitfalls index** — seeded.

## 1. The demo, and what it proves

nginx + gunicorn + django is the classic deployment stack, and it is
chosen for what it exercises, not for sentiment. It is a process *tree*
(an init, nginx's master and worker, gunicorn's master and worker) built
by `fork` and `execve`; nginx is an epoll-driven event loop and gunicorn
a poll-driven prefork server, so both readiness disciplines run at once;
the two halves talk over a socket only they can see, and the outside
world talks to nginx over a socket only the host can deliver; signals
manage the workers; and the whole thing is a *server*, which means the
container spends its life blocked — the one thing no workload so far has
asked the design to do cheaply.

Everything below the sockets already exists: the process table
(`kisal/src/system.rs`), pipes in a shared arena (`kisal/src/pipe.rs`),
`poll`/`epoll` with the readiness rule (`kisal/src/poll.rs`), signals,
and the `subprocess.run` verification that a forked CPython can be
captured through a pipe and reaped. The network is the missing subsystem,
and this plan is scoped to exactly it.

## 2. Two networks, and only one crosses the host boundary

The split the whole design hangs on: **nginx ↔ gunicorn is loopback, and
loopback never leaves the kernel.** Both endpoints are guest processes,
so a connection is what a pipe already is — rings in a kernel-owned arena
the whole process tree shares, reached through per-process descriptor
tables. `connect(127.0.0.1:8000)` looks up a port table, builds a
connected pair, queues one end on the listener's accept queue. No host,
no mount, no tape entry.

**curl ↔ nginx is the edge, and the host terminates it.** The runner
accepts a real TCP connection and hands kisal a *stream* — "a connection
arrived on your port 80; here are bytes; here is end-of-file" — through
the same two `ll-store` imports everything else uses. kisal materializes
it as an accepted socket whose rings the kernel feeds from the store.

`container-plan.md`'s "no packets, anywhere" carries over whole and gets
stronger: kisal has no TCP state machine, no IP layer and no checksums,
because every connection either has both ends in the guest (a kernel
object) or one end at the host (which speaks real TCP on our behalf).
gVisor built a userspace netstack for this; we refuse one, on the
grounds the plan already recorded — nothing ever becomes a packet, so
there is nothing for a netstack to do.

The consequence for capability control shifts one notch and is recorded
in section 11: loopback needs no mount, because it is kernel state — a
container with no `/iso/net` mount has `lo` and nothing else, which is
exactly a Linux netns with no interfaces attached. The mount gates the
*edge*, and the mount's configuration (the port map) is the firewall,
exactly as `docker -p` is.

## 3. The loopback arena

**A socket is a pipe with addressing, an accept queue, and a half-close
matrix.** The arena shape is `pipe.rs`'s, kept deliberately: one
`Sockets` table behind an `Rc` on the kernel, cloned by `fork`, kept by
`execve`, indexed by descriptors that stay `Copy`
(`Backing::Socket { socket: u32 }` beside `Backing::Pipe` in
`kisal/src/fd.rs`). Three kinds of entry:

- **A stream endpoint** is two ring indices — receive and transmit —
  because a socket is a bidirectional pipe and nothing more. A
  connection is two endpoints crossed over one pair of rings. The rings
  are `pipe::Ring`'s deque with the same capacity discipline;
  `SO_RCVBUF`/`SO_SNDBUF` size them honestly, per the plan.
- **A listener** is an accept queue of endpoint indices, a backlog
  bound, and its binding (address, port — or a VFS path for `AF_UNIX`).
- **The port table** lives *in the arena*, because it is netns state and
  a netns spans processes — which under the shared-`Rc` arrangement is a
  sentence, not a subsystem. `SO_REUSEADDR` affects its rebind rule and
  nothing else.

`connect` to a bound loopback port: make a ring pair and two endpoints,
push one on the listener's queue, done — the accepting process finds it
on its next `accept4`, and readiness says so before that. `socketpair`
is the same minus the table. `AF_UNIX` `bind` creates the filesystem
node through the VFS (which is what makes glibc's
`/var/run/nscd/socket` probe answer `ENOENT` for free, as the plan
observed) and keys the same machinery by vnode instead of by port.

**The reference census extends, direction by direction.** A pipe end
counts readers or writers; a stream endpoint counts references, and
carries two shutdown bits, because `shutdown(SHUT_WR)` is a half-close
that `close` is only the both-halves case of. The rules are the pipe's,
applied per direction: a reader whose peer has no writing references
left drains then reads zero; a writer whose peer has no reading
references left gets `EPIPE` and the synchronous `SIGPIPE` (suppressed
per-call by `MSG_NOSIGNAL`, which real servers pass). And the accounting
follows `dup`, `fork`, `execve` and `close` through the same
census-before-and-after that `file.rs` already runs for pipes — five
call sites that cannot each remember a direction is exactly the bug the
census was built to make impossible, and it generalizes by widening what
is counted, not by adding sites.

**The prefork bill is repealed.** The plan's "multi-process honesty
clause" priced sockets as the subsystem where fd hoisting costs most — a
prefork server's hot accept path running through a host-side router.
Under the interpreter there is no router: gunicorn's workers inherit the
listener the way forked processes inherit a pipe, the accept queue is
arena state every process reaches through its own kernel, and a worker's
`accept4` is a queue pop. The plan's analysis of *what* is shared stands
verbatim; the mechanism collapsed, exactly as it did for pipes.

## 4. Readiness, and where sockets plug in

`poll.rs` was built on one rule — readiness is computed from kernel
state, never from any process's memory, because the question is asked
while choosing which process to run — and sockets were the reason the
rule had to be that strict. So the integration is small and its
smallness is evidence the shape was right:

- `readiness_of` (`kisal/src/poll.rs:233`) gains a `Backing::Socket`
  arm: a stream endpoint answers `IN` when its receive ring has bytes or
  its read-side peer is gone (plus `RDHUP`/`HUP` per the matrix), `OUT`
  when its transmit ring has room or errored; a listener answers `IN`
  when its accept queue is non-empty. All of it is arena state. `epoll`,
  `poll`, `select` (a `poll` in older clothes, one row) and blocking
  reads then work on sockets with no further design, including the
  registered-on-the-description rule and the inherited-epoll refusal.
- `State::Transferring` generalizes: `pipe::Transfer` names a pipe and
  an end; it becomes a transfer against *a ring* — which both pipes and
  sockets hold — so `resume_transfers` in `kisal/src/run.rs` and the
  parked-write completion rule ("on the parked process's own turn,
  never the waker's") carry over unchanged. `recvfrom`/`sendto`/
  `recvmsg`/`sendmsg`/`read`/`write` on a stream all lower to it.
- Non-blocking `connect` on loopback completes immediately (the peer is
  a queue push), so `EINPROGRESS` arises only at the edge, where the
  broker's async shape gives the faithful sequence for free — plan text,
  still true here: `EINPROGRESS`, then writability plus `SO_ERROR`.
- `System::stall` learns the socket states, because a hung server must
  say "parked in `accept4` on listener 3 (queue empty, port 80, edge)"
  for the same reason it already narrates pipes.

## 5. The edge: the `/iso/net` protocol

The host boundary stays two imports. `kisal/src/abi.rs` states the
rule — "adding a capability later is adding a mount, never adding an
import" — and the edge honors it: everything below is paths read and
written through `ll_read`/`ll_write`, served by whatever the embedder
mounts at `/iso/net`. No mount, no edge.

**Registering a listener.** When the guest binds and listens on a port,
kisal writes `/iso/net/listen` with the port number. The store consults
its port map (section 7): a mapped port opens a real host listener and
answers a result path `/iso/net/listener/{n}`; an unmapped port answers
a result path that says *loopback-only*, and kisal records the listener
as arena-only. The guest cannot tell — `bind` and `listen` succeed
either way, exactly as they do under `docker` without `-p`.

**The event stream.** One path, `/iso/net/events`, read by kisal and
answered with a batch of newline-separated events since the last read
(`ok(none)` when there are none):

```
open {conn} {listener} {peer-address}
data {conn}
room {conn}
eof {conn}
```

`open` materializes an accepted endpoint in the arena, bridged to
`{conn}`, queued on the guest listener — from there it is
indistinguishable from a loopback connection, which is the design's
seam: **everything above the rings is one code path.** `data` and
`room` are hints to pump; `eof` marks the receive side's peer gone. The
peer address is recorded on the endpoint so `accept4` and
`getpeername` answer it — faithful live, and faithful under replay,
because it arrived on the tape.

**The pump.** Bytes cross through per-connection paths: kisal reads
`/iso/net/conn/{j}/rx` (answered with up to as many bytes as the
endpoint's receive ring has room for — the ring's room is stated in the
path, so the host never over-delivers and real TCP backpressure reaches
all the way to curl), and writes drained transmit-ring bytes to
`/iso/net/conn/{j}/tx`. Guest-side `close`/`shutdown` write
`/iso/net/conn/{j}/ctl`. The pump is kernel code touching only kernel
state — rings, never guest memory — so it may run on *any* turn without
violating the address-space rule. It runs at two deterministic points:
when the process rotation completes a slice (bounding event latency for
a busy container to `SLICE × QUANTUM` retired instructions — tens of
milliseconds at measured floor speeds, and a named knob), and when the
system would otherwise idle (section 6).

**Determinism, restated for a network.** A live network is
nondeterministic; the design's claim was never that the world is
deterministic, it is that everything nondeterministic enters through the
store — which is the tape. Every event batch and every `rx` payload is
a store answer at a point that is a pure function of execution and prior
tape; record the answers and a replay reproduces the run bit for bit,
scheduling included, exactly as the clock and the random seed do today.
A *served HTTP session, replayed exactly* falls out of this and is demo
material in its own right.

## 6. Waiting without spinning

`poll.rs`'s header names the awkward case: the boundary's two imports do
not wait, so a container whose only pending work is a timeout spins. A
batch workload tolerates that; a server *is* that case for its whole
life, and a demo that pegs a core while idle is not servable.

The fix keeps the boundary at two imports: **one designated path whose
read is allowed to take time.** kisal reads `/iso/net/wait/{ms}` only
when nothing in the container is runnable and at least one thread is
parked on an edge object or a deadline; the store blocks up to `{ms}`
milliseconds for a host event and answers the same batch
`/iso/net/events` would, or `ok(none)` on timeout. kisal computes
`{ms}` from the earliest guest deadline (via the same monotonic read
`poll.rs` already does), or passes a large cap when there is none. A
blocking read is observationally a slow store — nothing about the ABI
changes, wasmtime's host function simply doesn't return yet — and in the
browser it is the one place JSPI or a worker-side `Atomics.wait` is
needed (section 10).

Two consequences worth their own sentences. First, **the timeout spin
dies as a side effect**: a `ppoll` with a deadline, a timed futex wait
(the named fault from V3), gunicorn's one-second `select` heartbeat —
all become bounded sleeps in the host, because "wait for events or
`{ms}`" with no events expected *is* a sleep. Second,
`Exit::Deadlocked` gains the third verdict section 5 of the process
work foreshadowed: nothing-runnable with an edge listener or a deadline
outstanding is not a deadlock, it is a server at rest, and
`System::run` waits instead of returning. Nothing-runnable with
*neither* remains the honest deadlock it is today, stall report and all.

## 7. The wasmtime runner

`runner` grows a `NetStore` mounted at `/iso/net`, and `zaqaru-run`
grows the flag that configures it:

```
zaqaru-run hello-django.wasm -p 8080:80
```

`-p HOST:GUEST`, repeatable, is the port map — the docker convention,
living host-side as mount configuration, which is what the capability
model always said the firewall was. Internally `NetStore` holds real
`std::net` listeners and connections on host threads that push events
and bytes into a queue the store answers from; the wait read blocks on
that queue's condvar with the deadline. Threads are fine here — the
guest never sees them; what the guest sees is one serialized stream of
store answers, and the serialization is what keeps the tape one stream.

Two host-side quality items ride along, both invisible to the guest:
the console `Sink` learns to **tee writes to the runner's own
stdout/stderr as they arrive**, because a server's logs read back after
exit (`zaqaru-run.rs`'s current shape) is a diagnostic for a program
that exits, and a server doesn't; and **Ctrl-C** installs a handler that
writes `/iso/shutdown/requested`, which kisal synthesizes into a
process-directed `SIGTERM` to init exactly as `container-plan.md`'s
signals section specifies — so a demo ends the way `docker stop` ends,
with nginx and gunicorn shutting down cleanly and the exit status
arriving at `/iso/shutdown/complete`.

## 8. Running the demo

The image is an ordinary Dockerfile — Debian-based for glibc, which is
the tested libc: `python:3.12-slim`, `apt-get install nginx`, `pip
install gunicorn django`, a hello-world Django project, an nginx config
proxying `/` to `127.0.0.1:8000` (or `unix:/run/gunicorn.sock`; both
are the same arena object), and an init script as the entrypoint,
because an OCI image runs one command and this container is a tree:

```sh
#!/bin/sh
gunicorn --workers 1 --bind 127.0.0.1:8000 hello.wsgi &
exec nginx -g 'daemon off;'
```

Then the whole pipeline, on the machine of anyone with the repo:

```
docker build -t hello-django demo/hello-django
docker save hello-django -o hello-django.tar
cargo run --release --example bake-vm -- hello-django.tar hello-django.wasm
zaqaru-run hello-django.wasm -p 8080:80
```

The bake is the VM bake: seconds, no translation, the image's own
entrypoint and environment read from the `docker save` config exactly
as `bake-vm` already does. And then, from another terminal:

```
$ curl http://localhost:8080/
<h1>Hello, world!</h1>
```

What that one command traverses, end to end: curl opens a real TCP
connection to the host's port 8080, which `NetStore`'s listener thread
accepts and reports as `open` on the event queue. kisal — parked in the
wait read, using no CPU — wakes with the batch, materializes the
endpoint on nginx's port-80 listener, and nginx's `epoll_wait` reports
the listener readable. nginx `accept4`s, reads the request from the
receive ring (the pump fed it from `conn/{j}/rx`), and connects to the
upstream — a loopback connect that never leaves the arena. gunicorn's
worker wakes from `poll`, accepts, and hands Django the request; Django
renders the page; the response crosses back through the same two hops —
arena rings to nginx, transmit ring to the pump, `conn/{j}/tx` to the
host socket — and curl prints it. Every byte of application logic ran
interpreted inside one `.wasm`; the host relayed streams and did
nothing else. Ctrl-C in the runner's terminal sends the container its
SIGTERM and the tree exits the way it would under docker.

The browser version of the same demo differs only in who relays:
section 10.

## 9. The syscall surface, priced by the stack

What the demo binaries actually call, split by cost. Gate N0 replaces
this a-priori list with a traced one before N1 starts, per the house
method — the worklist is driven by real programs, not by guesses.

**Rows that are the loopback arena (N1–N2):** `socket`, `bind`,
`listen`, `accept4` (with `SOCK_NONBLOCK|SOCK_CLOEXEC`), `connect`,
`socketpair`, `shutdown`, `getsockname`/`getpeername`,
`recvfrom`/`sendto`/`recvmsg`/`sendmsg` (lowering to the generalized
transfer; `MSG_NOSIGNAL`, `MSG_PEEK`, `MSG_DONTWAIT` as flags on it),
`setsockopt`/`getsockopt` (`SO_REUSEADDR` into the rebind rule;
`SO_ERROR` for non-blocking connect; `TCP_NODELAY`/`SO_KEEPALIVE`
recorded no-ops, there being no TCP to tune; `SO_RCVBUF`/`SNDBUF`
sizing rings; `SO_RCVTIMEO`/`SNDTIMEO` as a transfer racing a
deadline — CPython's `socket.settimeout` machinery needs these),
`select`/`pselect6` (a `poll` row), and the `ioctl`s `FIONBIO` (nginx
sets non-blocking this way, not through `fcntl`) and `FIONREAD` (a
ring-length read).

**Rows that are cheap and worth building on sight:** `sendfile` —
nginx's static path, and in this kernel a file-to-ring copy with the
transfer's own parking; `eventfd2` — a counter in the arena with
pipe-shaped readiness, named by the plan as in-guest, reached by nginx
only under thread pools but cheap enough to not wait for that.

**The named potholes, so nobody meets them as surprises:**

- **`SCM_RIGHTS`** — descriptor passing over `AF_UNIX`. nginx's master
  passes worker channel sockets to *other* workers with it, so it is
  reached the moment `worker_processes` exceeds one. The demo pins
  `worker_processes 1`, which passes nothing; the row's design is
  written now because the arena makes it small: a passed descriptor is
  a `(Backing, flags)` value carried in the message queue, holding a
  census reference while in flight, materialized into the receiver's
  table at `recvmsg`. Built when N5's config wants a second worker, not
  before.
- **`MAP_SHARED` anonymous memory across `fork`** — nginx always maps a
  small shared region at event-module init (the connection counter and
  the accept mutex). Under one-address-space-per-process a shared page
  is state two processes share, which the copied address space breaks.
  The demo is safe by configuration — accept_mutex defaults off since
  nginx 1.11.3, and with one worker the counter has one writer and no
  reader — but the honest design is named now: shared ranges are
  enumerable in the VMA tree, and `activate`/`deactivate` can copy them
  through a kernel-held backing at the switch, which under this
  scheduler is *sequentially consistent at switch granularity*, because
  processes never truly interleave. A row for later, with that sketch
  and its stated limit (a lock held across a park is held across a
  switch, and the design must say so when built).
- **`getaddrinfo` and netlink** — deferred exactly as
  `container-plan.md` defers them, and the demo stays inside the
  deferral by using numeric addresses and unix sockets throughout.
  glibc short-circuits numeric hosts before its `check_pf` netlink
  dance; the day a config says `localhost`, `/etc/hosts` resolution may
  reach `RTM_GETADDR`, and the plan's gVisor-shaped minimal answer is
  the recorded fix.

**Already built, listed so nobody re-plans them:** the process tree the
init script needs, pipes for `subprocess`, `poll`/`epoll` and their
famous semantics, signal routing for `SIGTERM`/`SIGCHLD`/`SIGHUP`, and
the overlay for nginx's logs and pid files.

## 10. The browser harness, briefly

The protocol is the point of section 5: the browser host implements the
same `/iso/net` mount, and nothing inside the module changes. A Service
Worker intercepts fetches to a virtual origin and forwards
request/response streams as `open`/`rx`/`tx` on a connection — Django's
actual bytes rendered in a real tab — with a page-level request box as
the simpler fallback. The wait read is the one mechanically different
piece: a synchronous import cannot block the main thread, so the engine
runs in a Worker and the wait is an `Atomics.wait` on a
`SharedArrayBuffer` the page posts events through, or JSPI where it
ships. Egress (`connect` to the wider internet) has no fetch analogue
worth pretending about; it stays a loud error in the browser, and the
demo needs none.

## 11. Amendments to `container-plan.md`

To fold back into the sockets section when this plan starts landing,
per the rule that a disagreement is resolved in both documents:

1. **Loopback needs no mount.** The plan's "no `/iso/net` mount means
   no network at all" becomes "no mount means no *edge*"; loopback is
   kernel state under the interpreter, present the way `lo` is present
   in an empty netns. The capability statement survives aimed at what
   actually crosses the boundary.
2. **The router is repealed.** The multi-process honesty clause priced
   listeners and the port table moving to a host-side router at fork.
   The shared arena dissolves it — same repeal as pipes, recorded so
   the largest-bill warning stops being quoted.
3. **The readiness bridge simplifies.** The plan's guest-side readiness
   cache with transition-derived edges assumed host events arriving
   against state the kernel could not see. Here host bytes land in
   arena rings and readiness is computed from the rings, so `EPOLLET`
   edges derive from ring transitions with no second cache. The
   ET-discipline warning itself stands.
4. **The single kernel wait has a concrete shape**: the blocking read
   on `/iso/net/wait/{ms}`, two imports unchanged.

## 12. Gates and milestones

Gate first, M0-style: hours, a verdict line, a reroute.

**N0 — trace the actual stack. DONE, 2026-08-30.** The image is
`demo/hello-django`, the tracing variant is `Dockerfile.trace`, the run is
`demo/hello-django/trace.sh`, and the extracted surface is
`demo/hello-django/baseline/n0-surface.txt` — which is both the worklist
and the baseline N5 diffs against. **Five processes, 88 distinct
syscalls, 9,365 calls**, boot through two `curl` requests to a `SIGTERM`
shutdown every process exits 0 from. **58 of the 88 already have rows in
kisal; 24 do not.** The original text follows the verdict.

Two mechanical notes, because both cost a run to find. The shutdown has
to be signalled from *inside* the container: `docker stop` signals pid 1,
which is `strace`, and the tracer dies taking the shutdown half of the
trace with it. And the tracing image is separate from the demo image on
purpose — a tracing tool in the image is a syscall surface of its own
inside the measurement.

**The 24 missing rows, and what they are.** Twelve are the sockets this
plan is about (`socket` has a refusing row already): `bind`, `listen`,
`accept4`, `connect`, `socketpair`, `shutdown`, `recvfrom`, `sendto`,
`recvmsg`, `sendmsg`, `getsockname`, `getpeername`, `setsockopt`,
`getsockopt`. One is readiness (`pselect6` — and note it is `pselect6`,
not `select`). One is an in-guest object section 9 already listed
(`eventfd2`). **The remaining eight were not in section 9 at all**, and
they are the gate's actual finding:

- **`chown`, `umask`, `setuid`, `setgid`, `setgroups`, `prctl`
  (`PR_SET_DUMPABLE`) — nginx drops privileges.** The master `chown`s
  `/var/lib/nginx/{body,fastcgi,proxy,scgi}` to uid 65534 and the worker
  `setgid`/`setgroups`/`setuid`s to it. `container-plan.md` answers
  `getuid` and friends with a constant zero on the grounds that "a
  container has one user and it is the one that started it" — which was
  true until a program in the container changed users. The honest
  minimum is to record the ids and answer from them; whether any
  permission check *enforces* them is a separate decision that must be
  stated rather than left implied, because nginx will believe it has
  dropped privileges either way.
- **`rt_sigsuspend`, `clock_nanosleep`, `setitimer` — three more wait
  shapes.** Section 6 assumed one ("gunicorn's one-second `select`
  heartbeat"). The real stack waits five ways at once: nginx's worker in
  `epoll_wait` with a 60-second timeout, gunicorn's arbiter in
  `pselect6` with 15 seconds, its worker in `poll` with 2 seconds,
  nginx's master in `rt_sigsuspend` with an empty mask, and gunicorn's
  threads in `clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME)`. Plus
  `setitimer(ITIMER_REAL, 50ms)`, which nginx's master arms during
  shutdown to poll for worker exit — so `SIGALRM` delivery is on the
  shutdown path.

**And the timed futex wait is not optional.** `vm.md`'s V3 lists it as a
named fault — "a timeout needs a clock to expire against" — and
gunicorn calls `FUTEX_WAIT_BITSET_PRIVATE` with a deadline seven times in
one boot. N4 retires it along with the rest of the spin, and this is the
evidence that it must.

**Corrections to section 9 in the other direction**, where reality was
narrower than the guess: no `sendfile` (nginx proxies here and serves
nothing static), no `select` (only `pselect6`), no `SCM_RIGHTS` (the
pinned single worker works, as designed — but the master→worker channel
socketpair still carries the shutdown command by `sendmsg`/`recvmsg`, so
those two rows are on the *shutdown* path and not optional). The
`ioctl` traffic is 583 calls of which 566 are `TCGETS`/`TIOCGWINSZ`
isatty probing that already has a row; the socket-relevant ones are
`FIONBIO` ×13 — nginx setting non-blocking by ioctl, exactly as
pitfall 6 warned — plus one each of `FIOCLEX`, `FIONCLEX`, `FIOASYNC`.

**One correction to section 4.** It says non-blocking `connect` on
loopback "completes immediately … so `EINPROGRESS` arises only at the
edge". nginx connects to its upstream non-blocking and then calls
`getsockopt(SO_ERROR)` *regardless* of whether the connect completed —
twice in this trace, both on loopback. So `SO_ERROR` is a loopback row,
not an edge row, and pitfall 4's asymmetry is about `EINPROGRESS` alone.

**Also observed, and cheap:** `getpeername` on a listening socket must
answer `ENOTCONN` (gunicorn probes it); `bind` is to `0.0.0.0:80` and
not to a specific address, so the edge listener registration must handle
a wildcard; backlogs are 511 (nginx), 2048 (gunicorn) and 100 (its
control socket); `accept4` is called with `SOCK_CLOEXEC` and with
`SOCK_NONBLOCK`; and glibc's `/var/run/nscd/socket` probe is a real
`AF_UNIX` `connect` four times over, so `connect` to an unbound path
must answer rather than the VFS answering before socket code is reached
— which is a small correction to `container-plan.md`'s reading of the
same probe.

The original text follows.

**N0 — trace the actual stack.** Build the demo image, run it natively
under `strace -f`, and extract the deduplicated syscall surface of all
five processes from boot through one request and a `SIGTERM` shutdown.
Verdict: section 9's list corrected against reality — every call not
listed there is either added as a row or named a pothole. Reroute: none;
this gate cannot fail, only inform. (Wall-clock: minutes. The trace is
also the strace-diff baseline N5 uses.)

Milestones, each with acceptance and its negative control; the standing
suites grow, never shrink; native kisal tests first at every step, the
module build at checkpoints — the same discipline every prior milestone
carried.

**N1 — the loopback arena. BUILT, 2026-08-30**, except two rows named at
the end. `kisal/src/ring.rs` is the generalized transfer — split out of
`pipe.rs` first, because a connected socket is two rings crossed and the
run loop must not grow a second case for that. `kisal/src/socket.rs` is
the arena: `Backing::Socket`, the port table as a scan over listeners
(which *is* a port table for the handful a container holds), `AF_INET`
loopback streams, `AF_UNIX` by the vnode `bind` creates, `socketpair`,
the half-close matrix, `SIGPIPE` with `MSG_NOSIGNAL`, `MSG_PEEK` and
`MSG_DONTWAIT`, and the option rows.

Twelve of N0's twenty-four missing rows are gone: `socket`, `bind`,
`listen`, `connect`, `accept`/`accept4`, `socketpair`, `shutdown`,
`getsockname`, `getpeername`, `setsockopt`, `getsockopt`,
`sendto`/`recvfrom`.

**The half-close matrix is not a matrix**, which is the return on the
ring split. `shutdown(SHUT_WR)` drops the endpoint's writer reference on
the ring it transmits into, so the peer drains and then reads zero —
which is what the last writer of a pipe closing already does.
`SHUT_RD` drops its reader reference on the ring it receives from, so the
peer's next write is `EPIPE`. One rule per direction, and every cell of
the table the milestone asks for is that rule seen from a different side.

**Readiness needed one arm**, which is the evidence section 4's claim was
right. A socket's readiness is its rings' contents and its accept queue's
length — all arena state, so a scheduling decision can ask it without
touching any process's memory. `EPOLLRDHUP` falls out and is kept
distinct from `POLLHUP`, which is only for when both directions are done.

**A parked `accept` completes on the parked process's own turn**, joining
the parked transfers, the parked waits and `wait4` under the same rule,
for the same reason: the peer's address is written into that process's
memory.

Five programs against native runs of the same binary: a socketpair across
a fork carrying both directions; the half-close matrix; a loopback server
with a *forked* client, port zero, `getsockname`, and an `accept` parked
until another process connects; `ECONNREFUSED` for a port nothing holds
against `ENOENT` for an `AF_UNIX` path that is not there — the
distinction glibc's `nscd` probe turns on; and the options with
`MSG_PEEK`, `MSG_DONTWAIT` and `MSG_NOSIGNAL`. The negative control was
run rather than reasoned about: delete the socket half of the reference
census and the socketpair test deadlocks.

**What was left of N1**, and is not any more: `recvmsg`/`sendmsg`, which
N0 found on the *shutdown* path — nginx's master tells its worker to
stop over their channel socketpair — and `eventfd2`. All three landed
with N2. `recvmsg` cost one thing this text did not predict: nginx
passes a control buffer on every channel message, so refusing a
non-null `msg_control` refuses the shutdown path. Only *sending*
ancillary data is refused now; on receive, `msg_controllen` and
`msg_flags` are both cleared, and leaving `msg_flags` alone is what
made nginx report "recvmsg() truncated data".

**Two corrections to this section's own text**, both from building it.
`SO_RCVTIMEO`/`SNDTIMEO` are refused by name rather than recorded,
because a program told its timeout was accepted will wait forever when it
should have fired — they need N4's deadlines. And `SO_REUSEADDR` is
recorded with *no* effect rather than affecting a rebind rule: the rule
exists on Linux to let a listener rebind a port still held by connections
in `TIME_WAIT`, and there is no `TIME_WAIT` here because there is no TCP,
so a port is free the moment its listener closes.

The original text follows.

**N1 — the loopback arena.** `Backing::Socket`, the arena with port
table and census, `AF_INET` loopback streams, `AF_UNIX` by vnode,
`socketpair`, the half-close matrix, `SIGPIPE`/`MSG_NOSIGNAL`, the
generalized transfer. Acceptance: a forked client/server guest pair
(the `interpreted.rs` pattern) runs against the same binary natively
with identical output, covering connect-before-listen (`ECONNREFUSED`),
backlog overflow, half-close drain-then-zero, `EPIPE`, and a
cross-process `AF_UNIX` echo. Negative control: the census test —
a fork-then-close-both-in-child leaves the parent's `read` returning
EOF, not hanging, and the test fails when a census site is deleted.

**N2 — readiness and the non-blocking discipline.** The
`readiness_of` arm, `select`, non-blocking `connect`+`SO_ERROR`,
`accept4` flags, `FIONBIO`, timed socket transfers. Acceptance:
`python3 -m http.server` in a forked child, `urllib.request` in the
parent, one process tree, response bytes exact — CPython serving
CPython with no host network. Negative control: an epoll-registered
descriptor closed while a dup survives still fires (the description
rule, now with a socket behind it).

**N3 — the edge, and the first curl. BUILT, 2026-08-30.**
`runner/src/net.rs` is the `/iso/net` broker: real `TcpListener`s on host
threads, one event queue the store answers from, and the protocol section 5
specifies — `write /iso/net/listen`, `read /iso/net/events`,
`read /iso/net/conn/{j}/rx/{room}`, `write /iso/net/conn/{j}/tx`. The room
is in the *path*, so the host never delivers more than the guest's ring can
hold and real backpressure reaches all the way back to curl.
`zaqaru-run` takes `-p HOST:GUEST`, repeatable. `Kernel::pump` is the
kisal half, and it touches kernel state only — rings, the socket arena, the
store, never any process's memory — which is what lets it run on any turn
without breaking the rule the process table is built on.

**And the demo answers.** `zaqaru-run hello-django.wasm -p 8081:80`, then
from another terminal:

```
$ curl http://localhost:8081/
<h1>Hello, world!</h1>
```

Five processes inside one 170 MB `.wasm` — an init script, nginx's master
and worker, gunicorn's arbiter and worker — with nginx's own access log
recording `127.0.0.1 - - "GET / HTTP/1.1" 200 34 "-" "curl/8.5.0"`. The
`docker save` archive is the input and the image's own entrypoint runs it.

Three things the milestone did not anticipate. **A listener's registration
answers nothing**, and does not need to: an unmapped port simply never
produces an event, so a loopback-only listener is one with nothing arriving
on it, and a guest that could tell the difference would be a guest that can
see its own port mapping — which `docker` does not give it either. **One
runner serves both paths**: `Container::boot` picks `targum_boot` or
`kisal_boot` by which the module has, because everything around the entry
is identical and a runner that knew one name would put the choice of
execution path in the command line. And **the peer address is not
optional** — nginx writes it into its access log, so dropping it gives a
container whose log says `unix:` for every request.

**N4 arrived with it**, because the demo could not work without it. The
`/iso/net/wait/{ms}` read is built and `System::idle` is where it is
called: nothing runnable, earliest deadline known, so "wait for an event or
that long" is the one call that turns a spin into a sleep. `Exit::Deadlocked`
gained the third verdict this section foresaw — nothing runnable with a
listener outstanding is a server at rest, not a deadlock. What has *not*
been done is measuring the idle CPU, which is N4's own acceptance line.

The original text follows.

**N3 — the edge, and the first curl.** The `/iso/net` protocol,
the pump at its two deterministic points, `NetStore` with `-p`, the
console tee. Acceptance: `zaqaru-run module.wasm -p 8080:80` around
the N2 guest server, and **`curl http://localhost:8080/` answers from
outside** — the demo's seam proven on a one-process stack. The syscall
trace of the guest matches N0's shape for the request path. Negative
control: no `-p` — the guest binds and serves loopback happily, the
host connection is refused; no `/iso/net` mount — `bind` still
succeeds, loopback-only, and nothing reaches the store.

**N4 — the wait. BUILT, 2026-08-30, and measured.** The blocking read is
`/iso/net/wait/{ms}` and `System::idle` is where it is called: nothing
runnable, earliest deadline known. **The demo idles at 0% of one core**
between requests — three successive ten-second windows, after roughly
twenty seconds of post-request work that is Django still importing rather
than the kernel spinning. `Exit::Deadlocked` gained the third verdict:
nothing runnable with a listener outstanding is a server at rest.

**Ctrl-C ends the tree, and the tree ends itself.** `SIGINT` at the runner
sets a flag; `/iso/shutdown/requested` is a path the guest *reads* at the
points it already asks the host things, because the boundary has no way to
push; and what it becomes is a `SIGTERM` at the first process, which is
what `docker stop` sends. The log is the whole story:

```
[notice] 1#1: signal 15 (SIGTERM) received, exiting
[notice] 3#1: exiting
[notice] 3#1: exit
[notice] 1#1: signal 17 (SIGCHLD) received
[notice] 1#1: worker process 3 exited with code 0
[notice] 1#1: exit
```

nginx's master told its worker over their channel socketpair, the worker
exited, the master reaped it and left. Exit status 0.

**Three defects on the way, each of which only a server finds.**

*The mask a suspend puts aside goes back on `sigreturn`, not on wake.*
`rt_sigsuspend` replaces the blocked set for the length of the wait, and
restoring it when the wait ends re-blocks the very signal the program
suspended in order to receive — between the moment it arrived and the
moment a handler was chosen for it. nginx does exactly this, and the
container sat there ignoring its own shutdown with the signal pending and
unblocked in the trace. It goes into the frame the handler returns
through, which is where Linux puts it.

*`recvmsg` with a control buffer is not ancillary data.* The refusal was
on the buffer's presence rather than on anything being sent, and nginx
passes one on every channel message because it *might* be sent a
descriptor. Only a `sendmsg` carrying ancillary data is refused now; a
receiver is told `msg_controllen` is zero, which is how it learns nothing
arrived.

*An out parameter that is only sometimes written is one the caller cannot
use.* `msg_flags` was never cleared, so nginx read whatever was in that
stack slot, found `MSG_CTRUNC`, and logged "recvmsg() truncated data" on
every message.

**And `setitimer` with it**, which N0 said was on the shutdown path:
nginx's master arms a fifty-millisecond `ITIMER_REAL` to poll for its
worker's exit, so `SIGALRM` had to be real. It is a per-process deadline
that raises a signal through the disposition table like any other, and it
joins the earliest-deadline computation — a container asleep past an alarm
would deliver it late.

The original text follows.

**N4 — the wait.** `/iso/net/wait/{ms}`, the earliest-deadline
computation, the `Deadlocked`-versus-listening verdict, Ctrl-C to
SIGTERM. Acceptance: the N3 server idles at (measured) ~0 % host CPU
between requests, and a `ppoll`-with-timeout guest stops spinning —
both measured, both pinned. Negative control: a genuinely deadlocked
container (two processes, one pipe, both reading) still exits
`Deadlocked` with the stall report; the listener case must not have
widened into never-say-deadlock.

**N5 — the stack. BUILT, 2026-08-30**, except the replay. The demo image
of section 8 is `demo/hello-django`; it is baked and served and `curl`
returns the Django page; a second request reuses the container warm; and
`SIGTERM` shuts the tree down cleanly with exit status 0 — see N3 and N4
above, which is where those landed.

**And the strace diff is done**, which was also V2's long-outstanding
acceptance: `demo/hello-django/baseline/n5-diff.txt`, regenerated by
`trace.sh` and `diff.py`. The same stack traced both ways on the same
scenario — boot, two requests, shutdown — and **every divergence
accounted for**:

- **The vDSO's absence *was* the whole of it**, 6,455 of 6,621 extra
  calls — and it is built, so it is not any more. See below; the numbers
  here are what the diff said before that.
  The native trace makes *zero* clock syscalls, because glibc reads the
  time from a page the kernel maps into userspace that no `strace` ever
  sees. There is none here and `AT_SYSINFO_EHDR` is absent from the
  auxiliary vector, so glibc takes the syscall path it keeps for exactly
  that case. `clock_gettime`, `gettimeofday`, `time`, `clock_getres` and
  `sched_getaffinity` are all this one cause. It was documented in the
  `clock_gettime` row long before anything diffed it, and closing it means
  building a vDSO — a real option, not a defect.
- **`kill` is the two shutdowns differing.** N0's was signalled from
  *inside* the container, because `docker stop` signals pid 1 which under
  tracing is `strace` itself; the interpreted run's arrives from the host
  at pid 1 directly, so no guest process calls `kill` at all.
- **`madvise`, `exit` and `getpeername` are one call each**, and each is a
  moment one run reached and the other had not.

What is *not* in the diff matters as much: no call appears in the native
trace that this kernel would have refused, and no shared call's count
differs by more than a factor of two.

**And the tape replays**, which was the last of it. `zaqaru-run --record`
keeps every answer the host gave and `--replay` answers from that instead
of from the world; `demo/hello-django/replay.sh` records a served HTTP
session and replays it **with no network at all** — no `-p`, nothing
mounted at `/iso/net` — and the container's output is identical byte for
byte, down to nginx's access-log timestamp and the clean `SIGTERM`
shutdown after it.

Reads only, and that is not a limitation: a write *leaves* the container,
and replaying one would mean sending the same bytes to a real socket a
second time, which is repeating an effect rather than reproducing a run.

One thing it had to be taught, and it is the kind of thing only running it
finds: **a tape holds every answer, including a refusal.** A read of a
path nothing is mounted at fails, and that failure is an answer the guest
acts on — an unmounted `/iso/config/trace` is how a container learns it is
not being traced. Recording only the successful ones shifted every later
entry by one, and the replay handed the guest its entropy seed where it
had asked for a configuration flag.

The original text follows.

**N5 — the stack.** The demo image of section 8, baked and served;
`sendfile` and whatever N0 added. Acceptance: `curl` returns the
Django page through nginx and gunicorn; a second request reuses the
container warm; `SIGTERM` shuts the tree down cleanly with the exit
status at `/iso/shutdown/complete`; the request path's syscall trace
diffs against N0's native trace modulo documented divergences. Then
the recorded tape replays to a bit-identical run — the record/replay
claim of section 5, demonstrated rather than asserted.

The browser harness is deliberately after N5, not a milestone here: it
reimplements section 5's mount against a proven protocol, and its own
plan (Service Worker plumbing, the Worker/`Atomics.wait` shape) belongs
next to the harness code when N5 has settled the protocol's details.

## 13. Risks and open questions

- **Event latency under load** is bounded by the pump's slice-boundary
  cadence — `SLICE × QUANTUM` retired instructions, tens of
  milliseconds at floor speed. Fine for a demo; a throughput-minded
  host wants the knob, and the knob must stay a function of retired
  instructions or determinism breaks. Named, not solved.
- **The wait read holds the wasm thread.** While kisal sits in
  `/iso/net/wait` the module cannot do anything else — which is
  correct (nothing was runnable) but means the host must not route
  *other* store traffic through the same thread. The runner's threads
  make this easy; a single-threaded embedder must slice `{ms}` finer.
- **Throughput through the store** is two copies (ring → arena → host
  socket) plus the canonical-ABI transfer arena, whose 64 KiB capacity
  (`abi.rs`) becomes the pump's chunk size. Hello-world does not care;
  a file-download benchmark eventually will, and the measurement
  belongs to N5's numbers, not to guesswork here.
- **`MAP_SHARED` across fork** is the one genuine boundary the demo
  configures around rather than fixes (section 9). The copy-on-switch
  sketch exists; whether it is ever built is decided by a real image
  that needs it, per the standing policy.
- **Half-close at the edge** has more states than loopback (the host
  peer can shutdown one direction); the `ctl`/`eof` vocabulary must
  cover `SHUT_WR` from either side, and N3's acceptance should include
  one curl with `--http1.0` no-keepalive and one with keepalive to
  exercise both close orders.

## 14. Pitfalls index

Seeded from the design work; grown as they are earned.

1. **The census counts directions, not descriptors.** A socket
   endpoint copied by fork raises one reference that stands for both
   rings; the shutdown bits are per direction and are *not* references.
   Conflating the two hangs a reader exactly the way the pipe comment
   warns, and it hangs only in the forked case, which is the untested
   one by default.
2. **Readiness for an edge socket is the ring, never the store.** The
   moment a scheduling decision reads `/iso/net/*`, the tape gains
   entries whose timing depends on scheduling — which is the
   determinism loop this design exists to avoid. The pump is the only
   store toucher, at its two named points.
3. **`open` events must land on the listener that was mapped, not the
   most recent one.** Two guest listeners with two `-p` mappings is the
   first config that catches a store keyed by "the" listener.
4. **A loopback `connect` completes synchronously; an edge `connect`
   never does.** Code written against loopback's instant success will
   pass every in-guest test and break on the first outbound edge
   connection. The demo needs no egress, which is exactly why the
   asymmetry must be stated where the next feature will trip it.

   **As built, there is no egress at all**, and what was found in
   review is that this has to be *said* rather than left to fall out.
   The guest is told of no interface, so `127.0.0.0/8` is the whole of
   the world it can route to; a `connect` anywhere else now answers
   `ENETUNREACH`. It used to answer `ECONNREFUSED`, which is a
   different claim and the wrong one — a client that hears "refused"
   concludes the service is down and retries forever, where one that
   hears "unreachable" concludes there is no network, which is true.
   `-p` does not soften this: it publishes a guest port *inward*, a
   listener the host reaches, never a route the guest can take out.
   When egress is built it takes the `EINPROGRESS`/`SO_ERROR` shape
   this pitfall describes, and `ENETUNREACH` is what it replaces.
5. **Do not let the wait read carry a zero timeout as "forever".**
   `{ms}` of zero means poll-and-return; forever is a large explicit
   cap. An accidental infinite host block with no guest deadline is a
   hang that looks identical to pitfall 2's spin from the outside.
6. **`FIONBIO` and `O_NONBLOCK` are one bit.** nginx sets it by ioctl,
   CPython by `fcntl`; a per-call flag check that consults only one
   spelling makes one of the two servers block where Linux would not.
