# Fidelity: what the kernel refuses, what it records, and what is not built

Status: reference, read out of the code rather than the plans. The design
*rationale* for any single entry lives at its call site; this is the index.

Every entry names the shape a program would have to have for the difference
to matter. That is the point of the document: a divergence nobody can
describe the failure of is a divergence nobody can look for.

## The three answers

A guest asking for something this kernel does not fully do gets one of
three answers, and which one is a deliberate choice each time.

1. **A refusal.** The container stops and the log names the syscall, its
   six arguments, and which *part* was missing. Nothing continues in a
   wrong state. This is the default, and 30 calls take it by name.
2. **A recorded no-op.** The call succeeds and changes nothing observable.
   Allowed only with evidence that nothing in the target workload reads
   what it would have changed — and every one states what would break if
   something did.
3. **An honest errno.** The capability genuinely is not there, and Linux
   has a way to say so: `ENETUNREACH` for an address with no route,
   `EROFS` for a mount with no writable layer.

A syscall with no row at all is a refusal, so the worklist is complete by
construction: nothing can be missing and quiet at the same time.

## 1. Refusals

### Memory

| Call | Refused | Shape that needs it |
| --- | --- | --- |
| `mmap` | a writable shared file mapping | anything that `mmap`s a file `MAP_SHARED\|PROT_WRITE` and expects write-back |

### Files

| Call | Refused | Shape that needs it |
| --- | --- | --- |
| `fcntl` | record locks (`F_GETLK`, `F_SETLK`, `F_SETLKW`, and the `OFD_` three) | anything coordinating through `flock`-style file locking |
| `fcntl` | `F_GETOWN_EX` | a program distinguishing a thread owner from a process one |
| `ioctl` | any request with no driver | a device this container does not model |
| `readlinkat` | `/proc/self/exe` before anything set it | reachable only on a path where `execve` has not run |
| `linkat` | `AT_EMPTY_PATH` | linking a descriptor whose name was not kept |
| `fchmod` | a file reached only by a descriptor | same: the name was not kept |
| `renameat2` | any flag (`RENAME_NOREPLACE`, `RENAME_EXCHANGE`, `RENAME_WHITEOUT`) | each promises a different atomicity, and accepting a flag without keeping its promise is worse than saying so |
| `pipe2` | `O_DIRECT` | packet-mode pipes, a different data structure |

### Sockets and polling

| Call | Refused | Shape that needs it |
| --- | --- | --- |
| `setsockopt` | `SO_RCVTIMEO`, `SO_SNDTIMEO` | a parked transfer racing a deadline. Refused rather than recorded on purpose: a program told its timeout was accepted waits forever where it should have fired |
| `sendmsg` | ancillary data, which here means `SCM_RIGHTS` | descriptor passing — **nginx needs it the moment `worker_processes > 1`** |
| `epoll_*` | an `epoll` descriptor inherited across `fork` | Linux shares the interest list; here a description is a per-process index, so sharing would let one process cancel another's registration |

Receiving a control buffer is *not* refused: nginx passes one on every
channel message. `msg_controllen` and `msg_flags` are cleared on the way
out, and leaving `msg_flags` alone is what once made nginx report
"recvmsg() truncated data".

### Process and time

| Call | Refused | Shape that needs it |
| --- | --- | --- |
| `kill` | a negative pid | a container has one process group, so there is nothing for the number to select |
| `clone` | a flag this kernel does not read | anything changing what the new thread *is* |
| `prctl` | an option with no row (the number is in the first argument) | — |
| `prlimit64` | changing a limit, or asking about a resource nothing here decides | a program that sizes something against a limit it just set |
| `setitimer` | `ITIMER_VIRTUAL`, `ITIMER_PROF` | timers counting processor time, which this kernel does not account for |
| `setitimer`, `nanosleep`, `futex` | any deadline **with no `/iso/time` mount** | a container booted without a clock |
| `arch_prctl` | the `%gs` base | nothing on this path uses it |
| `arch_prctl` | CPUID faulting | the engine curates CPUID rather than emulating it |
| `futex` | an operation not implemented | only `WAIT`, `WAKE`, `WAIT_BITSET` and `WAKE_BITSET` exist; requeue, `WAKE_OP` and the priority-inheritance family do not |

### Only on the native tests' machine

The native kernel tests run over a bare register file with one thread
and no scheduler. There, `pause`, `rt_sigsuspend`, `nanosleep`, `clone`,
`rt_sigreturn` and a `futex` wait are refused, because nothing could ever
wake or resume them. The interpreter's machine implements all six; a
container never sees these refusals.

## 2. Recorded no-ops, with the divergence stated

Each of these succeeds and changes nothing. The evidence column is why
that was allowed; the risk column is the program that would notice.

| What | Evidence it is unread | What would break |
| --- | --- | --- |
| `ioctl(FIOASYNC)` | nginx's master sets it on its end of the worker channel and then waits in `rt_sigsuspend`; the worker learns everything through `epoll_wait` | a program that sets it and blocks with **no other readiness source** never wakes — no `SIGIO` is ever sent |
| `fcntl(F_SETOWN)` / `F_GETOWN` | the owner is recorded and answered; the same divergence as `FIOASYNC` | the same |
| `prctl(PR_SET_PDEATHSIG)` | recorded and answered | no signal arrives when the parent dies. The process table *does* know — it reparents children there — so this is a row waiting to be finished, not a capability the design lacks |
| `prctl(PR_SET_DUMPABLE)` | recorded and answered | nothing dumps core here anyway |
| `SO_REUSEADDR` | there is no `TIME_WAIT` because there is no TCP; a port is free the moment its listener closes | nothing — there is nothing for it to relax |
| `SO_KEEPALIVE`, `TCP_NODELAY`, `SO_RCVBUF`/`SO_SNDBUF` | recorded; there is no TCP here to tune and no segments to coalesce | a program measuring throughput against a buffer size it set |
| user and group identity | recorded and answered, never enforced. nginx's worker drops to `nobody` and then opens the log its master created | a program relying on a permission *denial*. Half-enforcing is worse than not: adding a check to one of the places that needs one is how that fails unpredictably. The execute bit is the exception — `execve` does check it |
| resource limits | `RLIMIT_STACK`, `RLIMIT_NOFILE` and `RLIMIT_AS` answer real numbers; the rest are refused rather than invented | nothing sizes itself against a limit nobody keeps |

Page protections are *not* on this list: `mprotect` and `mmap`'s `prot`
are enforced by the interpreter's page table on every access, so a
`PROT_NONE` guard page faults and a write to read-only memory is a real
`SIGSEGV`.

Two divergences are inherited rather than recorded.

**The entropy generator is copied by `fork`**, so a parent and child that
both read it get the same stream, where Linux's pool would give them
different bytes. glibc reseeds `arc4random` on fork and CPython reseeds
its hash, so nothing yet observes it. The fix is for `/iso/random` to be
asked again in the child rather than copied.

**The flags word in a signal frame may be an earlier instruction's.** An
arithmetic instruction whose six status flags are overwritten before
anything reads them does not record them (the interpreter's dead-flag
elimination, and the bytecode transpiler's liveness analysis, which every
shift and multiply it emits depends on). The guest cannot tell — by
construction the next reader sees the overwriting instruction's flags —
except by inspecting `EFLAGS` in the `ucontext` of a signal delivered at a
quantum boundary that fell between the two. A handler that reads it sees
the flags as they stood after the last *recorded* writer. Nothing but a
debugger does that; the lockstep oracle withholds its flags comparison at
exactly those points for exactly this reason.

## 3. Not built

| Gap | What a guest gets today |
| --- | --- |
| **Egress** — no outbound connection at the edge | `ENETUNREACH` for anything off `127.0.0.0/8`. The guest is told of no interface, so this is the truthful answer for a namespace holding only `lo`; `-p` publishes a port *inward* and is not a route out |
| **DNS** — no `getaddrinfo` path, no netlink | the demo uses numeric addresses. glibc's NSS probe of `/var/run/nscd/socket` gets `ENOENT`, which is what sends it to `/etc/passwd` |
| **`SCM_RIGHTS`** | refused by name (above) |
| **`MAP_SHARED` anonymous across `fork`** | accepted, and the child gets a *copy* — the one genuine boundary the demo configures around rather than fixes. nginx maps a small shared region for its connection counter and accept mutex; the mutex has defaulted off since nginx 1.11.3 and with one worker the counter has one writer and no reader. The sketch of a fix is to copy through a kernel-held backing at the process switch |
| **Instructions the engine does not decode** | the container stops naming the address. Distinct from a syscall refusal and reported separately |

## Keeping this honest

The refusal count is checkable:

```
grep -rc 'Fault::detailed(' --include=*.rs crates/kernel/src   # 30 sites
```

If that number moves and this document does not, one of them is wrong.
