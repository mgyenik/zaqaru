# Container worklog

Status: **live** — appended to as work happens.

The milestone plan ([container-build-plan.md](container-build-plan.md)) says
what is being built and records each milestone's verdict when it closes. This
records the *working* layer underneath that: decisions taken mid-build and
why, roadblocks and how they were cleared, and mistakes worth not repeating.

Two standing rules for this file. **Nothing is deferred here.** If something
is in a milestone's scope it gets built in that milestone; an entry that says
"later" is either scope the design doc already excludes — with a pointer to
where — or it is a defect in this file. And **an entry is written when the
thing happens**, not reconstructed afterwards, because reconstruction is how
"we checked that" gets written down about something nobody checked.

---

## 2026-08-28 — M0, M1, M2

Verdicts and deviations are in the build plan's own sections. What belongs
here is the shape of what went wrong, because it repeated.

**Every defect found in M1's review was a silent wrong answer**, in a project
whose stated policy is that nothing fails silently. The seam spending the
guest's red zone, guest pointers truncated from 64 bits to 32 after being
validated at 64, flags left in locals across a syscall, `%fs` added to a
`lea`: none of these would have produced an error, a trap, or a log line.
They would have produced wrong data much later, somewhere else.

The two root causes are worth naming because they are cheap to repeat:

1. **Inheriting a shape instead of re-deriving it.** The seam's stack dance
   was copied verbatim from the interop thunk, where it is correct because
   the SysV ABI lets a callee destroy the red zone. A `syscall` is not a
   call. The code even carried a comment asserting the copy was deliberate.
2. **Assertions that cannot distinguish the right answer from the default
   one.** `rcx` and `r11` were asserted zero after a syscall — which they
   were anyway, because nothing ever wrote them. Four separate tests passed
   with the feature under test broken. The fix in every case was to make the
   guest write something that is not the default.

What was done about it: each of the four tests was rewritten so that the
assertion can distinguish the right answer from the default, and each was
checked by breaking the feature on purpose and confirming the test fails.
That is a technique, applied here because four tests had just been caught
being useless — not a rule anyone has adopted, and not a thing every future
test is owed.

---

## 2026-08-28 — M3 begins: the index format and the baker

### Decisions

**The index format lives in `kisal::image`, and the baker depends on kisal
for it.** The reader defines the layout; the writer imports it. The
alternative — a format module both crates copy — is exactly the shape that
produced the canonical-ABI layout bug in M1, where a struct encoded the wrong
union arm and the two sides disagreed silently. One definition means a round
trip cannot catch a mistake *in* that definition, so the layout is
additionally pinned against independently written literals in a test.

**Interning happens where a name enters the image**, not where it is written
out. The first version interned only symlink targets and xattrs and left
directory entry names unresolved; `finish` then panicked on its own
assertion. Worth recording as a success rather than a failure: the assertion
existed, it fired, and it named the invariant.

**`nlink` is computed after the whole tree is walked.** A hardlink's twin may
not exist yet when the first name is added, and a directory's count is two
plus its subdirectories — POSIX counting `.` and its parent's entry. Both are
whole-tree facts.

**Page alignment for ELFs is done now although nothing uses it.** The
zero-copy aliasing optimisation is designed, flagged and off, and v0 copies
every file mapping. But alignment is a property of the *blob layout*: adding
it later means re-baking every image, and it costs a few kilobytes of padding
on a handful of files. This is not deferred work — the optimisation is
explicitly out of scope in the design doc, and this is the part of it that is
cheaper to do than to retrofit.

### Roadblocks cleared

**The allocation counter was measuring other tests.** The design's claim that
resolution and `stat` allocate nothing is asserted with a counting global
allocator. The first version used a global `AtomicUsize` and reported 587
allocations for a path that makes none — because `cargo test` runs tests in
parallel threads and the counter saw all of them. Now per-thread, with
`const` initialisation so the thread-local cannot itself allocate inside the
allocator, and `try_with` so teardown does not panic. The instrument has its
own test: a planted allocation must be seen.

### Sequencing

M3's scope is the baker *and* kisal's read-only file layer. Order within it:
the index format and the directory-tree bake first (done), then the wasm
object emission, then kisal's resolution loop and fd table, then the
read-only syscall rows, then the differential corpus guests, then the
`docker save` tarball input. The tarball is last because a layer stack
flattens *into* a tree and the tree path is what everything else needs; it is
in M3 and closes with M3.

---

## 2026-08-28 — M3: the read-only filesystem under emulation

### Built

The wasm object emission (`baker::object`), kisal's resolution loop
(`kisal::vfs`), the descriptor table with shared open file descriptions
(`kisal::fd`), and the read-only syscall rows (`kisal::file`): `open`,
`openat`, `close`, `read`, `pread64`, `lseek`, `stat`, `lstat`, `fstat`,
`newfstatat`, `statx`, `getdents64`, `readlink`, `readlinkat`, `access`,
`faccessat`, `fcntl`, `dup`, `dup2`, `dup3`, `getcwd`, `chdir`, `fchdir`.

Thirty native tests drive those rows against a real bake, and
`tests/filesystem_differential.rs` runs the same C program under the real
kernel and under kisal and compares the reports record for record, in all
four build configurations. Three `struct stat` fields are excluded and each
exclusion is argued in that file's header rather than discovered by a
disagreement.

### Decisions

**A directory's parent is stored in the index, not tracked during a walk.**
`openat` starts a walk at a descriptor and `getcwd` starts at nothing, so a
path stack is not available at the moment `..` needs it. POSIX forbids hard
links to directories, so a directory has exactly one parent and the field is
well defined — the same reason a real kernel can afford `..` as a real entry.

**`getdents64` synthesizes `.` and `..`.** The index stores neither, because
a directory already carries both facts. A real directory has them, POSIX says
it has them, and readers assume it — an image that omitted them would differ
from every filesystem any program has been run against. Found by writing the
differential, not by a test failing.

**The image is read-only and says `EROFS`.** An overlay goes over it at M4.
Until then a write-intent open gets the errno that means exactly what
happened, rather than `EACCES` or a silent success.

**`access(W_OK)` is refused and permission bits are not enforced.** The bake
preserves mode bits completely and whether kisal ever *honours* them is
deliberately open; answering `EACCES` from them here would quietly close that
question. Writing is refused because the filesystem really is read-only.

### The roadblock, and what it exposed

The differential failed with every path resolving to `ENOENT` after the first
one. Half a day of bisecting — tree contents, image size, kernel-stack size,
arena size, promotion on and off, both control-flow modes, guest source
truncated five ways — ruled out the translator entirely, because all four
configurations failed identically.

The cause: `Container::allocate` handed out memory from **the canonical
ABI's transfer arena**, whose lifetime is one syscall. The test placed the
guest's path prefix there; the first syscall reset the arena and the runner
then wrote the console mount's result path — `["iso", "console", "stdout"]` —
back through it at offset zero. The prefix became `"iso"`, so every path the
guest built was `"iso/etc/hosts"`. The `'i'` visible in a hex dump of the
guest's buffer was literally the first byte of `"iso"`.

Two lessons worth keeping:

1. **The bug was mine, and the first fix was worse than the bug.** I added a
   second allocator, `kisal_reserve`, so the host could place durable data in
   guest memory — and justified it with a claim that M6's argv and envp would
   need the same thing. They will not: kisal builds the initial stack itself,
   in its own memory, which is what both design documents say. The API was
   production surface added to fix a test's problem, and it was reverted. What
   shipped instead is a rename — `Container::allocate` is now
   `allocate_transfer` — and a doc comment that says the lifetime out loud.
   The test's own problem went away by not needing host-placed memory at all:
   the guest treats a null prefix as empty.
2. **It was silent.** Nothing failed; the answers were simply wrong. The
   diagnostic that finally located it was making the guest report the bytes
   it was about to pass, which is the same move as a loud error: print what
   you have rather than infer it from what happened next.

`tests/kernel_seam.rs` has `the_transfer_arena_lasts_one_syscall`, and it is
worth being accurate about what it is: a **characterisation** test. It asserts
the arena *is* clobbered by the next syscall, so it passes against the
arrangement that produced the bug and would fail only against the reverted
design. It documents the lifetime; it does not catch a caller who ignores it.
An earlier version of this entry claimed it "fails against the old
arrangement", which is the opposite of what it does.

---

## 2026-08-28 — M3: what four adversarial reviews found, and what it cost

Four reviewers went over the M3 work, at the user's instruction: memory
safety on the wasm32 target, POSIX conformance against a real kernel, test
quality, and the honesty of the documents. Between them they found thirty-odd defects. The
shape of them is worth recording, because it repeats the shape M1's review
found.

**Almost every conformance defect was a plausible wrong answer**, not a
failure. `access(path, 8)` said yes to a garbage mode. `faccessat` looked
like it honoured `AT_SYMLINK_NOFOLLOW` — and separately, the row it was
being compared against does not take flags at all. A directory offset past
four gigabytes wrapped back to `.` and re-listed the directory forever.
`statx` wrote the modification time into the birth-time field and advertised
it in the mask, which is the one field `statx` exists to add over `stat`. The
guest could not tell any of these from a correct answer.

**Every memory-safety defect was a 32-bit narrowing.** `usize` is 32 bits
inside the module and 64 on the host that runs the tests, so a cast that is
harmless in every native test traps or reads the wrong bytes in the thing
that ships. `read` with an offset of 2^32 returned the head of the file and
reported success. The bounds checks in `image.rs` were `start + 2 >
region.len()` with a `start` the guest could make `0xFFFF_FFFF` — a guard
that reads like a guard and wraps to `1`. The rule that came out of it is
written at the top of `kisal::image`: **checked at full width, narrowed only
after the range is known to fit.**

**Two of the reviewers' own findings were wrong, and checking cost less than
believing them.** One said `fchdir` on a regular-file descriptor is `EBADF`
on Linux; it is `ENOTDIR`, and kisal was already right. One said `faccessat`
silently discards its flags argument; the kernel's `faccessat` is
`SYSCALL_DEFINE3` and has no flags argument — reading a fourth register would
have answered `EINVAL` to ordinary calls whenever `%r10` happened to be
non-zero. Both were settled by running a C program against this machine's
kernel, which took a minute each.

### The mount table, which was in M3's scope and had been dropped

The build plan lists "the mount table and the vnode walk" under M3. There was
no mount table: `vfs.rs` held one `Image` and a comment saying M4 would make
it a table. Nothing recorded the omission — not the worklog's sequencing
list, not its "still open" section.

It is built now, and it changed a type that runs through the whole kernel: a
file is a **vnode**, `(mount, inode)`, not an inode. That is not bookkeeping.
`st_dev` plus `st_ino` is how every program on Unix decides two paths are the
same file — `find -xdev`, `cp -a`'s hardlink detection, `du`'s deduplication
— and two independently baked images reuse inode numbers freely. An identity
that was only an inode number made unrelated files in different mounts
compare equal.

What crossing a mount point means is decided in one place, on the way *into*
a directory, so no caller ever holds a vnode on the wrong side of one. `..`
is the direction that needed thought: at the root of a mounted filesystem it
leaves that filesystem, landing on the parent of the directory the mount
covers. Getting that wrong traps a process inside the mount, because the root
of every filesystem is its own parent.

Production has one mount until M4 attaches an overlay. The tests attach a
second baked image over a directory of the first, which is what makes the
crossing, the `..`, the per-mount device numbers and `getcwd`'s naming of the
mount point observable rather than asserted.

### The kernel stack's guard

`src/seam.rs` claimed a stack of the kernel's own "removes the whole class
rather than leaving a margin to be exceeded later". Half true: it removes the
red-zone class. It replaced it with a fixed 64 KiB region, no guard page, no
check, in an environment with no faults — which is precisely a margin.

There is a guard now: four kilobytes below the stack, filled with a sentinel
on the way into every syscall and checked on the way out, and again around
the whole of `x86_run_thread` so that a syscall leaving through the yield
throw is checked too. An overrun traps instead of writing into whatever the
linker placed below. The trap is deliberately a trap and not a report: the
kernel is what overran, so asking it to format a diagnostic would run the
same code on the same broken stack.

This has a guard page's limitation and not more — a single frame larger than
the guard could step over it — and the comment now says so instead of
claiming the class is gone. The demonstration is a container linked against a
stand-in kernel that asks for a 66 KiB frame, with a modest-frame control
that must not trap.

### Test quality: four false greens, and the fixture that caused three

A reviewer mutated `getdents64` to hardcode `DT_REG` for every entry. **The
entire 188-test suite stayed green**, including the differential against the
real kernel. The cause was not the assertions; it was the fixtures. Both
trees listed exactly one directory, `/etc`, and it held nothing but regular
files, so the only `d_type`s ever observed were the hardcoded constant and
the synthesized `.`/`..`. The fixture now holds a subdirectory, a symlink and
a fifo, the differential lists the root as well, and the same mutation now
fails on record 67.

Three other tests asserted things they could not fail:

- `a_mount_lookup_uses_every_segment_it_was_given` wrote one path and
  asserted a shorter one read back `None`. Nothing had ever written the
  shorter path, so `None` was the answer under the bug and under the fix
  alike. It now writes both paths, with different bytes, and each has to read
  back its own.
- `resolution_and_stat_allocate_nothing` called the *test file's own* walk
  helper, never `Vfs::resolve` and never any `stat` at all. The instrument
  was sound — a planted allocation is seen — but it was pointed at a copy of
  the code. The claim is now measured through `Kernel::dispatch` over `stat`,
  `statx`, `open`, `read`, `getdents64`, `readlink` and their misses; the
  index-walk measurement kept its own honest name.
- `a_path_outside_the_guest_is_refused` never reached `GuestMemory::check`:
  both its inputs were caught by an earlier guard. The two branches its
  documentation described — an unterminated string running off the end of
  memory, and one running past `PATH_MAX` with memory to spare — now have a
  test each, and they are different errnos for a reason.

**The runner's `ll_read` was dead code in the test suite.** Every filesystem
row answers out of the image and every console write goes the other way, so
nothing had ever driven a read *down* through the store boundary — the
closure, its return-area decoding and all three of its arms. Two container
tests now do, one with bytes and one with an absent path.

### Decisions taken while fixing

**`statx` advertises only what it filled.** The image has no birth time, and
an OCI layer is a tar archive that carries one timestamp. Reporting the
modification time as a birth time and setting `STATX_BTIME` would be a
plausible wrong answer to the only question `statx` answers that `stat` does
not. `stat`'s three timestamps are all the modification time, which is what a
filesystem built from an archive can honestly say, and the differential
excludes `st_atim`/`st_ctim` with that argument written down rather than
discovered.

**A console stream answers `ENODATA` and an empty list for extended
attributes, not `ENOTSUP`.** Checked against `/dev/null`, which does exactly
that: `ENOTSUP` says the *filesystem* has no attributes at all, which is a
different fact, and callers branch on the difference.

**`read` checks the caller's buffer against the requested count before it
looks at the file.** `read(fd, buf, (size_t)-1)` is `EFAULT` on Linux, not a
short read — verified, along with the ordering against a negative offset's
`EINVAL`. Clamping to the file's length first turned a garbage count into a
cheerful success.

**`lseek` refuses only an overflow, not a large offset.** Filesystems differ
about their maximum — this machine's ext4 refuses `INT64_MAX` and tmpfs
accepts it — and a directory's `d_off` cookies are not byte positions at all,
so refusing early would break the one caller that legitimately seeks large.

**`fcntl`'s record-lock and owner commands are named faults, not `EINVAL`,
and this is a deliberate divergence.** Linux answers `F_SETLK`, `F_GETLK` and
`F_GETOWN` successfully on a read-only file — a reviewer measured it — and
answering `EINVAL` would tell a caller "this kernel has no such command",
which is a lie about a command Linux implements. Succeeding would also be
defensible today, since one process holds every lock uncontended. It stops
being defensible at M6, when `fork` and threads arrive and a granted lock
means something. The loud fault is the policy: it costs a container that uses
`fcntl` locks, and it cannot become a silent wrong answer between now and the
milestone that implements them. `container-plan.md` already puts locks in
scope as in-guest state.

**Device nodes in the image are refused by name.** A character device baked
into an image has no driver behind it; `/dev` becomes a synthetic mount at
M4. `read` answers `ENODEV`, which is what "there is no such device" means,
rather than an `EINVAL` that would be a plausible answer to a valid call.

**`W_OK` is not in the differential.** kisal answers `EROFS` and the oracle's
tree is an ordinary writable directory under `/tmp`, so the real kernel
answers 0. That is a difference between the two *mounts*, not between the two
kernels, and comparing it would be comparing the fixture. The `EROFS` answer
is asserted natively, against ground truth taken from a real read-only mount.

---

## 2026-08-28 — M3 closes: the `docker save` tarball input

### The refactor that came first

Two front ends were about to write inodes directly — one from `statx`, one
from tar headers — which is two copies of the inode-construction rules, and a
rule that exists twice is a rule that will be true in one place. The reviews
had just spent a day on exactly that shape.

So there is now a `baker::tree::Tree` in between: a directory tree of files
with POSIX metadata, which both front ends produce and the baker alone
consumes. The directory path was converted first, with its twenty tests as
the check that the conversion changed nothing.

It changed one thing, and the differential caught it. A directory's
`st_size` used to be whatever the host filesystem said — 4096 on ext4 — read
straight out of `statx`. A tar archive carries no directory size at all, so
the tarball path could never reproduce that number, and the two front ends
would have disagreed about the same tree. The image now answers the length of
its own entry block: a real number about its own storage, which is what every
filesystem reports and what each one means differently. The differential
excludes directory `st_size` with that argument written into its header.

### Built

`baker::tar`, `baker::json`, `baker::layers`, and `bake_archive`.

**The tar reader is hand-written and the JSON reader is too**, for the same
reason: their failure mode is accepting a document that silently means
something else. A general-purpose tar reader has to be permissive about
records it does not understand, and a skipped record is a file missing from
the image that nobody will notice is gone; a scanner hunting for `"Layers"`
in a byte stream finds it inside a string literal too, and an image built
from the wrong layer order is not the image that was asked for. Both refuse
what they do not understand and name it.

**DEFLATE is not hand-written**, and the line is worth stating because it is
the same rule read the other way: inflate has no such failure mode. It either
produces the bytes or it does not. A decoder written here would buy nothing
and risk a class of bug this project has no way to detect, so `flate2` with
its pure-Rust backend is the one dependency in the crate that does real work.

### The assumption that was wrong, and how it was caught

The refusal for a compressed layer originally read: "`docker save` writes
layers uncompressed; a compressed one comes from a registry pull." That was
written from memory and it is false. Docker 29.1.3 on this machine writes an
OCI layout whose `blobs/sha256/…` layers begin `1f 8b` — every one of them
gzip-compressed. The refusal would have rejected every real archive the tool
exists to read.

It was caught by running the thing against a real image before writing a test
about it, which took two minutes. What decides now is the bytes rather than
the name: a blob named by its digest says nothing about its encoding.

### The oracle

`docker export` flattens an image's layers itself, and comparing the two
flattenings is the strongest check available. Against a four-layer image
built on `alpine:3.20` — with a directory deleted in a later layer, a file
replaced, another file whiteouted out of the base, and a hardlink — the two
agree on **524 paths**, mode for mode and byte count for byte count. The
seven differences are all files `docker export` adds because it exports a
*container* rather than an image: `.dockerenv`, `/dev/{console,pts,shm}`, and
the bind-mounted `/etc/{hosts,hostname,resolv.conf}`.

That check is `baker/examples/image_differential.rs` rather than a test,
because it needs a docker daemon and the rest of the suite needs nothing but
a compiler. **This is a deliberate line and it is worth naming**: a test that
needs a daemon is a test that stops being run, and one that quietly passes
when the daemon is missing is worse than no test. What runs in the suite is
ten deterministic archives built with `tar` — whiteouts, opaque directories,
layer order, hardlink records, PAX owner names and xattrs, a gzipped layer,
the refusals — plus a differential between the two bake front ends over the
same tree. The daemon check stays runnable, and its numbers are recorded
here.

### Still open in M3

Nothing. The milestone's scope is built.

Two guards have no test that reaches them through real input, and both say so
where they are written: an extended attribute larger than 64 KiB, and an
image region larger than four gigabytes. No filesystem these tests run on can
produce either — ext4 keeps a file's whole attribute block in one block — and
the tarball path can now carry both, so they are reachable in principle and
the inputs that would do it have to be built by hand rather than by `tar`.
The arithmetic is checked directly.

---

## 2026-08-28 — M4: the writable world

### The type change that came first

An overlay is not an image, so the mount table stopped holding images and
started holding a `Filesystem` — a two-variant enum, closed rather than a
trait, so the compiler is what notices when a third kind appears. Everything
above it is written once: resolution, `stat` and `getdents64` do not know
which kind they are walking.

The root mount is now always an overlay with an empty upper, which means
every one of the existing tests runs through the merge. That was the point of
doing it that way: the read path's behaviour is pinned by eighty-odd tests
and a differential, and if the merge broke any of it, it would have said so
immediately. It broke one thing, and the allocation test caught it.

**The merge cannot allocate.** The first version built a `Vec` of names per
merged directory and derived a directory's size and link count from it on
every `inode()` — which put an allocation on every path resolution, and the
allocation-free assertion failed with 5,500 allocations over 600 calls. The
design's claim is load-bearing: the filesystem is 80% of a real application's
syscalls. So the merge is a cursor over two sorted sequences, and a
directory's size and link count are *stored*, refreshed when its entries
change. That is what every filesystem does, and it was the obvious answer
once the test refused the other one.

### What copy-up needs, and when it can have it

A file has no parent pointer. The index gives one to directories and leaves
the field meaningless for everything else, because a file can have several
names and a directory cannot — which is the same fact that makes hardlinks
possible.

So a file can only be copied up by *the name it was reached through*. That
name is in hand at `open` and gone by the time a descriptor is all that is
left. Copy-up therefore happens at open-for-writing, which is also where
kernel overlayfs does it, and the reason is the same one.

The consequence is stated where it bites: `fchmod` and `futimens` on a
descriptor opened read-only, whose file is still in the image, have no way
back to a name. Linux allows both; this cannot, and says so by name rather
than answering something plausible. Storing the name in every open file
description would fix it and would put an allocation on every `open`; the
milestone that needs it can make that trade with a caller to justify it.

### The bug the tests found

`is_empty_directory` indexed the upper arena with whatever number it was
given — including a *lower* inode number, which the high-bit encoding meant
was some unrelated upper node or nothing at all. `rmdir` of a directory that
had never been written to was the path that reached it, and the "recreate a
directory and check it is opaque" test is what ran that path.

The fix is not just the one function: `node()` now refuses a non-upper number
outright. A wrong answer with no symptom is what the encoding invites, and
one check at the accessor is what makes "this number is upper" a fact rather
than a convention every caller has to remember.

### `EXDEV`, measured rather than assumed

The plan's acceptance asks for the directory-rename case "asserted against
native kernel overlayfs behavior (fixture: a real overlayfs mount where
available, golden expectations where not)". An unprivileged overlayfs mount
is not available here — Ubuntu's AppArmor blocks unprivileged user
namespaces — but a *container's root filesystem is one*, so the question can
be asked directly:

```
$ docker run --rm debian:bookworm perl -e '…'
rename(/usr/share)                 Invalid cross-device link
rename(/fresh)                     ok
rename(/usr/bin/env)               ok
rename(/fresh2)                    ok
```

A lower directory is `EXDEV`; an upper directory, a lower file, and a
non-empty upper directory are all fine. All four are asserted, and the
measurement is recorded beside the assertions so that "matches overlayfs"
stays a claim with evidence rather than a claim with a citation.

The first run of that test failed, which is what it was for: renaming a
*file* from the image went through the directory copy-up path and got
`EINVAL`. Rename now copies up by name like everything else.

### Decisions

**A container has no clock, and timestamps say so.** New files get a counter,
not a wall time. `clock_gettime` is what will answer the time when M6 asks
it; a timestamp that claimed to be now would be a plausible wrong answer. The
one caller that depends on timestamps — a `.pyc` against its source — needs
only that a file written later compares later, and a counter gives that. The
differential compares timestamps a caller *set*, exactly, and excludes ones
nothing set, because comparing those would compare the two clocks.

**The `EXDEV` measurement missed a case.** The four cases recorded above —
lower directory, upper directory, lower file, non-empty upper directory —
did not include a lower directory that has been copied up because something
was created inside it. Kernel overlayfs still answers `EXDEV` there;
this did not, because the guard asked `is_upper`, which becomes true the
moment anything is written inside a lower directory. The condition is
whether the directory still reads through to the layer below. Corrected
after the review, with the measurement extended.

**`/dev/random` and `/dev/urandom` are the same device.** Linux stopped
distinguishing them in 5.6, and a container whose `/dev/random` blocked would
hang programs that still prefer it out of habit.

**Extended attributes are not copied up.** Nothing can write one — `setxattr`
is `EROFS` — so a copied-up file has exactly the attributes it was copied up
with, which is none. `xattr_count` says zero rather than reading the lower
file's stale reference.

**A mount with no writable layer is `EROFS`, and that is now a fact about the
mount rather than about the image.** `/proc` refuses writes; the root accepts
them. `access(W_OK)` asks the same question and gets the same two answers,
which is why the read differential's exclusion of `W_OK` was the right call
earlier: it was comparing the fixture's mount, not the kernel's answer.

---

## 2026-08-28 — what the M4 review found

One reviewer, on the whole milestone. Fourteen confirmed defects, every one
of them reproduced against this machine's kernel or in a container running
real overlayfs rather than argued from memory. The shape is worth recording
because it is not the shape M3's review had.

**No 32-bit truncation defects.** That was M3's largest class — every
memory-safety finding was a cast that is harmless on the 64-bit host the
tests run on and wrong in the module that ships. The reviewer went through
every cast in the M4 code and found none. The rule written at the top of
`kisal::image` after M3 — checked at full width, narrowed only after the
range is known to fit — held.

**The new class was "the last component".** Five rows changed a symlink
instead of what it points at, because the parent walk stops one component
short by design and the callers had already resolved the target and thrown
that answer away. `open` for writing handed back a descriptor on the link,
so `write` answered `ENODEV` and `O_TRUNC` did nothing at all — a silent
success, in the row the milestone calls load-bearing for `.pyc` staleness.
Nothing in eighty-six tests, the write corpus or either differential ever
opened or changed a file *through* a symlink. A whole category of path was
untested, and every test in it would have failed.

**Three defects were the same mistake in different places: acting before
checking.** `writable_parent` copied a directory up before anything was
validated, so a failed `unlink` or an `EEXIST` from `mkdir` renumbered the
parent's `st_ino` — the number `find -xdev`, `du` and every cycle check use
to decide two paths are one file. `rmdir("/dev")` removed a mount point and
left the mount attached to a vnode nothing could reach. `rename("/a",
"/a/b")` spliced a directory into its own subtree and detached everything
below it. All three now check first and change afterwards.

**`set_executable` stacked mounts instead of replacing them.** Resolution
crosses *into* a mount, so the second call saw the synthetic filesystem's own
root rather than the directory it covers, and `Mounts::replace`'s replace
branch — whose doc names this as its one caller — was dead code. The mount
count climbed 3 → 5 → 7 → 8, then hit the table's limit, and the error was
swallowed by a `let _ =`: `/proc/self/exe` stopped changing with nothing said
anywhere. M6's `execve` runs more than once in a real container.

**A cookie that is a position stopped meaning anything.** M4 made directory
listings a merge computed on demand, so removing an entry shifts every entry
after it down one — and a scan resuming at a stored position skips whatever
moved into a slot already consumed. `rm -r` is readdir and unlink
interleaved. Scans now resume by *name*, which does not move; the name is
held inline in the descriptor table rather than in a `Vec`, because
`getdents64` is on the path this design promises is allocation-free and the
first version of the fix put three hundred allocations on it.

**The ChaCha20 test did not test the ChaCha20 that runs.** It re-implemented
the ten double rounds inline over a hand-built state and never called
`block`. Corrupting the constant *inside* `block` left it passing, and every
test downstream with it, because those only compare one keystream against
another — they are invariant under any deterministic change to the cipher.
The vector's nonce is non-zero and `block` had none, which is why the test
was written the way it was; `block` now takes the two nonce words as a
parameter that the stream passes zeros for, and the published output is
reproduced by the function that ships.

**One over-claim of mine, corrected.** The `EXDEV` measurement recorded four
cases and the documents presented it as parity with overlayfs. It missed the
one that mattered — a lower directory copied up because something was
created inside it, which overlayfs still refuses and this allowed.

### On mutation testing

The fixes above are each covered by a test, and each test was checked by
breaking its fix. That is the last time it will be done as a matter of
course. Mutation testing is a technique for settling a specific doubt about a
specific claim, and this project had drifted into applying it to everything —
which is expensive, and which is how a batch of thirteen mutations at once
produced a test that hung rather than failed and cost several minutes of
wall clock to notice. The hang was a second defect: a scan loop in a test
with no bound. It has one now.

### Closing the review's unconfirmed findings

The five the reviewer reasoned about but did not reproduce end to end, and
what each cost to settle.

**A large write offset would have killed the instance.** Contents are a flat
buffer, so `pwrite(1 byte, offset = 1 TiB)` asked the allocator to
materialise the hole below it — and `Vec`'s ordinary growth *aborts* on
failure, which inside a wasm module is a trap that takes the container. Two
guards now: a maximum file size checked arithmetically before any resize,
answering `EFBIG` as Linux does when a write exceeds what a filesystem can
represent; and `try_reserve`, so a request the allocator cannot meet becomes
`ENOSPC`. Both are testable without allocating anything, because the refusal
happens before the allocation is attempted — which is also the point.

**The quadratic listing was real, and is measured rather than argued.**

| entries | list before | list after |
|---|---|---|
| 200 | 324 µs | 17 µs |
| 1000 | 8.6 ms | 83 µs |
| 2000 | 35 ms | 217 µs |

Two separate costs, and they had different fixes. Recomputing a directory's
stored size and link count on every change walked the whole listing, so
filling a directory was quadratic: those counts are now adjusted by what the
one name did, with a binary search of the layer below. And `getdents64` asked
the filesystem for the entry *at a position*, which restarts the merge every
step: it now takes a cursor and walks it once per call. What is left is one
restart per buffer-full, which is a real cost and a small one — four calls
for two thousand entries.

**Copying a file up breaks its hard link, and now says so.** Writing the test
for the link count found something worse than the finding: the map that gives
a copied-up node its identity was keyed on *any* lower inode, so after
copying up one name of a hardlinked file, every other name resolved to the
copy too. An image is full of hard links — every busybox applet is the same
binary — so writing through one name became visible through all of them. That
map is now directories only, which is safe for exactly the reason it was
unsafe for files: a directory has one name.

**Two `utimensat` edges**: a null path with `AT_SYMLINK_NOFOLLOW` is `EINVAL`,
there being no final component to decline to follow; and `UTIME_OMIT` checks
the mount accepts changes before reporting the success it did not perform.

**The arena.** A create that could not be linked left a node behind; the
directory is checked before anything is pushed.

The other half — that the upper layer never gave anything back — I first
wrote up as a design question outside M4's scope. That was wrong on both
counts, and the argument I gave for it was worse: I said freeing a node
needs reference counting because a descriptor may hold it. What the
descriptor table actually needs to answer is "does any open description name
this node", and it has at most a thousand of them; that is a scan, on a path
that runs at `unlink` and `close` rather than anywhere hot. "Outside M4's
scope" was wrong too — the milestone's scope is "upper memory store in
kisal's heap", and a store that never reclaims is a defective store rather
than an unbuilt feature.

It is built. What made it more than a scan was a second question underneath
it, and it is worth recording because the shortcut was tempting: my first
argument leaned on "an upper node has exactly one name", which was true only
because `link` was not implemented. Building reclamation on that would have
left a trap for whoever added `link` later — unlinking one name of a pair
would free bytes the other name still reached, with no symptom. So `link`
came first.

### `link`, and why its absence was worse than it looked

`link` and `linkat` had names in the syscall table and no dispatch row, so a
guest that called one did not get an errno — the container *died*. The
milestone excluded them because the trace this design was built from is
CPython and Flask, which never call them. That is a statement about one
workload. `git` calls `link` in `finalize_object_file` and falls back to
`rename` when it fails — but a fault is not a failure it can catch. The
NFS-safe locking idiom is create-link-unlink. `cp -l`, `ln`, ccache,
`rsync --link-dest`, Maildir delivery.

They are rows now. The upper layer holds real hard links — two entries
pointing at one node — and `nlink` counts names, which is what reclamation
turns on. A file still in the image is copied up first, after which both new
names name the copy and the image's other names still resolve below; that is
kernel overlayfs with its index feature off, and the same rule that makes
writing through one name of a hardlinked image file break the link.

`link` does *not* follow a trailing symlink unless asked — the link itself
gains a name — which is the opposite of every other row that changes
something, and the reason `AT_SYMLINK_FOLLOW` exists.

Building it immediately exposed the trap it was meant to prevent, in a place
I had not thought about: `rename` was implemented as unlink-then-link, so a
renamed file lost its only name for an instant, was marked reclaimable, and
had its bytes freed out from under the name it had just been given. The test
suite caught it on the first run. Removing a name and *moving* one are now
different operations, because they mean different things about the count.

### What reclamation does and does not do

An unlinked node's *contents* are freed when no name and no descriptor reach
it — at the `unlink` if nothing has it open, at the `close` of the last
descriptor otherwise, which is the POSIX rule stated directly. Thirty-two
rounds of write-four-pages-and-delete leave the layer exactly the size it
was after the first.

The node's *slot* is deliberately not reused. A stale number — in a
descriptor, a mapping, a saved working directory — would then name a
different file, silently, which is the class of bug two reviews have been
spent removing. The gain would be sixty-four bytes per deleted file; the
contents are the megabytes, and those come back.

---

## 2026-08-28 — M5: the address space

The milestone the plan calls mechanical, and mostly it was — the decisions
were made in the design doc and the work is interval surgery. Three things
were not mechanical, and two of them came from running the code against the
real kernel rather than from reading about it.

**The merge could not allocate, and the first version did.** Every existing
test runs through the address space now, but the one that mattered was the
allocation-free assertion: deriving a directory's size on every `inode()`
put an allocation on every path resolution and it failed with 5,500 of them
over 600 calls. Same lesson as M4's merge — a design claim that is asserted
is a design claim that gets defended.

**A page entirely past a file's end is not backed.** The native side of the
differential took `SIGBUS` on its first run. Linux zero-fills the *partial*
last page of a file mapping and leaves whole pages past the end unmapped;
touching one faults. Wasm has no faults, so kisal answers zeros there and no
implementation of it could do otherwise — the divergence is unavoidable and
is now written into the harness header rather than discovered later. What
the corpus checks instead is the partial page, which is the guarantee
programs actually rely on.

**`mmap` ignores unknown protection bits; `mprotect` refuses them.** The
differential found this too: `mmap(prot = 0x40)` succeeded natively and was
`EINVAL` here. Checked with a C program rather than fitting to the one data
point — `mmap` accepts even `0x80000000`, `mprotect` answers `EINVAL` to
`0x40`, and `PROT_SEM` is accepted by `mprotect` and does nothing. Both rows
now match, and the native test says which is which and why.

**`/proc/self/maps` names its files by searching for them.** A mapping is
made from a descriptor, and a file does not know its own name — the index
gives a parent pointer to directories only. Keeping the name at `open` would
work and would put an allocation on every `open`; searching costs a walk
when something reads the file, which a process does once or twice. The
trade is stated where the search is.

### The `brk` exclusion

`brk` is in the native tests and deliberately not in the differential. The
native side of a differential runs inside the test process, whose heap *is*
the break — moving it would move glibc's allocator out from under the
harness. It is covered where the arena belongs to nobody else, and the
harness header says so rather than leaving a gap for someone to notice.

## 2026-08-28 — M6: the linked-ELF front end and the exec path

The scope correction the build plan records — that consuming a *linked*
executable is not deferrable, because a musl-static CPython is one —
turned out to run further than "the reader learns a second input shape".
It reaches the translator's operands, the data segments, the jump-table
rewrite, the bake's memory layout, and the kernel's boot path, and the
thread through all of it is one sentence: **a linked executable's
operands are addresses, so the only place its bytes can be right is at
those addresses.**

### What the two shapes actually differ in

A relocatable object has no addresses. Every reference is a relocation
naming a symbol, and the linker decides later where that symbol lands.
A linked executable has already decided. So the three places an operand
becomes a number needed separate answers, and one of the three I got
wrong first:

- **Program-counter-relative operands** resolve to a *section offset*,
  not an address, because the decoder runs with the instruction's offset
  within its section as its program counter. The address the guest sees
  is that offset plus wherever the loader put the section. The first
  version emitted the displacement unchanged, on the reasoning that a
  linked input's operands "are already addresses" — true of the file,
  false of what `iced` hands back.
- **Call targets** need nothing at all, for the same reason from the
  other side: a relative branch resolves within its own section, so the
  offset is right in both shapes. A branch I added to look up the callee
  by virtual address was removed.
- **Jump table entries** have to be rewritten, and that is where the
  shapes genuinely part company. See below.

### The main binary *is* loaded now, and the arenas had to move

`container-plan.md`'s address-space section opens with "a simplifying
fact first: the main binary is never loaded." That was written when the
main binary was always relocatable, and it stopped being true here. A
linked program's segments are copied to their virtual addresses, which
linear memory can do because linear memory is ours.

Except that those addresses are *low* — a `-no-pie` x86-64 executable's
first `PT_LOAD` is at `0x400000` — and `wasm-ld` places module data from
`1024` upward. Three collisions, none fixable at run time:

1. `__image_blob` holds every file in the image, a hundred megabytes for
   a real one. It covers four megabytes long before it ends, so the
   bytes the program must occupy are the image the program is being read
   out of.
2. The `brk` and `mmap` arenas are carved from the top of what the module
   occupies at boot — above the module's data, and therefore above the
   program. `brk` then walks straight through the program's text.
3. Reading the program in order to load it allocates tens of megabytes,
   and the allocator takes those pages from the end of memory. Its arena
   moves past `0x400000`, and placing the program then overwrites the
   live allocator that made the read possible. This one only bites at a
   binary size big enough to matter, which is the worst way to find out.

"Carve the arenas around the program" cannot fix any of them, because
the program's base is *below* the module's data rather than above it:
the region is taken before kisal runs an instruction. So the bake
decides instead — `baker::layout` sets `--global-base` above everything
the program occupies, and everything downstream of the data is then
above the program by construction. A container with nothing to load
keeps the linker's default, because reserving a region for a program
that does not exist would cost every relocatable-tier container a hole
it has no use for.

kisal checks the result rather than assuming it, against `__global_base`
— the linker's own symbol, so the check compares what the bake decided
with what the program needs, at the one moment when saying so is still
useful.

### Two defects the end-to-end run found, that nothing else would have

Both were invisible to every narrower test, and both were found the
first time a whole program actually ran.

**The exec map wrote its table slots as constants.** Every object
numbers its own table entries from one, and the linker renumbers them as
it merges the tables. The map is *data*, so a slot written there as a
constant stayed whatever the translated object called it — and in a
container that number belongs to the seam, whose `kisal_yield` takes the
first slot. Entering the program threw instead of running it. The
symptom was perfect: `x86_slot_of(entry)` returned 1, no syscall was
ever dispatched, and the kernel reported a thread that left without
exiting. The slots are relocations now.

**Nothing applied the translator's image patches.** A linked jump
table's entries are rewritten so the dispatch computes `table + arm`
whatever form they held — that is what makes the translated dispatch a
`br_table` over an arm number. For a relocatable object those bytes are
in a data segment the module carries, so the translator rewrites them
itself; for a linked one they are in the program, and the program
reaches the guest through the image. So the rewrite has to reach the
image, which makes it bake-time work: `baker::program::apply` maps each
patch's virtual address back to the file offset that holds it. Without
it the guest ran correctly until its first `switch` and then went
somewhere else.

The general lesson is the one this project keeps relearning: a value
that is *data* rather than an instruction operand does not get the
linker's attention unless something asks for it. Both bugs are that.

### `.eh_frame`, which turned out to be worth more than planned

The plan scoped code discovery to `.symtab` plus FDEs and deliberately
excluded stripped binaries as later hardening. Reading `.eh_frame`
properly gave the stripped case for free, so it is in: a zero-size
symbol takes its extent from the matching entry instead of being a hard
error, and an extent no symbol covers becomes a function named after its
own address. `a_stripped_executable_still_has_functions` checks that
stripping changes nothing — the same extents come back, from the other
witness.

What it cost was reading the CIE properly rather than assuming the
common shape. The augmentation string decides the pointer encoding; a
personality routine's address has to be stepped over by exactly its own
width or the `R` encoding read next is a byte of that address; and an
indirect encoding names a slot a dynamic loader fills, which is refused
outright because a guessed extent silently translates the wrong bytes.

### Decisions

**One number is written on both sides of the seam.** The kernel cannot
throw — a wasm exception unwinds wasm frames without running Rust's
drops, so a throw raised inside a kisal frame would leak whatever that
frame owned. So the kernel returns a sentinel and the seam turns it into
the throw. Everywhere else the two sides meet, a disagreement is a link
error because it is a signature; this one would be silent, so
`the_leave_sentinel_agrees_across_the_seam` is what keeps them equal.
The kernel also asserts that no completed syscall ever returns it, which
is what makes the sentinel unambiguous rather than merely unlikely.

**Starting a process and scheduling a thread are the same act.** M6
added no catch of its own: `x86_run_thread` already enters a slot under
a `try_table`, which is exactly what boot needs. A trampoline added
earlier in the milestone was deleted once that was clear — what the boot
path actually needed was the address-to-slot lookup made nameable across
the link, one exported function instead of two.

**The catch now restores the shadow-stack pointer.** The design named
this obligation and nothing was paying it: the seam moves the pointer
into the kernel's fixed region and restores it when the syscall returns,
and a syscall that *leaves* never returns. Every leave was stranding it
inside the kernel's own frames. It bit nothing yet because M6 throws
once and returns to the host; it would have bitten M7 on the second
thread.

**Only `%rsp` is written at boot.** Every other register a Linux process
starts with is zero, and a wasm global starts at zero — so the kernel
needs one accessor rather than a copy of the machine image's layout,
which is the layout it would otherwise have to restate and could
silently disagree about.

**A function's symbol is now optional.** A function found in the unwind
table and not the symbol table has no name, which means nothing outside
the object can call it: it binds locally and gets no host wrapper. That
is not a special case bolted on, it is the same rule local functions
already followed.

### Still open in M6

The acceptance ladder has not been climbed: this runs a corpus program,
not musl BusyBox and not CPython. The baker does not yet drive the
translator over an image's ELFs — `baker::program` and `baker::layout`
are the pieces that pass will use, and the boot test assembles them by
hand in the meantime. The syscall grind (`uname`, `prlimit64`,
`sched_getaffinity`, `getrandom`, `rseq`, the clock rows, `readv`/
`writev`, the recorded signal rows) has not started, and neither has the
strace-diff harness or the determinism check.

## 2026-08-28 — what a real binary said, and the policy it forced

No musl toolchain is installed, and a static *glibc* one builds and runs
here, so it got used as a stand-in to see what a real binary demands.
The plan targets musl deliberately; what follows is partly a measurement
of why.

### Two front-end defects, found in the first two seconds

Neither was reachable from the corpus.

**A function with no stated size.** `deregister_tm_clones` and its
`crtbegin.o` neighbours carry neither a `.size` nor an unwind entry, and
they are in every binary gcc links. The symbol table still says where
the *next* thing starts, and a function cannot run past that — so that
is the third witness, after the symbol's size and `.eh_frame`.

**A program-counter-relative operand means different things in the two
shapes.** In a relocatable object, one with no relocation could only
have been resolved by the assembler against the section it was
assembling, so it names a function in it. In a linked object *nothing*
has a relocation, so the shape says nothing: the operand is an address,
naming data as readily as code. The corpus hid this by linking its
executables `-fno-pie`, which turns every global's address into an
immediate; a distribution's `gcc -static` still compiles
position-independently.

**And one design error of mine, corrected.** I had reasoned that because
the decoder runs with a section-relative program counter, a call's
target is an offset in the caller's section — true of the *number*, and
wrong about its *scope*. A relocatable object's assembler can only
resolve within one section; a linked object has had everything placed,
so `.text.unlikely` calls `.text` freely. The target is an address, and
the section's own base is what turns the offset into one.

### The policy: a function nobody can translate stops only itself

glibc ships AVX-512 string routines beside SSE2 ones and chooses between
them from CPUID, which the design curates to a baseline without AVX
precisely so the SSE2 paths are taken. The AVX bodies are still in the
binary. A translator that must render every byte it finds refuses every
real program over code that cannot execute — which is not a loud error,
it is a wrong one.

So `--trap-untranslatable` gives such a function a body that names
itself and stops, and lists every one on stderr. The default still
refuses, because for anything written to be translated a refusal is a
gap in the translator rather than a fact about the input. Reaching one
at run time goes through the kernel's own log and names the function,
so the answer is "implement this instruction" and not "a trap happened".

### What the measurement actually said

With both fixes and the policy, a static glibc `hello` translates in
56 ms — 1054 functions, 719 of them refused. The shape of the 719 is the
interesting part, because most of it is not an instruction list:

| count | cause |
| --- | --- |
| 338 | a branch into the middle of another function |
| 131 | a call to a `.plt` stub, which has no function symbol |
| 66 | `cmpxchg` |
| ~90 | AVX/AVX-512 (`vpxor`, `vmovd`, `vpbroadcastb`, `vmovdqu64`, …) |
| 18 | `xchg` |
| 16 | `pmovmskb` |
| rest | `ror`, `punpcklbw`, `bt`, `stosq`, `cpuid`, `rdsspq`, … |

The top two are worth naming precisely, because they are not gaps in
the translator:

- **The branches are real.** `_dl_start` states a size of 13 bytes and
  jumps 185 bytes past its own end, into `abort` at `+0xa7` of `0x1a3`.
  That is a branch into the middle of another function, which the design
  already calls out of scope — not an artifact of the extent inference
  above, since the symbol states its size.
- **The calls are static ifunc.** A static glibc still has a `.plt`,
  used to resolve `IRELATIVE` relocations at startup, and its stubs
  carry no function symbols. Discovering them is a separate piece of
  work with a separate design (the exec map already handles the indirect
  jump through the GOT; nothing yet makes a stub a function).

Both are glibc-shaped. musl has neither the hot/cold split at this scale
nor the ifunc fan-out, which is why the plan named musl as the interim
breadth target — and this measurement is the evidence for that choice
rather than a reason to argue with it. The instruction rows below the
top two (`cmpxchg`, `xchg`, `pmovmskb`, `ror`, `bt`, `stosq`, `cpuid`)
are real gaps that musl and CPython will want too, and they are the part
of this list worth working from.

### Correction: the 338 were not out of scope

The table above says 338 refusals were "a branch into the middle of
another function", and cites `_dl_start` jumping into `abort`. That is
wrong, and the way it was got wrong is worth keeping.

The reported offset is **section-relative**, because the lifter decodes
each function with its offset within its section as the program counter.
I read it as function-relative: added `0xb9` to `_dl_start`'s address
instead of to `.text`'s, landed inside `abort` by coincidence, and
generalised from that one sample to all 338 — then labelled the category
out of scope on the strength of it.

Measuring all 338 instead of one: **332 are a fall-through off the
function's own end**, six are something else. `.text` is at `0x401180`,
`_dl_start` is at section offset `0xac` with a stated size of 13, so it
spans `[0xac, 0xb9)` — and `0xb9` is one past its own last byte, not a
jump 185 bytes away.

What they actually are: a function whose final instruction is a call to
something that does not return. `__libc_message_impl.cold` is five bytes
and is one `call abort`. `_nl_load_domain.cold` is the same five bytes.
gcc emits nothing after such a call because control cannot continue, and
the graph builder asked where control continues.

`Terminator::Unreachable` is the answer, and it took the refusal count
from 719 to 522 and the branch category from 338 to 20. The remaining
worklist is mostly instructions after all — `cmpxchg` (93), `xchg` (32),
`ror` (27), `pmovmskb` (16), `stosq` (10), `punpcklbw` (8), `bt` (8),
`cpuid` (7) — plus the 180 static-ifunc PLT calls, which are real.

Two lessons, both of which this project has recorded before:

- **One sample is not a category.** Reading a single failure and
  extending it to 338 produced a confident, wrong claim about a third of
  a binary — and the claim conveniently made the work disappear.
- **"Out of scope" needs the same evidence as any other assertion.**
  Citing the design doc's out-of-scope clause for a category I had not
  measured is exactly the deferral this worklog keeps recording. The
  design's clause is real; it just did not apply to these.

### The atomic read-modify-write family, and what "atomic" means here

`xchg`, `xadd` and `cmpxchg` were the largest instruction gap on the
reachable path — 43 of the 380 functions a static glibc `hello` can
reach, including `__pthread_mutex_lock`, `__pthread_rwlock_rdlock` and
`__calloc`. They are one shape (read the destination, compute, write one
or both operands back) and differ in which operand receives what and
which flags survive:

- **`xchg`** writes no flags at all. Both operands are read before either
  is written, because they can name the same register and a swap that
  read the second one afterwards would read what it had just written.
- **`xadd`** writes an addition's flags, and gives the *source* the
  destination's old value — the half that makes it an exchange rather
  than an add.
- **`cmpxchg`** compares the accumulator with the destination and writes
  one of them: equal, and the source replaces the destination; unequal,
  and the destination replaces the accumulator. The flags are the
  comparison's either way, which is what makes `ZF` the answer to "did
  it take".

**They are not atomic, and that is a property of the scheduler rather
than of the translation.** On x86 `xchg` against memory is atomic
whether or not a `lock` prefix is present, and `xadd`/`cmpxchg` are when
prefixed. What is emitted here is an ordinary sequence of loads and
stores — no `i64.atomic.rmw.*`, no wasm atomics at all.

That is sound *only* under the model the design already commits to: one
instance per process, one linear memory, and threads that switch only at
syscalls. Nothing can run between the load and the store, so nothing can
observe the halves apart. Two things would break it, and neither is on
this plan's path:

- **Preemption at arbitrary points.** The designed poll insertion goes on
  CFG back-edges and function entries, never inside one instruction's
  translation, so that particular door stays closed — but a future that
  polls anywhere would open it.
- **Real shared-memory wasm threads.** Genuine parallelism over one
  memory would need the atomic opcodes.

The comment naming this lives on `translate_exchange`, so the search for
"atomic" finds the place that has to change rather than a doc that
mentions it. **M7 must not widen the switching model without revisiting
these three.**

Two simplifications, both invisible to a single-threaded guest:
`cmpxchg`'s failure path does not write the destination back with its own
value (architecturally a locked RMW dirties the line), and
`cmpxchg8b`/`cmpxchg16b` are untouched and stay loud.

### Where the numbers went

Refusals on a static glibc `hello`, as each piece landed:

| refused (total) | reachable from `_start` | after |
| --- | --- | --- |
| 719 | 165 | the trap policy and the linked-call fix |
| 522 | 165 | `Terminator::Unreachable` |
| 421 | 138 | the atomic family |

"Reachable" counts only direct calls and jumps, so it is a lower bound.
It is the number that matters: the unreachable remainder is dominated by
AVX-512 string routines that curated CPUID means are never selected, and
the reachable set contains **no AVX at all** — which is the evidence for
that curation argument rather than an assumption of it.

What is left on the reachable path is about six pieces of work, not 138:
72 static-ifunc PLT stubs (one piece), `ror`/`rol` (13), `bt`/`bts`/`btr`
(9), `cpuid` (7, which the design already says to implement and curate),
`stosq` (6), and a short tail — `rdsspq`, `fld`, `stmxcsr`, two genuine
`hlt` abort paths, and four calls to a suspicious `0x0` that have not
been looked at.

## 2026-08-28 — the x87 crate: softfloat with the hardware as oracle

The `x87/` workspace crate exists and is green: the extended-precision
softfloat core (add/sub/mul/div/sqrt/`fprem`/`fprem1`/`frndint`/
`fscale`/`fxtract`, all conversions, both compare families), the
register stack with TOP/tags/FCW/FSW and masked stack faults, the env
and fnsave images, the RC-sensitive constants, the f64-backed
transcendental four, and the `x87_*` FFI symbols over one static —
`crate-type = ["staticlib", "rlib"]`, same arrangement as kisal, and
the wasm32 staticlib builds. 47 unit tests plus a 14-test host-FPU
oracle: ~1.3M random-operand comparisons against real hardware
instructions demanding bit-identical results *and* flag words, across
all four rounding modes and all three precision-control settings, in
0.31s (volumes are sized by measurement, and say so in the file).

The oracle earned its place in the first run. Seven of fourteen tests
failed against the spec-from-memory implementation, and every failure
was the hardware teaching something the manual does not say plainly:

- **Denormal-flag suppression.** #D is *not* sticky-accumulated
  alongside a higher-priority pre-operation exception: denormal ÷ 0
  raises ZE alone, denormal vs SNaN raises IE alone — but
  denormal + 0 and denormal + ∞ keep #D. Probed across the class
  matrix; the rule (invalid or zero-divide suppresses denormal) is now
  in the special-case arms, with the probe date cited.
- **The store family raises no #D at all.** `fst`/`fstp`/`fist(p)` of
  an extended subnormal: UE|PE only. #D belongs to loads and
  arithmetic.
- **`fscale` ignores precision control.** Full significand preserved
  under PC = single; the exponent add is exact, only range effects
  round.
- **`fprem`'s partial step is 32 + (D mod 32) quotient bits**, leaving
  a multiple of 32 for the next pass — measured by matching the
  hardware's partial remainders for D = 64..133 and 6670 against the
  shift-subtract loop, and squarely inside the manual's
  "implementation-dependent number between 32 and 63".
- **`fprem` canonicalizes a pseudo-denormal it returns unreduced**:
  exp field 0 with the integer bit set comes back as the equivalent
  exponent-1 normal.
- **`f2xm1` outside [−1, 1]** returns the operand unchanged with PE —
  "undefined" in the manual, definite on the die.

The constants (`fldpi` and family) were measured before implementation:
all four rounding modes on the host, which confirmed the internal
values are wider than 64 bits and RC-sensitive, and cross-checked the
embedded 128-bit significands. The f64-backed transcendentals measure
their divergence rather than assume it: worst observed 4.5k ulps of
extended (f2xm1), 4.2k (fpatan), 3.6k (fyl2x) — inside the ~2^11–2^12
band a 53-bit core predicts, asserted under 2^14, printed on every run.

Not yet built, in order: the translator lowering
(`src/translate/x87.rs` + symbol plumbing beside `syscall_entry`), the
corpus differentials (`long_double.c`, the `fprem` asm loop), and the
build-plan appendix's later rows (`fsin` family, MMX, `fxsave`,
unmasked delivery — the tier table in `x87/src/lib.rs` is the
tracker).

## 2026-08-28 — x87 lowered, a static glibc long double, and the ledger

The `x87` crate is now reached by translated instructions, and two static
glibc programs run end to end through the container path. What follows is
the state of the x87 plan, what fell out of it, and — at the end — every
open thread in one place, because several of them were found by accident
and would otherwise be lost.

### The x87 plan, X1 through X5

**X1–X3** (symbol plumbing, the lowering module, the staticlib in every
link) are done. `src/translate/x87.rs` is ~900 lines: 59 helpers, one
`FunctionTranslator` method per instruction shape.

Three things the lowering had to get right, each of which cost a bug first:

- **Which operand carries the stack index is not constant.** `ffree st(2)`
  and `fld st(2)` have one operand and it *is* the index; `fxch st(1)`,
  `fcom st(2)` and `fcmovb st, st(2)` have two, of which the first is
  always ST0; `fcompp` has none and means `st(1)`. The last operand is the
  index in every form that has one. Reading operand 0 is right about
  `st(1)` by accident, which is why it survived until something asked
  about `st(2)`.
- **`DE E1` is `fsubrp` and `DE F9` is `fdivp`.** The AT&T spelling is
  reversed from the opcode.
- **A helper must not run on the guest's stack.** This is the kernel seam's
  rule and not the interop thunk's, and the distinction is the one SysV
  draws: a foreign *call* may eat its caller's red zone, so a thunk hands
  the callee the guest's own stack pointer. An x87 instruction is not a
  call, and a compiler will keep a `long double`'s bytes in the 128 bytes
  below `%rsp` across one. `x87_helper_stack` is that region;
  `red_zone_across_helpers` is the fixture that reads a value back across
  two x87 instructions and was off by a helper frame until it existed.

**X4** is done, and was done twice. The first pass covered what gcc and
clang emit, which is a little over half of what the lowering implements.
`x87_coverage.s` is the other half — the eight conditional moves, the
compares that answer in the condition codes rather than the flags, the
integer-operand arithmetic at both widths, four of the seven constants,
`ffreep`, `fnop`, `fnclex`, `fninit`. Both fixtures were checked by
breaking them: with the operand index read from operand 0,
`compare_registers` fails; with `fcmovnbe` mapped to `ae`,
`move_if_not_below_or_equal` fails. A fixture that large passing first
time is not evidence on its own.

`f2xm1`, `fyl2x`, `fyl2xp1` and `fpatan` are deliberately excluded from the
differential. The crate backs them with f64 and *measures* its divergence
from the hardware rather than matching it, so a bit-exact comparison
against native is the one test they must not be given.

`x87_control.s` gained what X4 named: the `fprem`/`fprem1` loops in the
shape musl's `fmodl` writes them — the C2 protocol end to end, which
terminates only if the partial-step rule is right — plus `fscale`,
`fxtract` in both halves, `fxam` over every operand class,
`fincstp`/`fdecstp` walked all the way round, `fnsave`/`frstor` and
`fnstenv`/`fldenv` round trips with a clobber between, and the
control-word round trip `fesetround` would have emitted, written by hand
because the corpus links no libc. The denormal class had to be *made*
rather than passed in: a subnormal `double` arrives normalised through
`fldl`, so it is scaled into the extended format's basement in the
register instead.

`x87_lowering.rs` asserts the two properties a differential structurally
cannot see: that the helpers are imported with the types the crate
defines, and that nothing is flushed between two adjacent x87
instructions. The second corrected itself — the `global.set` in that gap
is the linker stack pointer being switched off the guest's stack, so the
test asserts it is *there* as well as asserting nothing else is.

### X5 — the gate

Two static glibc programs, `gcc -static -O2`:

| program | result |
| --- | --- |
| `puts("hello")` | prints, exits 0 |
| `strtold` + `printf("%.21Lg")` ×3 | byte-identical to the same binary run natively |

21 significant digits is past what a double-backed answer can fake: an
extended significand carries a little over 19.

Refusals on a static glibc `hello`, continuing the table above:

| refused (total) | reachable from `_start` | after |
| --- | --- | --- |
| 421 | 138 | the atomic family |
| 237 | 3 | x87, the saturating family, wide division |
| 187 | 3 | `ConditionalLeaveOrFallOut` |

The three are `_dl_runtime_resolve_fxsave`, `_dl_runtime_resolve_xsave`
and `_dl_runtime_resolve_xsavec`, which a *static* program stores a
pointer to and never calls — as both programs running to completion
demonstrates. They need `fxsave`/`xsave`, which is X7c.

**The reachability number changed meaning twice, both times because the
tool was flattering itself.** `examples/refusals` originally followed
direct calls only, which stops at `_start` — `main` is handed to
`__libc_start_main` as a pointer — and reported the whole program
unreachable, which reads as good news. Then it followed address-taken
edges but not the edge from a piece that runs off its end into the one
below, so every piece of a split function after the first was invisible;
it reported three reachable refusals for a program that then trapped in a
fourth. It follows all three kinds now, and the number is still a lower
bound: a function reached only through a pointer computed at run time is
invisible to it, and an address-taken function counts as reached whether
or not anything calls it.

### Two instructions the gate needed that were not x87

**`psubusb`**, in `strtold`'s digit scanner, where the clamp at zero *is*
the comparison. Its whole family went in — all eight saturating forms,
because x86 and wasm have exactly the same eight.

**`div` at eight bytes.** It trapped whenever `rdx` held anything but the
sign, which the docs named as post-MVP work; glibc's `strtold` scales a
mantissa by dividing a value that genuinely occupies both registers, so
post-MVP was the wrong answer. `x86_divide_128` is one function per
module: restartable long division a bit at a time, behind a fast path for
the dividends that do fit. The two-digit Knuth recurrence would be quicker
and is not worth it — the fast path takes nearly every division, and the
loop can be read and checked against the machine in a way the recurrence
cannot. The remainder deliberately does not come back from it:
`dividend − quotient × divisor` is exact in the low 64 bits, because the
true remainder is smaller than the divisor. `wide_division.s` is assembly
because no C expression builds a 128-bit dividend — a compiler emits `div`
only after `cqo` or a zeroed `rdx`, which is exactly why a translation
handling only that case passes every test written in C.

### The two binaries, and what putting a command line on the pipeline found

`zaqaru-bake` and `zaqaru-run` are what the build plan has been calling
for — baker as "the bake tool: translator driving, the final link", runner
as test support that "graduates to a binary".

    $ gcc -static -O2 hello.c -o hello
    $ zaqaru-bake hello -o hello.wasm
    $ zaqaru-run hello.wasm

Two failures immediately, neither of which any test could have found.

The first was in the host: boot entropy goes to `/iso/random/bytes/32`,
where the count is the last path segment, and the CLI wrote to
`/iso/random/bytes` and ignored the error. The guest's `getrandom` found
nothing, and glibc did what it does without entropy — seeded itself from
`clock_gettime`, three layers from the cause.

The second was real. **A conditional branch that leaves the function was
assumed to continue inside it when not taken, and splitting makes that
false.** A piece cut in front of another piece can end with one, and then
both edges leave: the taken one to wherever it names, the untaken one into
the piece below. It is glibc's `memcpy` — the size check against the
non-temporal threshold is the last instruction of a split piece and its
fall-through is the first byte of the next. So `puts` of a 41-character
string trapped where `puts` of `"hello"` did not, and every "branch into
another function is out of scope" refusal in a static glibc binary, all
fifty of them, was this one shape. `Terminator::ConditionalLeaveOrFallOut`
is that shape and there are now none.

### `clock_gettime`

A native process never issues it — glibc reads the clock out of the vDSO
in userspace, which is why it appears in no `strace` and why nothing
noticed it was missing until a container went looking. There is no vDSO
here and `AT_SYSINFO_EHDR` is absent from the auxv, so glibc takes the
syscall path it keeps for that case.

`/iso/time/realtime_ns` and `/iso/time/monotonic_ns`, read on every call
rather than counted here: a container's sense of time is something its
host grants. Nanoseconds as a decimal integer rather than the isotope
spec's ISO 8601 and counter forms — every caller is `clock_gettime`, which
wants a `timespec`, and going through a formatted date would mean parsing
a calendar in the kernel to produce a number the host already had. This is
the ns-typed extension the design proposes.

A container whose host mounted no `/iso/time` has no clock, and asking
what time it is fails rather than being answered with the epoch. Same
capability decision as entropy, for a stronger reason: a plausible wrong
time is a certificate that verifies, an expiry that has not passed, and a
log that says something untrue.

### An overclaim, recorded because the pattern is the point

Two programs passing became "a working C program with glibc" in how this
was reported. Two programs is two programs. The refusal count was 187 and
the reachable count was per-binary — a measurement about *those* binaries,
reported as a property of the system. The correction: those two programs
run; arbitrary C does not, and every new program finds the next missing
thing. That is what this document has been calling the grind, and the
honest unit is one binary at a time with no claim attached until it runs.

Also recorded: the X6 change below (`kisal/src/machine.rs`,
`kisal/src/exec.rs`) was swept into commit 95cb177 by a `git add -A`, and
that commit's message says nothing about it.

### The ledger — everything open, in one place

**x87, from `docs/x87-plan.md`:**

- **X6 is wired and unproven.** `Machine::reset_floating_point` is called
  from `Kernel::exec`, importing `x87_reset` on wasm. Its stated
  acceptance — a container that execs twice, the second seeing FNINIT
  state — is *not buildable*: there is no `execve` syscall row, and
  `Kernel::exec` is reachable only from `kisal_boot`. A fresh instance
  also already starts in FNINIT, so the reset is unobservable on a first
  exec either way. What exists is the call site and the fact that every
  container link must now resolve `x87_reset`.
- **X7a** `fsin`/`fcos`/`fsincos`/`fptan` — C2-partial protocol for
  |x| ≥ 2⁶³, `fptan` pushes 1.0, ≤1 ulp at extended via double-double
  reduction, oracle by ulp tolerance.
- **X7b** `fbld`/`fbstp` — ten-byte packed BCD, m80-style by address.
- **X7c** `fxsave`/`fxrstor` and the sigframe render. **This is what the
  three remaining reachable refusals in every static glibc binary need.**
- **X7d** MMX. **X7e** unmasked-exception delivery.

**Instructions a real program has already asked for and not got**, from
the refusal tail of a static glibc binary using `perror`, AVX excluded
because curated CPUID never selects it:

| instruction | count | what wants it |
| --- | --- | --- |
| `packuswb` | 1 | `main`, once `perror` is in the program — blocks every error path |
| `pcmpistri` | 3 | SSE4.2 string search |
| `bzhi` | 3 | BMI2 |
| `sfence` | 2 | the non-temporal `memcpy` tail |
| `pshufb` | 2 | SSSE3 |
| `pminud` | 2 | SSE4.1 |
| `xsave`, `xsavec`, `fxsave` | 3 | the `_dl_runtime_resolve_*` trio |

**Syscalls a real program has already asked for and not got:**
`rt_sigprocmask` (14), reached through `abort()`; and `execve`, which
nothing can reach because there is no row for it.

**Undiagnosed, and the most serious item here:** a **stack-canary
mismatch**. A program with three `timespec` locals, a `volatile` delay
loop and `perror` on its error paths reaches `__stack_chk_fail` →
`abort()`. The same program without `perror` runs correctly, and a
canary-carrying program with no clock call also runs correctly. So it is
neither the canary machinery alone nor the clock alone. Not yet
understood, and it is a correctness bug rather than a missing feature.

**Test debt:** `tests/glibc_boot.rs` currently carries a clock test that
fails — its program uses `perror`, so it is blocked on `packuswb` and the
canary question, not on the clock. `clock_gettime` itself is proven by
four kisal unit tests and by a container printing the same second as the
host.

**Milestone debt, from `docs/container-build-plan.md`:** M6's acceptance
ladder (musl BusyBox, CPython), the strace-diff harness, and the
determinism check are untouched.

## 2026-08-28 — the grind, driven by programs instead of by guesses

The ledger above was written at `817eee8`. Five commits later most of it is
closed, and everything closed was found the same way: by running an ordinary
C program and seeing where it stopped. That turned out to be worth far more
than reading the refusal list, because the refusal list is a statement about
one binary and every new program takes a different path through the library.

### Two demos, which is what started it

    $ gcc -static -O2 hello.c -o hello
    $ zaqaru-bake hello -o hello.wasm
    $ zaqaru-run hello.wasm
    hello from x86-64, running as WebAssembly

    $ zaqaru-run longdouble.wasm
    3.14159265358979323851
    9.86960440108935861941
    3.63082455165596093213

The second is 80-bit x87 arithmetic — `strtold`, a multiply, a divide, and
`printf("%.21Lg")` — running as softfloat, byte-identical to the same
binary run natively. 21 significant digits is past what a double-backed
answer can fake.

`zaqaru-bake` and `zaqaru-run` are the two binaries the build plan has been
calling for: baker as "the bake tool: translator driving, the final link",
runner as test support that "graduates to a binary".

### Four defects, each found by a program rather than by inspection

**A tail jump moved `%rsp`.** `jmp` does not touch the stack pointer and
the translation moved it eight bytes, so every tail-called function ran with
a stack pointer below the machine's and every stack-relative operand it had
was off by one slot. Nothing caught it because no fixture's tail-call target
read the stack — they all passed values in registers. What it cost: a
static glibc `main` with `perror` on its error paths is split at its canary
epilogue, because the cold path rejoins there; the second piece read
`0x48(%rsp)` expecting the canary, got the eight bytes below it, and
`__stack_chk_fail` fired on a program that had overflowed nothing.

The reservation existed so the callee's `ret` would have a slot to pop. It
already has one — the slot this function's own caller reserved, which is
exactly what the callee pops on the hardware. A guest tail jump is a wasm
tail call now; a resume body is the exception, because it answers with the
resume ID of the frame above while the guest function it transfers to
answers with nothing, and `return_call` requires those to agree.

**A conditional branch's untaken edge can leave too.** Splitting makes it:
a piece cut in front of another piece can end with a conditional branch, and
then both edges leave — the taken one to wherever it names, the untaken one
into the piece below. It is glibc's `memcpy`, where the size check against
the non-temporal threshold is a split piece's last instruction and its
fall-through is the first byte of the next. `puts` of a 41-character string
trapped where `puts` of `"hello"` did not, and all fifty "branch into
another function is out of scope" refusals in a static glibc binary were
this one shape.

**A linear sweep is wrong about where instructions begin.** After a call
that never returns there is no previous instruction, so what follows is
padding — and a decoder reads it as an instruction that can span straight
through the place a real one starts. glibc's `____longjmp_chk` is the case:
two branches target one offset and the padding after its
`call __fortify_fail` decodes into an instruction covering it. Decoding is
a fixpoint now — sweep, collect branch targets, re-sweep knowing the ones
that landed inside an instruction begin one. Undecodable bytes stopped
being fatal on sight for the same reason: the corpus fixture's padding
spells a `lock` prefix on a register operand, which no decoder accepts and
which killed the function before the first sweep could collect a single
branch target. A poisoned offset is now an error exactly when an
instruction that continues runs into it, which is the case where the decode
has lost sync and has to stay loud.

**Function discovery had three witnesses and needed a fourth.** A symbol,
an unwind entry, and where the next thing starts — and none of them
describes the crt fragments in `.init` and `.fini`, which carry no `.size`,
no unwind entry and, stripped, no symbol either. `_start` calls straight
into them. So: whatever a discovered function transfers to directly, that
lands in code and that nothing already covers, is a function. Plus the
initialiser arrays, which are the same kind of evidence from data —
`.init_array` and its siblings are defined by the ABI to hold pointers to
functions, and glibc walks `__init_array_start` and calls through them.

Both are direct evidence about a particular address. The line this does not
cross is worth stating because it was nearly crossed: scanning for
prologues, or for the `endbr64` that marks an indirect-branch target, finds
more functions and also invents them. `endbr64` is a *necessary* condition
for being an indirect target — a jump-table arm and a landing pad have one
too — and using it as a sufficient condition for "a function begins here"
is a different claim.

### What was implemented alongside

- **`clock_gettime`**, from `/iso/time/realtime_ns` and
  `/iso/time/monotonic_ns`, read per call. A native process never issues
  this syscall — glibc reads the vDSO — which is why it appears in no
  `strace` and why nothing noticed it was missing until a container went
  looking. A container whose host mounted no clock has none, and asking
  fails rather than being answered with the epoch: the same capability
  decision as entropy, for a stronger reason.
- **`rt_sigprocmask` and `tgkill`**, so a process can end itself. `abort`
  unblocks `SIGABRT` and raises it at itself precisely so nothing can stop
  it, and a failed `assert` — the most ordinary way a C program dies —
  could not die. The mask is honoured rather than pretended at: self-directed
  signals are the only signals there are.
- **`packuswb` and its family**, which is what `perror` costs, and so every
  error path in every program.
- **The eight saturating packed forms** and **`x86_divide_128`**, both
  demanded by `strtold`.
- **A filesystem in baked images.** `zaqaru-bake` put the program in an
  image and nothing else, so a container's whole filesystem was one file
  and `fopen` returned `ENOENT` — correctly, from a kernel that has an
  overlay and a VFS specifically to provide better. `--root` bakes a tree;
  without one the program gets `/tmp`, `/var/tmp`, `/home`, `/dev`,
  `/proc`, `/etc`.

### Where it stands: eighteen ordinary C programs

Written to find gaps rather than to pass. Compared byte-for-byte against
the same binary run natively.

| what | result |
| --- | --- |
| malloc/realloc/free, qsort, strings + `snprintf` | identical |
| libm: `sqrt` `sin` `log` `pow` | identical |
| varargs, struct-by-value return | identical |
| binary stdio: `fwrite`/`fread`/`fseek`/`ftell`, 4 KiB | identical |
| the `printf` format zoo, `%a` and `%e` included | identical |
| `gmtime` + `strftime` | identical |
| 2000-deep recursion, 256-byte frames | identical |
| `strtol`/`strtod` with `errno` and endptr | identical |
| overlapping `memmove`, both directions | identical |
| 8 MiB allocation, memset, strided read | identical |
| file I/O round trip | identical |
| failed `assert` | prints the same, exits 134 |
| `argv`/`getenv` | differs only in `argv[0]` and an empty environment, both correct |
| `setjmp`/`longjmp` | **blocked**, and see below |

### busybox: further than expected, and stopped somewhere specific

A stripped, statically linked 2.1 MB busybox now parses (2262 functions,
0.47s), bakes, boots, and runs `_start` → `__libc_start_main` → its
constructor. It stops on an indirect call through `0x511aa5` — a real
function reached only through a pointer table, almost certainly the applet
table.

That needs a fifth witness, and it is a genuine design question rather than
a missing afternoon's work. **`.eh_frame` coverage stops at `0x50a970`**,
leaving roughly 640 KB of `.text` with no unwind entries at all. In that
region a function reached only through a pointer has no witness among the
four. The available candidate is scanning data for values that land in
`.text`, which is not direct evidence the way the other four are, and whose
bad case is real: a false positive landing in a gap bounds an undiscovered
function short and silently cuts its body off. Worth designing.

### The ledger, updated

**Closed since the last one:** `packuswb`; the stack-canary mismatch, which
was the tail-jump defect; the decode class that refused `____longjmp_chk`;
`abort` and its two syscalls; a filesystem in images; `clock_gettime`;
dynamic relocations stopping a parse.

**Open:**

- **setjmp/longjmp**, the parked thorn. It now fails exactly where
  container-plan.md predicts: `longjmp` jumps to the return address
  `setjmp` saved, and that slot holds the sentinel. The sketch there — a
  third arm of the standing discrimination, so the saved "PC" is already a
  materialized continuation — is a shape, not a design.
- **The fifth witness**, above.
- **`sfence`, `pshufb`, `pminud`, `bzhi`, `pcmpistri`** — all in string
  routines curated CPUID never selects, so none is reachable today.
- **`fxsave`/`xsave`/`xsavec`**, which is x87-plan X7c and which is what the
  three remaining reachable refusals in *every* static glibc binary are.
  They are the `_dl_runtime_resolve_*` trio, which a static program stores a
  pointer to and never calls.
- **x87 X6** wired but unprovable without an `execve` row, and **X7a, X7b,
  X7d, X7e**.
- **`execve`**, which nothing can reach because there is no row for it.
- M6's acceptance ladder beyond this point (CPython), the strace-diff
  harness, the determinism check.

### Two corrections, recorded because the pattern is the point

Reporting two passing programs as "a working C program with glibc" was an
overclaim, and the correction is in the entry above this one. The related
error is worth naming separately: the demo those two programs were asked
for was built from a *new* `hello.c` with a longer string rather than from
the verified one, and the longer string took a different `memcpy` path.
Every defect in this entry was found because of that substitution, which
does not make the substitution right — the request was to demonstrate what
already worked.

And: when the `endbr64` approach was questioned and withdrawn, this said
the edit had been rejected and nothing written. It had already been
applied, and had displaced a neighbouring doc comment on the way in. It was
found still in the file afterwards. A tool call that is stopped is not
evidence that its effects were not applied.

## 2026-08-29 — D1: the busybox unwind hole is in the binary, not the parser

`docs/code-discovery.md` opens with a gate: before building a weaker
witness to cover a 640 KB hole, check whether the hole is real. Ghidra
reaches near-total function recall on Linux binaries from `.eh_frame`
alone, so a hole that size is unusual enough to be a parser bug first and
a fact second.

It is a fact. The verdict, with the numbers:

| | |
| --- | --- |
| `src/eh_frame.rs::frames` on the stripped busybox | **2040** frames |
| `readelf --debug-dump=frames \| grep -c FDE` | **2040** |
| `.eh_frame_hdr`'s own count | **does not exist** — see below |

The parser agrees exactly with an external reader, so D1's first outcome —
a parse or layout artifact that would have shrunk everything below — is
ruled out.

The third cross-check the milestone asks for is unavailable rather than
skipped: this binary has **no `.eh_frame_hdr` section and no
`PT_GNU_EH_FRAME` program header**. It was linked without
`--eh-frame-hdr`, which is what a program that never unwinds at run time
gets. So there is no binary-search table to count against, and saying so
is the honest answer where inventing a third number would not be.

Coverage, per executable section, measured by `examples/frames`:

| section | bytes | frames | covered |
| --- | --- | --- | --- |
| `.init` | 27 | 0 | 0% |
| `.plt` | 336 | 0 | 0% |
| `.text` | 1,734,359 | 2040 | **61.8%** |
| `.fini` | 13 | 0 | 0% |

The uncovered 38.2% of `.text` is not scattered. It is **one contiguous
tail hole at `0x50ac21`, 646,198 bytes — 631 KiB, 37.3% of the section** —
plus 15,656 bytes spread over 1835 small interior gaps, which are the
inter-function padding a linker leaves and not missing functions. The
entry point at `0x410870` is inside coverage; the applet-table target
`0x511aa5` that stopped the run is inside the hole.

So the binary genuinely carries no asynchronous unwind tables for the last
631 KiB of its text, and **D5 is confirmed necessary**. `.init`, `.plt` and
`.fini` having no frames at all is expected and already handled — hand-written
crt fragments and linkage stubs are what the fourth witness and the
linkage-table witness are for.

One correction to the previous entry: it put the hole's start at
`0x50a970`, which was the last FDE *start* rather than the last FDE's
*end*. The hole begins at `0x50ac21`. It also reached that figure by
comparing against `0x5a8858`, which is `.fini` — `.text` does end one byte
below it, so the conclusion held, but by luck rather than by measurement.
`examples/frames` exists so the next such number comes from a tool.

## 2026-08-29 — what the population actually looks like

D1–D4 were built and validated against two binaries: `/usr/bin/busybox`,
and a family of `gcc -static -O2` programs I compiled myself, which are one
binary with different `main`s. The worklog already carries a correction
from the previous day saying two programs is two programs. This is the same
error, made again, one day later.

So: every ELF in `/usr/bin`, once, through the reader and discovery.
`examples/survey` is the tool, and its header says plainly that it is not a
test and must not become one — it is for the handful of moments when the
question is about the *population* of binaries rather than about a change.
Expect it to be archived once the population questions are settled.

### The finding that dwarfs the others

**1261 of 1291 ELF files — 97.7% — do not parse at all.** One error, every
time: *"expected a relocatable object (`gcc -c`) or a static executable,
found Dynamic"*. Nearly everything shipped is a dynamic PIE, and the reader
refuses it at the door.

That is the real distance to "the target is any binary", and no amount of
discovery work closes it. It was invisible from a sample of one static
binary and one static program I built myself.

### Among the 29 that do parse

All 29 are stripped. 26 carry `.eh_frame_hdr` and 3 do not, so busybox
lacking one is unusual — the D1 caveat was pointing at something real.

FDE coverage of `.text`, though, is bimodal rather than merely variable:

| | coverage |
| --- | --- |
| median | 97.9% |
| 25th percentile | 96% |
| **6 of 29** | **below 62%** |

And the six are the interesting part:

| coverage | binary | text | functions found |
| --- | --- | --- | --- |
| 0.0% | `containerd-shim-runc-v2` | 3.7 MB | 1170 |
| 0.0% | `gh` | 15.7 MB | 1259 |
| 0.0% | `glab` | 18.6 MB | 1339 |
| 0.1% | `ubuntu-report` | 3.4 MB | 1187 |
| 1.0% | `shellcheck` | 17.1 MB | 10483 |
| 61.8% | `busybox` | 1.7 MB | 3587 |

The first four are Go, which emits no `.eh_frame` at all — it carries its
own tables in `.gopclntab`. The fifth is GHC. **busybox is the mildest case
in the group, not the extreme one**, and the conclusion I drew from it —
"a 631 KiB hole is unusual, D5 is confirmed necessary" — was right about
the necessity and wrong about the reason. `gh` finds 1259 functions in
15.7 MB of text: one per twelve kilobytes, which is not a discovery result,
it is a discovery failure that nothing reported.

### A quadratic the sweep found immediately

`split_once` asked, for every function, about every branch target in its
section — a cross product. Fine on a corpus fixture; fatal on a real
program. `/usr/bin/python3.12`, which `docs/code-discovery.md` names as the
milestone target, **did not finish a single round in two and a half
minutes**. `arm-none-eabi-lto-dump` did not finish in three.

D4 is what tipped it over: tripling busybox's function count tripled one
side of the product.

The fix is the standard one — index the functions per section by start,
with a running maximum of how far anything up to that point reaches, and
walk back from the target only while something can still reach it. Exact,
not approximate: a target is contained only by functions starting at or
before it, and the walk stops as soon as nothing earlier is long enough.

    python3.12   never finished  →  0.78s
    busybox            0.47s     →  0.20s
    /usr/bin sweep   ~20 min+     →  17s for all 1291

The function lists for busybox and the static glibc hello are unchanged.

### What this changes about the plan

`docs/code-discovery.md`'s milestones stay right, and their *justification*
moves:

- **D5 and D6 are more necessary, not less.** The case is not busybox's
  partial hole; it is Go and Haskell binaries with no unwind information
  whatsoever, where the witnesses have almost nothing to work from and the
  saturated tier is the only thing that could make them run.
- **Dynamic PIE is upstream of all of it**, and calling it a "wall" — as
  the first version of this entry did — was wrong on the facts. It is not
  an unrecognised obstacle; it is a *sequenced* one, and the sequencing is
  written down. `container-build-plan.md` lists dynamic linking under
  "explicitly not in this plan, deferred to phase two with their designs
  already written in the design doc", and says of glibc that it "is not
  being dodged — it arrives with the dynamic tier, where the CPUID-curation
  and shadow-GOT designs exist precisely to meet it — it is being
  *sequenced*." `container-plan.md`'s "Dynamic linking and ld.so" section
  then designs it: prelink at bake, the shadow GOT, `DF_1_NOW` so
  `_dl_runtime_resolve` never runs, ld.so as ordinary transpiled guest
  code. `reader.rs` even carries the comment saying `Dynamic` is left out
  deliberately. I had read that file's mapping table, its x87 section and
  its setjmp thorn, and not its ld.so section, and wrote "not in this
  document's scope" about the thing it describes at greatest length.

  What the sweep does add, which the plan does not have, is the size of the
  tier. The plan's tier-one answer — build the image's binaries statically
  — is correct for an image we build. For images we do not build, static is
  **2.3%** of what ships. The dynamic tier is not an enhancement to the
  static one; it is most of the target, and that is an argument about
  sequencing rather than about scope.

  The split, for whenever it is picked up. *Reading* a dynamic ELF is
  small: accept `ET_DYN` as linked at a bake-assigned base, which is what
  "prelink at bake" already specifies, and read `.dynsym` — `read_symbols`
  takes `file.symbols()`, which is `.symtab` alone, and a stripped dynamic
  binary still carries `.dynsym` because linking requires it. Those 1261
  files are much less blind than a stripped static one, and D3's relocation
  harvest pays off far better there besides. *Running* one is the phase-two
  milestone as designed.

## 2026-08-29 — the dynamic tier: a PIE, ld.so and libc as one module

`gcc -O2 hello.c` produces a position-independent executable that cannot run
until something maps `libc.so.6` and patches its GOT. That something is now
glibc's own `ld.so`, translated ahead of time and running as ordinary guest
code. `tests/dynamic_boot.rs` is the gate.

This is `container-plan.md`'s "Dynamic linking and ld.so" built as written —
prelink at bake, one exec map, `mmap` of a translated ELF answering with the
address its code was translated at. It was listed in
`container-build-plan.md` as phase two; the sweep the day before is what
moved it, because static is 2.3% of what ships.

### The shadow GOT is not needed for correctness, and that was already true

The design gives the cross-DSO call three layers: discrimination, a generic
fallback through the exec map, and a shadow array as a fast path. Building
it turned up that **the generic fallback is the discriminating indirect call
that already exists**. A `jmp *GOT[n]` is an indirect transfer; in linked
mode every indirect transfer is an exec-map lookup; the GOT holds an address
the loader wrote, which is an address the bake translated at. Nothing was
needed. The shadow GOT stays an optimisation with a measurable baseline
rather than a prerequisite — which is worth recording because the design
does not say so, and reading it top to bottom leaves the impression that a
cache has to exist before a call can work.

### What was actually built

- **The bake takes a closure, not a file.** `baker::dynamic` reads
  `PT_INTERP` and `DT_NEEDED` and resolves them against the tree the image
  is made from, transitively, in load order. `/etc/ld.so.cache` is
  deliberately not read: it is a cache, it names files the search path also
  names, and trusting it would make a bake depend on the host's cache being
  right about the host's filesystem.
- **Every file gets a base, and they are translated as one unit.** Not
  because translating them separately is hard — because the *exec map*
  cannot be built separately, and in linked mode every cross-module call is
  a lookup in it. `ObjectFile::merge` is that unit; module-qualified
  function names fall out of it, and paid for themselves within the hour
  (see the backtrace below).
- **`kisal::exec` loads two files** — the program and its interpreter — with
  an auxv saying where each went, `AT_BASE` the loader's and `AT_ENTRY`
  still the program's, and enters the loader.
- **`mmap` of a translated ELF returns its prelink base.** The run-time half
  of prelinking. The bases ride in a new index region, eight bytes per
  module rather than per inode, with `EXEC_TRANSPILED` as the cheap test for
  whether to look at all. Mapping a file the bake did *not* translate with
  `PROT_EXEC` is now the loud error the design names.

### `ld.so` was the easy part

The design calls transpiling ld.so and glibc "the labeled grind". Measured
before assuming: **`ld-linux-x86-64.so.2` refuses five functions, all of
them the `_dl_runtime_resolve` trio** (`fxsave`, `xsave`, `xsavec`, and two
AVX `vmovdqa` forms) — which `DF_1_NOW` guarantees never run. `libc.so.6`
adds 211 more, none reachable. The whole of a dynamic hello — 4909 functions
across three files — translates in under a second.

The grind that did happen was somewhere else entirely: two discovery
defects and a jump-table defect, all in code that already existed and was
only exercised properly by real shared objects.

### Reading a position-independent file at zero destroys its evidence

Discovery on `ld.so` at base zero produced eleven address-taken functions
against three at a real base, and the eight extra ones shredded a region no
strong witness covered into pieces beginning partway through real
instructions. The cause is arithmetic rather than subtle: a shared object's
text starts a few kilobytes above its base, which at zero is exactly where
ordinary integer constants live, so `mov $0x1770,%eax` reads as an
instruction taking the address of code.

The first version of this guarded it in the reader — refuse `ET_DYN` at base
zero — and that was the wrong shape twice over. Zero is not the problem, low
is, so the check tested one value of a continuous property; and for
`ET_DYN` the base is *ours*, chosen at bake time, so the only way to get a
bad one is our own bug. The floor lives in `baker::layout::DYNAMIC_BASE`
now, where the choice is made, and `parse_at` documents what a low base
costs.

The mirror case — a *fixed* executable linked low — is one we cannot fix by
choosing, only refuse, and `MINIMUM_FIXED_ADDRESS` does. It is close to
vacuous in practice: `mmap_min_addr` forbids under 64 KiB and both GNU ld
and lld put `-no-pie` text at `0x400000`. An earlier version of this entry
justified the check with firmware and unikernels, which was invented — those
make no syscalls, touch `cr3` and page tables, and are not programs this
project is ever handed.

### Padding is not a function, whatever named it

Two defects, both about filler, both found by glibc:

- `is_padding` did not know the `cs`-prefixed multi-byte nops
  (`66 2e 0f 1f 84 …`), so functions were minted out of the space between
  real ones.
- **A strong witness can name something that is not an instruction.**
  glibc's signal-return trampoline carries an `.eh_frame` entry beginning
  one byte *before* `__restore_rt`, so that unwinding a signal frame — whose
  return address is the trampoline's first byte — finds an entry covering
  `pc - 1`. It is the unwinder's convention, not a mistake in the binary.

`docs/code-discovery.md` scopes the padding rule to weak witnesses, on the
argument that a branch target never lands on padding. That is wrong on real
input, and the filter now sits at both doors — accepting the FDE would
translate the tail of a `nop` as code, which is the silent failure the whole
design exists to avoid, where refusing it costs at most a loud miss on an
address nothing in a container transfers to.

It also sits in `placements`, because filler must not *bound* a neighbour
either: a padding candidate discarded after the extents were computed still
leaves the function before it ending in the middle of a `nop`, which the
lifter then refuses to decode. That one presented as "undecodable bytes",
three functions away from its cause.

### ELF names cannot be wasm symbol names

Two independent reasons one file names a thing twice. `.symtab` and
`.dynsym` are two views of the same code, and reading both — which a
stripped shared object requires, since it has only the second — sees an
exported function once in each. And symbol *versioning* puts one name at
several addresses: glibc ships `memcpy@GLIBC_2.2.5` beside
`memcpy@GLIBC_2.14`, with the version in `.gnu.version` rather than in the
name.

Exact duplicates are dropped; a name at several addresses is qualified by
address — every occurrence rather than the later ones, because which copy is
"first" depends only on the order two symbol tables happened to be read in.

### The index's length was the end of whichever region came last

Adding the prelink-base region made the container die with an *empty* kernel
log, which is a specific and useful symptom: every failure path in
`kisal_boot` reports before it panics, except the ones inside the kernel's
own construction — where there is no kernel yet to report with. So an empty
log names the image.

`index_length` read `xattr_offset + xattr_size`, true until a region was
added after it. The slice came back eight bytes short, `Image::parse`
refused it, and the panic had nothing to say. It now takes the end of
whatever region ends last, because a length that must be updated when a
region is added is a length that will not be.

Found by bisecting rather than by reading: the library path passed its
tests, the tool failed, and stashing the working tree proved the difference
was mine before any theory got attached to it. Three of the theories tried
first were wrong.

### A computed goto measures from a label, and its dispatches get merged

The one that took longest, and the only one that was a real design gap
rather than an oversight.

`fprintf` died at `libc+0x6a090`, an address inside `__vfprintf_internal`.
The backtrace named it, and named the path to it, because functions are
qualified by module now:

    kisal_no_function_at
    x86_slot_of
    libc.so.6!fn.0x100b9080_guest
    libc.so.6!fn.0x100bb500_guest
    libc.so.6!__printf_chk_guest
    /init!main_guest

Two things were wrong, one behind the other.

**First: entries measured from a code label.** glibc's printf writes
`&&label - &&do_form_unknown`, so the dispatch is

    lea    table(%rip),%rsi
    lea    base(%rip),%rdi      ; a label inside the function
    movslq (%rsi,%rax,4),%rax
    add    %rdi,%rax
    jmp    *%rax

Reading those entries against the *table's* address — the only relative form
the recovery knew — gives targets that are not instruction boundaries, so
the table is not recognised at all and the dispatch becomes an indirect call
into the middle of a function. The base is not guessed: it is an address the
dispatch sequence itself computes, so every text address in the backward
scan is offered and the entry scan decides. The rewrite absorbs it —
writing `table − base + arm` makes the guest arrive at `table + arm` as
before — so the dispatch lowering needed no change, and a difference too
large for its entry is refused rather than truncated.

That fixed `fprintf` and not `printf`, which is how the second one showed
up.

**Second: `gcc -O2` merges identical tails, and the tail of a computed goto
is `jmp *%rax`.** `__vfprintf_internal` has **six thirty-two-entry tables,
all measured from one label, and twenty-nine jumps between them**. A given
jump is reached from several paths that each loaded a different table, so
attributing a table by proximity is not merely unreliable — there is no
single right answer. Eighteen of the twenty-nine had been given the same
table.

The failure has two modes and neither is loud. Subtracting the wrong address
gives an arm number that is sometimes outside the table, where it falls back
to the exec map and reports a miss on an address that was never a function;
and sometimes *inside* it, where it branches to the wrong arm and says
nothing. The second is what made this worth fixing properly rather than
sharpening the heuristic.

So an arm space is a property of what the dispatch measures *from*, not of a
table. Every table sharing an origin contributes its arms to one list, each
dispatch branches over the whole list, and each table's entries are
rewritten to index into it — after which which table a jump was handed stops
mattering, because any of them names an arm of the same space. Which is what
the hardware was doing all along. In the relocatable pipeline every table's
origin is its own address, so each group holds one table and nothing moves.

`zaqaru --dump` grew `--at` and learned to print what each table measures
from. That is how "six tables, one base, twenty-nine jumps" became a fact
instead of an inference — three earlier theories about this failure were
wrong, and all three would have survived another round of reading.

### Where it stands

A dynamic program that *uses* its library runs, output byte-identical to the
same binary run natively: `qsort` through a callback from the executable,
`malloc`/`free`, `snprintf` with `%s`/`%d`/`%.3f`, a libm call, `strlen`,
and a file written and read back through the overlay.

Open, and none of it blocking the tier:

- **The shadow GOT**, as the optimisation it turns out to be.
- **`dlopen`** — untried. Baked libraries should work by the same path; a
  library that arrived at run time is the named loud error.
- **A second library.** Everything so far resolves to one `libc.so.6`;
  `-lm` on this system is absorbed into libc, so the multi-library closure
  is exercised in code and not yet by a program.
- **`ld.so.cache`.** The baker does not regenerate `/etc/ld.so.cache` as the
  design says it should. Nothing needed it, because the loader's search path
  finds the files at the paths the bake placed them — but an image that
  ships a stale cache naming a file that is not there has not been tried.
- **Breadth.** Three dynamic files read closely and fifteen more parsed in a
  spike is not a population. The `/usr/bin` sweep at a real base is the
  measurement, and it is cheap; it has not been run.

## 2026-08-29 — binaries we did not compile, and the cost of the suite itself

### Three stripped coreutils

`/usr/bin/true`, `/bin/echo` and `/usr/bin/uname` run. Every dynamic program
before them was compiled by the test at flags the test chose, which is a
weaker claim than it sounds — these are whatever the distribution shipped,
stripped, linked by someone else's toolchain, with the packager's `crt`
files. Four gaps between the two, each surfaced by running one of them:

- **`DT_INIT`'s function had no witness at all.** `.init` holds `_init`; a
  stripped binary gives it no symbol and `crti.o` gives it no unwind entry,
  so nothing saw it — while the loader calls it before `main` every run. The
  ABI defines `.init` and `.fini` as holding exactly one function each
  beginning at the section's start, which is strong evidence available in
  any file with section headers. All three died at the first byte of their
  own `.init`.
- **Dynamic relocations were never harvested.** The read walked
  `section.relocations()`, which finds nothing in a linked file: `.rela.dyn`
  is not attached to a target section the way a relocatable object's
  `.rela.text` is. So D3's best witness was silently absent from every
  position-independent file, and a PIE embeds an `R_X86_64_RELATIVE` for
  every pointer it holds — exactly the addresses no instruction names.
  `main`, handed to `__libc_start_main` through a GOT slot, is the first one
  every program needs. The tell was in a histogram printed hours earlier:
  `libc.so.6` showed **zero** `FileStated` functions, which is impossible
  for a PIE, and I read past it.
- **A function can fall out through alignment.** `__memcpy_chk` ends at
  `0xba96d`, three `nop` bytes follow, `memcpy` starts at `0xba970`, and the
  first really does fall into the second. Asking only about `0xba96d` found
  nothing and emitted a trap where the program has a path. The fall-out
  target is now the next function after the gap when everything between is
  filler; skipping *code* to reach it would invent control flow, so that
  stays a trap.
- **`uname`**, an M6 grind row, was the only thing between `/usr/bin/uname`
  and printing `Linux`.

### The suite was costing more than the work

The regression set had reached about 105 seconds, and I had run it four
times in an afternoon while proposing to add a fourth dynamic case to it.
The arithmetic that makes this a defect rather than an annoyance: two
minutes buys thirty iterations an hour, ten seconds buys hundreds, and the
difference is paid on every change for the life of the project.

**Measured before restructuring, and the measurement moved the answer.** The
same work takes 0.87s to bake and 1.97s to run through the release binaries
against ~25s inside a test — so the bottleneck was not the tests' shape but
the *build profile*. `Cargo.toml` carried no `[profile]` sections at all, so
every run used debug builds of `iced-x86`, which decodes once per
instruction of every function translated, and of `wasmtime`, which compiles
every body of every container a test builds — fifteen thousand for a
dynamic-tier module.

    dynamic_boot   78.0s → 12.0s
    glibc_boot     20.1s →  2.6s
    boot            1.4s →  0.3s

Debug info is kept and `debug-assertions` is untouched: it is set
independently of `opt-level`, so every assertion and overflow check inside a
dependency still fires. That distinction came from being asked what debug
dependencies actually buy when a test fails — the first version of the
change also set `debug = false`, which would have cost line numbers in a
dependency panic for no runtime gain. The diagnostics this project actually
reads are wasm backtraces out of a trapping guest, which come from the guest
module's own name section and are unaffected by any of it.

Then the duplicated work: three C programs differing by ten lines, each
separately translating all of `libc.so.6`, linking 22 MB and handing it to
the engine. One container now, shared through a `OnceLock`, with a `#[test]`
and a name each and one labelled output line per case — six named cases
where there were four, in 8.0s instead of 12.0s. The whole set is ~15s.

What was deliberately not done: `#[ignore]` on the slow tests, which
improves the number by not running them.

### And, again

The edit adding the distribution-binary test was interrupted, reported as
"not written", and had in fact already applied — the file had the test and
it passed. The worklog records this exact pattern from the day before: **a
tool call that is stopped is not evidence that its effects were not
applied.** Twice now, and both times the recovery was cheap only because
something else printed the truth.

## 2026-08-29 — the applet table, and what a guessed extent is worth

`busybox echo`, `busybox cat` and `busybox ls` run, on the distribution's own
stripped static binary. That is the first rung of M6's acceptance ladder, and
`tests/applet_multiplexer.rs` keeps it.

Getting there took three syscall rows, one design refinement, and a defect
whose fix had already been written down and then cancelled three lines above
itself.

### `prctl` is sixty syscalls, and busybox wants one

The ledger named `prctl` as what blocked busybox, which made it sound like an
API to implement. Measured instead: busybox issues exactly `PR_GET_NAME`,
once per applet. So there is one row, and every other option faults *naming
the option* — a worklist entry rather than a dead end. Not `EINVAL`, which is
what Linux answers for an option it does not know: indistinguishable from
Linux for a program that probes, and silently wrong for the commoner one that
asks the kernel to do something and is told, in effect, that it did.

**The real blocker was argv**, which the ledger did not name at all. busybox
picks its applet from `argv[0]` and the bake hardcoded `/init`. The command
line is now a region of the image index — a fact about the container rather
than a file in its filesystem, the same argument the design makes about
`/iso` — written by `zaqaru-bake <program> -- <argv>...`.

### A call cannot split its own extent

`fn.0x5110e7` called `0x511147`, `0x60` bytes inside itself, and the
translator refused: a call into the middle of a function has no spelling.

D4 had written the rule that fixes it — "compilers do not call into the
interior of a function they emitted, so a call that appears to is evidence
that the extent, not the call, is wrong" — and the self-branch exemption
three lines above cancelled it. Where a weak witness guesses an extent across
an unwind hole, the function it swallowed *and* the call reaching that
function land inside the same guess, so the call looks internal.

The first fix broke every relocatable object, and the suite said so in three
seconds: a call's displacement there is a placeholder a relocation will fill,
so it decodes as a branch to the next instruction — inside its own body,
every time. Linked mode only.

### The half of D5 that mattered was not the scan

`cat` and `ls` then died at addresses inside `fn.0x554e47+0x1f584+0x48a0` —
busybox's applet table naming functions inside an extent a transfer target
had guessed across a 631 KiB unwind hole.

The scan itself is what the design specifies. What the design got wrong is
which axis the permission to cut turns on. It said *witness strength*, and
the two come apart: a symbol is a strong witness, but a symbol with no size
states a **start**, and its extent is then whatever begins next — a bound,
not a fact. So a function now carries where its extent came from, Stated or
Guessed, and weak evidence may cut a guessed extent and never a stated one.

That is the invariant refined rather than relaxed, and both halves are load
bearing. Refusing to cut a stated extent is what keeps a computed goto's 256
labels from shredding `_PyEval_EvalFrameDefault`, whose symbol states its
size. Allowing a guessed one to be cut is what lets 278 applet pointers name
functions inside a span nothing ever stated the length of — and without it a
gap-only witness finds *none of them*, because they are not in a gap.

Measured, and it corrected the document twice more. The applet table is 278
bare pointers at stride eight, where the design predicted structs and its
pitfall index said a stride-eight scan would miss it; so the scan is stride
eight, and the generalisation waits for a binary that needs it rather than
for a guess about one.

### Implemented rather than refused

`sendfile` would have "worked" as `ENOSYS`: busybox tries it and falls back
when a kernel lacks it. It is implemented anyway, because it is a call that
moves data, and a kernel answering "I do not have that" to something it could
do will be believed by the next program that has no fallback. The
distinction against `rseq`, which *is* refused for real: there is nothing
behind `rseq`.

It needed the one thing the write path lacked — a way to write bytes the
kernel is holding, since every other write moves bytes from guest memory.

### Where it stops

`busybox ls -l` misses at an address that appears in no data section as
either four or eight bytes, so it is computed at run time. That is neither
D5's business nor — as `docs/code-discovery.md` has it — D6's, because it
lands inside a covered extent. The obvious extension, now that extents carry
their standing, is that the saturated tier could serve instruction starts
inside *guessed* extents as well as residue. Written down, not built.

### The suite

Seven targets in 20 seconds, having gained two. It was 105 for five this
morning; the entry above records why.

### `ls -l`, and a witness collected then dropped

`busybox ls -l` works. Against native the only differences are three the
image explains: `total` counts blocks an image does not allocate, uid 1000
stays a number with no `/etc/passwd` to resolve it, and the time is UTC with
no `/etc/localtime`.

The gap was not a missing witness. `0x598bd0` is named by a `lea rdx` at
`0x5a4f87` — a callback passed as an argument — and that instruction sits
*inside a discovered function*, so D4 had harvested it correctly. It was
then dropped, because `placements` skips an address something already
covers, and this one is covered by a piece whose extent a transfer target
guessed.

So the refinement the applet table forced had been applied too narrowly:
only table entries could revise a guessed extent, when the argument is about
weak evidence and says nothing about which kind. An address an instruction
takes disputes a guessed bound exactly as much as a table entry does. The
document now states three tiers instead of two.

Finding it took the runtime backtrace, the translator's own dump, and a byte
scan of `.text` for an instruction naming the address — because **`objdump`
had desynced** at a bad instruction boundary and showed the calling piece
ending in `ret` with no indirect transfer in it at all. The translator's
dump showed the `call r14` that was really there. Worth remembering: a
disassembler starting at the wrong offset is confidently wrong, and the
piece boundaries this project computes are exactly the offsets it starts at.

The dump was also stale by 677 functions until rebuilt, which is the second
time in a day a tool answered about an older binary than the one under test.

### Two performance defects of one shape

The cut loop resolved every target's section per round of a sixteen-round
fixpoint, through a lookup linear in the ninety sections of a merged dynamic
program. Hoisting it helped a little. The real one was the same mistake I
had just written a comment about while introducing it: **the data-array scan
asked which section holds an address once per eight-byte word of every data
section** — hundreds of thousands of words against ninety sections.

    dynamic bake, before D5   0.87s
    with D5 as first written  2.03s
    with the ranges hoisted   0.70s

Faster than before the witness existed. The lesson is not "hoist loop
invariants", which everyone knows; it is that a linear scan over sections is
*fine* everywhere this project had used it before, because the section count
was fifteen — and merging three modules into one translation unit multiplied
it by six without any of those call sites changing. A merge changes the cost
of every question asked per section.

## 2026-08-30 — D7: the front end becomes a fixpoint, and CPython starts running bytecode

`docs/code-discovery.md`'s D7 as designed, plus one premise of that design
that measurement contradicted.

### What it is

Discovery decides where functions begin, lifting decodes them, jump-table
recovery reads the dispatches out of what was decoded — and the last pass
produces evidence the first one needed. An arm of a recovered `switch` that
lies outside the piece dispatching to it has to be a function, because a
`br_table` cannot branch into another function's interior; but an arm is an
*indirect* target, so nothing knows it is an arm until after the cutting is
done. `_Py_HashBytes` is siphash, its tail is `switch (len & 7)`, two of its
eight arms are past a cut, and CPython trapped on the first string it hashed.

`src/frontend.rs` owns the loop now, and `ObjectFile::parse_at` calls it —
so a function list a later pass would still revise is never observable.
`discover::cut_at_switch_arms` is the only door, cutting at transfer-target
grade with `Witness::SwitchArm` recording what made each piece. Bounded at
eight rounds, loud on non-settlement.

Measured, per module:

| | rounds | functions | arms fed back |
| --- | --- | --- | --- |
| `/usr/bin/python3.12` | 3 | 73,124 → 74,300 | 1,073, then 9 |
| `libc.so.6` | 2 | 4,596 → 4,605 | 9 |
| the other four modules | 1 | unchanged | none |

The whole bake goes 2.7 s → 3.2 s and the refusal count does not move.

### Witness collection is not re-run, and that is a fact rather than a saving

The design says the loop is over discover → lift → recover. What is actually
re-run is the *splitting* phase, because an arm can never establish a
function: `read_linked_table` refuses to read a table whose entries are not
already instruction boundaries of discovered functions, so an arm always
lands inside something and cutting is the only act available. Re-running the
strong and weak witnesses would produce the same functions — and after
`ObjectFile::merge` it could not be done correctly anyway, since
`data_array_targets` needs one base per module and a merged object no longer
separates them. Settling therefore happens in `parse_at`, per module, before
any merge; arms cannot cross a module because `read_linked_table` requires
them to be boundaries *in the dispatching function's own section*.

### The premise that was wrong

D7's safety argument said the feedback set stays empty for the case that
matters because "`_PyEval_EvalFrameDefault`'s 256 computed-goto labels are
all inside the dispatching piece (nothing ever split the eval loop)".

Measured on `/usr/bin/python3.12`: the eval loop is **779 pieces**, every
one of them cut by a direct branch, before any of this runs — and its cold
half is a second body of 293 more, found by an unwind entry and named by no
symbol. Nothing about the eval loop is unsplit.

What actually keeps it safe is the invariant the document already states,
which is about *establishment*: a data value equal to a covered function's
interior address never mints a function. Cutting is a different act — the
pieces tile the same bytes, and the transfers between them are explicit tail
calls. CPython's eval loop has been tiled that way since the first day a
linked binary was read here. The correction is folded into
`code-discovery.md`; the general lesson is the one this log keeps recording,
which is that a safety argument resting on a property of a binary needs the
binary measured, not described.

### The acceptance, and the fixture proven able to fail

`tests/corpus/split_switch.s` is the shape reduced to something a test can
reason about: one `switch` over four arms, a direct jump into the body's
middle from outside, two arms past the cut. It runs in a container and is
compared byte for byte and status for status against the same ELF executed
by Linux. `dispatch` states its own size, so what an arm cuts is a *stated*
extent — the evidence-grade claim made into something that fails if it is
retracted.

Checked by breaking it: with the feedback set forced empty, both positive
tests fail and the program dies where predicted, in `dispatch_guest`'s
`unreachable`. The third test — that a `switch` nothing split feeds nothing
back — correctly keeps passing under that break, which is what it is for.

The trap-closed assertion is on the feedback *set* rather than on output
bytes. An empty set says the fixpoint provably changed nothing; byte
identity would only say that it did not.

### Where it stops now

Past `_Py_HashBytes`, into CPython's own bytecode evaluation: 82 syscalls
in, through `Py_InitializeFromConfig` → frozen-module import →
`PyEval_EvalCode` → `_PyEval_EvalFrameDefault`. It stops there on
`kisal: 0x5d6760 is not the address of any translated function`, and that is
a different defect, diagnosed below rather than guessed at.

## 2026-08-30 — CPython runs bytecode: two defects between there and here

Both found by running the thing and reading the trace, which is the M6
appendix's own instruction and the only method that has worked on this
program.

### A dispatch table's arms land in two bodies

With D7 in, python got past `_Py_HashBytes` and stopped inside
`_PyEval_EvalFrameDefault` on `kisal: 0x5d6760 is not the address of any
translated function`. The eval loop's own computed goto —
`mov rax,[rax*8+0x7846e0]; jmp rax`, five copies in the one piece — was not
recovered at all, so it translated as an ordinary indirect transfer and
missed the exec map on an interior address the first time a bytecode ran.

Measured rather than guessed at. `opcode_targets[]` is 256 absolute
eight-byte entries at `0x7846e0` in `.rodata`. **181 of them are inside
`_PyEval_EvalFrameDefault` and 75 are inside a second body at
`0x494c2a`** — its cold twin, found by an unwind entry and named by no
symbol — and entry *zero* is one of the 75. `holds_block_addresses` asked
whether the **first two** entries name blocks inside the dispatching body,
so the answer was no on entry zero and the table was never read.

The rule is right and its operationalisation was too narrow: a function
pointer names a function's start and a table entry names a block inside the
body that dispatches, but gcc splits a function's cold blocks into a body of
their own, so the arms of one dispatch are spread across two — and which
arms end up where is a property of the numbering, not of the table. The test
is now that *two entries anywhere in the run* name blocks inside the body,
scanning while the entries continue to be instruction boundaries and
stopping as soon as it has its two. A real table costs a handful of reads; a
run of unrelated data costs one.

Two smaller things came with it. An entry equal to the *body's* start is now
excluded as well as one equal to the piece's, because an array holding the
dispatching function's own address is the function-pointer array being told
apart. And the old rule was quietly unstable under D7: it compares against
`function.offset`, which moves when a piece is cut, so a dispatching piece
that came to begin at one of its own arms would have lost its table between
rounds. The monotonicity guard would have caught it; the wider window makes
it not arise.

`cold_switch.s` is the shape — entry zero in a cold body, the rest inside
the dispatching one — run in a container against the same ELF executed by
Linux, and checked by restoring the old rule, which fails it.

### A descriptor is an `int`

Then CPython ran `getpath`, which is Python bytecode, all the way through —
and died on `OSError: [Errno 9] Bad file descriptor` at
`openat(AT_FDCWD, "pyvenv.cfg")`.

The trace made it one line, with the dirfd rendered as it arrived:
`0xffffff9c`. That is −100 in an `int`, zero-extended into a sixty-four bit
register, which is what a caller that writes `edi` leaves behind — and glibc's
`openat` wrapper writes `edi`. kisal compared it against `AT_FDCWD` at
sixty-four bits, saw a number that was neither `AT_FDCWD` nor a descriptor,
and answered `EBADF`.

Every `…at` row was affected, and **only for relative paths**: an absolute
path never looks at the descriptor, deliberately and with a comment saying
so. That is why the whole dynamic tier, `ld.so` and glibc and three shipped
coreutils, ran without noticing. Every existing test wrote `at::FDCWD` as a
Rust `i64`, which sign-extends, so no fixture ever produced the shape a real
caller produces — the M1 lesson exactly: an assertion that cannot tell the
right answer from the default one.

The narrowing is not merely the fix, it is the conformant answer: a
descriptor argument of `0x1_0000_0005` is descriptor 5 on Linux, because the
top half was never part of the number. One predicate now owns it, and the
one place `i32::try_from` was used on a plain descriptor was changed to the
truncating cast every other row already used.

### Where it stands

`python3.12` now boots through `ld.so`, initialises CPython, runs the frozen
`getpath` module as bytecode, computes its full path configuration —

    stdlib dir = '/usr/lib/python3.12'
    sys.prefix = '/usr'
    sys.path = ['/usr/lib/python312.zip', '/usr/lib/python3.12',
                '/usr/lib/python3.12/lib-dynload']

— and stops on `ModuleNotFoundError: No module named 'encodings'`, 252
syscalls in, because the image holds the interpreter and none of its library.
That is the build plan's Python appendix item 2, reached rather than
predicted. Worth noting against that appendix: it found its `stdlib dir`
from the compiled-in prefix without `PYTHONHOME`, so the environment
question really is secondary to placing the tree.

## 2026-08-30 — `python3 -c 'print("hello")'`, and the red tests that were there all along

M6's acceptance ladder, second rung. The distribution's own
`/usr/bin/python3.12` — non-PIE, dynamically linked, five libraries — baked
with its whole standard library and run under wasmtime:

    $ zaqaru-bake /usr/bin/python3.12 --root pyroot \
        --env HOME=/root --env PATH=/usr/bin:/bin --env LANG=C.UTF-8 \
        -o pyhello.wasm -- /usr/bin/python3.12 -c 'print("hello")'
    $ zaqaru-run pyhello.wasm
    hello

Byte-identical to the same binary run natively with the same environment,
exit 0, 494 syscalls. Six modules, 81,714 functions, 371 refusals, none of
them reached.

### The two unmeasured quantities, measured

The build plan's Python appendix named them: bake time and the engine's
compile time for an image of that size. Both are non-issues.

| | |
| --- | --- |
| image tree | 58 MB (54 MB of `/usr/lib/python3.12`) |
| bake | 4.0 s |
| container | 153 MB |
| boot, including wasmtime compiling the module | 5.9 s wall (1 m 40 s CPU) |

### What stood between here and there

**The eval loop's dispatch, then the tokenizer's.** Both recorded in the
entry above and the one before it; the rule ended up as *two entries
anywhere in the run that are blocks, at least one inside the dispatching
body*, where a block is an instruction boundary that is not where some body
begins. It took three attempts, each corrected by a binary rather than by
argument:

1. "the first two entries, inside this body" — rejected the eval loop,
   whose entry zero is in its cold twin.
2. "two anywhere in the run, inside this body" — rejected the tokenizer's
   six-arm dispatch, five of whose arms are in a cold region.
3. "any entry that is a body start disqualifies" — rejected three of the
   four dispatches in `unicode_compare_eq`, each of which has a cold arm
   that discovery gave a function of its own, beginning exactly at the arm.

An array of function pointers is told apart by holding *nothing but* body
starts, not by holding one.

**A table nobody recognised was being overwritten.** The worst of the lot,
because it had no symptom. `table_limit` bounded each table by the next
*recognised* one, so a 19-arm table in `.rodata` was read as 25 and swallowed
the tokenizer's six-arm table — whose entries were then rewritten as arms of
the first one's space. The second dispatch, still unrecognised, read a
rewritten entry and jumped into the middle of `.rodata`. Now every address an
indirect jump reads its target from bounds the table before it, recognised or
not; and two recovered tables that overlap are a build-time refusal, because
the failure they cause has no other symptom.

**A descriptor is an `int`** — the entry above. **`rt_sigaction` records**,
which is the build plan's own M6/M10 split: CPython installs its handlers
before it evaluates a line. `tgkill` now consults the disposition, so
`SIG_IGN` is ignored and a signal with a handler is a named fault rather
than a plausible death.

**The environment.** `python -c` with no `HOME` does not fail — it takes a
different path: `expanduser` falls through to `pwd.getpwuid`, which is
glibc's NSS, which probes nscd over `AF_UNIX`. Measured natively: with
`HOME` set, zero socket calls; with an empty environment, two and a read of
`/etc/passwd`. So a container with no environment has a *larger* surface
than the run it will be diffed against, not a smaller one. The image index
grew an environment region beside its command line (version 4), and
`zaqaru-bake --env NAME=value` records it. `LD_BIND_NOW=1` goes first now,
so a bake that records its own wins.

Two things fell out of that. The index header is written through a named
struct rather than as fourteen bare `u32`s in a row — adding a fifteenth
argument to that call was how a wrong image gets built silently, and one
caller already passed `xattr_offset, 0, 0, 0` where only counting told you
which zero was `root_inode`. And **`zaqaru-bake --root` no longer copies the
tree**: the copy was pointless (the translated files go into the in-memory
tree, so nothing was written back) and lossy (`std::fs::copy` follows
symlinks, so an image built from a real root filesystem arrived with every
symlink replaced by a copy of its target, and a dangling one refused the
bake outright). A root filesystem is a symlink farm; `Tree::from_directory`
already read them faithfully and the copy stood in front of it.

### Three tests that were red before any of this, and are not now

Found by running every target rather than the ones I had touched. All three
were the same collision: the dynamic tier made `mmap(PROT_EXEC)` of a file
the bake did not translate a named refusal, and M5's loader-carving fixtures
still asked for exactly that.

- `tests/corpus/memory.c`'s carving sequence and
  `kisal/tests/memory.rs::the_loader_carving_sequence_lands_the_right_bytes`
  now carve the *writable* segment, which is the half of ld.so's sequence a
  data fixture can honestly represent and the one whose protection differs
  from its neighbours', so the shape assertion still bites.
- `proc_self_maps_renders_the_tree_as_it_stands` reaches `r-xp` on a file
  line through `mprotect` afterwards, which is the only way this kernel
  reaches that state for an untranslated file — `mmap` refuses it and
  `mprotect` records whatever it is asked, exactly as the design says.
- The refusal itself had no test at all. It has one now, in the kernel's own
  suite rather than in a differential, because Linux simply succeeds: this
  is kisal's policy, not a conformance question.

And `tests/memory_differential.rs` did not print the kernel log on a trap,
which every other container test does. Its backtrace named `kisal_syscall`
and nothing else; the log named the mapping in one line.

### One real gap the new corpus exposed in the *relocatable* path

`cold_switch.s` swept as a `gcc -c` object and was refused, because the
relocatable recogniser had the same hot/cold weakness the linked one had
just lost — it asked whether the *first* relocation named a block inside the
dispatching function. Both branches now ask the same question.

Fixing it surfaced a latent silent bug beside it: a relocatable table's
targets are section offsets carried without a section, and `resolve_entry`
did not check which section a relocation resolved into. An arm in
`.text.unlikely` — where gcc puts cold blocks in a relocatable object —
would have had its offset read against `.text` and branched somewhere
arbitrary. Such an arm is not representable until targets carry their
section; stopping the run is the honest answer, and it is now what happens.

### Where it stops

`import json` — the first line of any real script — dlopens
`lib-dynload/_json.cpython-312-x86_64-linux-gnu.so` and gets the named AOT
fault, exactly where the build plan's dlopen appendix says it will:

    kisal: unimplemented syscall mmap (9): an executable mapping of a file
    the bake did not translate, whose code therefore does not exist in this
    module

That is the next rung, and its design and spike ladder are already written.

## 2026-08-30 — `import json`: the unit of translation is the image

The rung after the checkpoint, and the bake gap was the whole of it.

A `dlopen`ed library is in nobody's closure — the executable does not link
against it, so walking `PT_INTERP` and `DT_NEEDED` from the program never
reaches one. `baker::dynamic::sweep` walks the image tree for ELFs and
translates every one beside the closure. Python's tree has 52 shared objects
in it; the bake goes from 6 modules and 81,714 functions to 58 and 111,652,
and from 4.0 s to 4.5 s. The per-section cost this was warned about did not
materialise at that scale.

    $ zaqaru-run pyjson.wasm
    {"a": 1, "b": [2, 3]}

Identical to native. So is `math`, `cmath`, `re`, `collections`,
`itertools`, `functools`.

### Two defects in the placement, both mine and both from the same hour

**A file already in the tree was being orphaned.** `place` added a new node
and linked the module's name to it. For a closure module read from outside
the tree that is right; for a *swept* one, which was found in the tree, it
leaves every other name for that file — a hardlink, or the symlink farm a
libc arrives as — pointing at the old node, which is then a second copy of
the same library, unpatched and untranslated and reachable by name. A file
the image already holds is updated where it is now.

**And then the first version of that overwrote a symlink's body while
leaving its mode saying symlink.** `Tree::resolve` walks entries without
following links, and an executable's `PT_INTERP` says
`/lib64/ld-linux-x86-64.so.2`, which on every modern distribution is a link
into `/lib/x86_64-linux-gnu`. The image ended up holding an inode of one
type carrying another's contents, and the container died with "the dynamic
loader its `PT_INTERP` names is not in the image" — which was true, and
said so, one layer away from the cause. Only a regular file is updated in
place; anything else falls through to replacing the name.

### The thorn, met on purpose

`dlopen` of something absent leaves `ld.so` by `longjmp` out of
`_dl_catch_exception`, and `longjmp` jumps to the return address `setjmp`
saved — the sentinel. `container-plan.md` predicted exactly this and asked
for the corpus to trigger it once so the shape is on record;
`a_failed_dlopen_stops_on_the_longjmp_thorn` does, and asserts the shape
rather than the bug, so it fails the day the thorn is designed away.

It is now the top blocker with real consumers rather than a parked
curiosity: every `dlopen` that fails is one, and real Python probes several
paths before it finds a module. `import hashlib` failed this way too, for
the same reason — its `libcrypto` was not in the tree.

Beside it, a discrepancy worth naming: the build plan says the container
pipeline always builds with `--resume`, and `baker::bake` does not pass it.
Nothing has needed it — and a resume ID in the slot is exactly what the
thorn's sketched third arm needs, so the two are one question.

### A test that was wrong about C rather than about the translation

The dlopen fixture printed `thread_local_(1)` and `thread_local_(2)` as two
arguments to one `printf` and expected `8 10`. It got `10 9`, because C
leaves argument evaluation order unspecified and gcc goes right to left.
The container and the native run had already agreed exactly — the
comparison against native passed and only the hand-written expectation
failed, which is the differential doing its job and the expectation doing
the opposite. The fixture sequences the two calls now.

### What is beyond it

`libcrypto.so.3` does not translate: *undecodable bytes in
`fn.0x100b66c0` at offset `0x16c4`*. Diagnosed rather than filed:
OpenSSL's hand-written asm puts constant tables inside `.text`, and this one
sits at `0xb7d8d`, inside a **guessed** extent belonging to a symbolless
function between `AES_cbc_encrypt` (which states its size and ends at
`0xb66b3`) and `AES_cfb128_encrypt`. Nothing names the table's start, so
nothing cuts the guess, and the decode walks into it.

The answer's shape is the one the stated/guessed distinction already gives:
undecodable bytes inside a *guessed* extent should truncate it rather than
refuse the bake, because a guess that runs into data is a guess that ran too
long, and code lost that way was undecodable anyway — anything that reaches
it gets a loud exec-map miss. Unbuilt. It blocks `hashlib`, `ssl` and
everything under them.

## 2026-08-30 — an extent is where the bytes end, not where the code does

`libcrypto.so.3` refused to translate: *undecodable bytes in
`fn.0x100b66c0` at offset `0x16c4`*. The fix was smaller than the diagnosis
and the diagnosis was wrong twice before it was right.

### What is actually at `0xb66c0`

`c6 63 63 a5, f8 7c 7c 84, ee 77 77 99, f6 7b 7b 8d` — AES's **Te0 S-box**.
It sits in `.text`, sixteen-byte aligned, immediately after
`AES_cbc_encrypt` (which states 1427 bytes and ends at `0xb66b3`), and
something takes its address because it is a lookup table. So the operand
harvest minted a function out of it, with a *guessed* extent bounded by
whatever begins next, and the decode fell over four bytes in.

Truncating a guessed extent at the poison is right for both shapes it meets:
a candidate that was never code becomes a dead three-byte stub nothing
transfers to, and a candidate that *was* code with a table beside it keeps
its code. Refusing the candidate outright would serve the first and lose
real code in the second, and a threshold between them would be a tuning
surface. Truncation needs neither.

### And then `RC4_options`, which says the asymmetry was wrong

With guessed extents truncating, the next refusal was a *stated* one, and
the first version of this fix kept those loud on the argument that a symbol
saying "1427 bytes of function" makes undecodable bytes inside it a decode
that lost sync.

`RC4_options` says otherwise. It is 208 bytes by its own `.size`, of which
the last 64 are the strings it returns: `repz ret` at `+0x27`, alignment
padding, then `"rc4(8x,int)"`. perlasm writes `.size name,.-name` *after*
the constant pool, so a stated extent spans data deliberately, and every
library built that way — which is every library with hand-written asm in it
— was refused.

So the rule is uniform. **An extent is where a function's bytes end; the
decode is where its code ends.** A stated extent's authority is over where a
function starts and that nothing else begins inside it; it was never a claim
that every byte is an instruction.

The dangerous case is still loud, one stage later and more precisely: the
last instruction kept is by construction one that *continues*, so its
fall-through lands where nothing begins and that is an `unreachable`, and
every other way in is an exec-map miss naming the address. What was there
before refused the whole *bake*, which is a worse answer than the project
gives an untranslatable function.

The truncation is read back into discovery by the front-end fixpoint, so
`Function` and `LiftedFunction` agree about size — the same channel D7's arm
feedback uses, and the second kind of evidence to flow backwards through it.
Two quantities now move and both move one way (cuts subdivide, extents only
shrink), which is what makes the loop terminate.

One interaction worth naming: the monotonicity guard would have fired on a
*correct* truncation, because "instructions" decoded out of an S-box include
indirect jumps and some of those recovered tables. A dispatch that no
function covers any more is not a dispatch that stopped being recoverable,
and the guard now says so.

### Where it got to

`libcrypto.so.3` lifts: 11,469 functions in 1.1 s. Then
`futex(FUTEX_WAKE_PRIVATE, INT_MAX)`, which OpenSSL calls on initialisation.
With one thread nothing can be parked on any address, so the number woken is
zero — exact rather than approximate, and a row it earns because refusing
stops single-threaded programs on a call whose answer is not in doubt.
`WAIT` stays a named fault: a wait that would block has nothing to wake it,
and the loud half is what makes sure the quiet half is revisited when M7
builds the queues rather than inherited.

    import hashlib -> sha256   identical to native

And with it `base64`, `uuid`, `random`, `datetime`, `struct`, `array`, on
top of the `json`, `math`, `cmath`, `re`, `collections`, `itertools` and
`functools` from the entry above.

Two left on the probe, both named rather than guessed: `zlib` reaches
`pshuflw` and `pinsrw`, which are two rows of the SSE gap list
`docs/design.md` already carries; `sqlite3` is the setjmp/longjmp thorn,
reached because its library is not in the tree.

### The fixture, and what building it cost

`tests/corpus/data_in_text.s` carries both shapes. Three attempts to make it
reproduce, each wrong in an instructive way:

1. Thirty-two real bytes of Te0 decode cleanly — where a table stops a
   decoder depends on which boundaries the fixpoint found in the surrounding
   garbage, so copying the bytes does not copy the failure.
2. Two hundred and fifty-six of them do too.
3. Starting the table with `nop` made it vanish: padding is never a
   function, and that filter refuses the candidate before this one is
   reached. Which is the filter working.

What reproduces it deterministically is an instruction that continues
followed by an opcode that does not exist in 64-bit mode — `0x06` — and the
fixture says why it is built that way rather than copied.

It is excluded from the relocatable breadth sweep, and the reason wants
stating precisely because I first stated it too grandly. In a `gcc -c`
object the assembler resolves `lea sbox(%rip)` itself — same section, known
distance — so no relocation survives, and a text-section address's only wasm
spelling is a function's table index. The refusal is correct.

But **that collision is the fixture's and not a real input's.** The shapes it
reproduces are real: `libcrypto.so.3` keeps Te0 in `.text` at `0xb66c0` and
takes its address from two `lea`s (`b5c82` and `b613d`), and `RC4_options`
states 208 bytes of which the last 64 are its strings. Both refused the
*bake*, in linked mode, with "undecodable bytes" — a different error
entirely. Nothing has ever produced the relocatable refusal except a corpus
source written for the linked tier and swept as a relocatable one, and
little could: compilers put constants in `.rodata`, so it takes hand-written
assembly, and hand-written assembly arrives here inside linked libraries.

The exclusion list now carries two categories instead of one, with that
distinction written into it rather than the design's out-of-scope clause,
which is real but was not what was happening.

## 2026-08-30 — a truncated body with no way out was never a function

The extent fix above left something it should not have, and reporting it as
complete was wrong. Truncating Te0's minted function to three bytes stopped
the bake failing, but kept a stub — and a stub is *in the exec map*, so an
indirect transfer to a data address would have called three bytes of
nonsense instead of missing by name. That is the silent answer this design
exists to prevent, reached from the other side.

The test is threshold-free: every real function contains something that ends
its execution — a `ret`, a jump out, a call that does not return — because
control has to get out. A truncated body with none of those is bytes
execution could enter and never leave. Te0 truncates to `xor %rax,%rax` and
nothing else.

Applied only to a witness that may not bound. A strong one said a function is
here; a body of its that decodes to nothing that returns is worth keeping and
looking at rather than deleting quietly. `RC4_options` is the check that the
line is in the right place: it truncates too, and its kept body still holds
its own `ret`, so it stays whole either way.

## 2026-08-30 — the word shuffles, and five mutations that survived a test with nothing in it

`zlib` reached `pshuflw` and `pinsrw`, so both families went in:
`pshuflw`/`pshufhw` — four words of one half permuted by an immediate, the
other copied through — and `pinsrw`/`pextrw`, the one member of the family
that reads across the two register files. All four are pair arithmetic
rather than SIMD, because a word permutation *within* an `i64` is exactly the
pair's own grain, which is the same argument the byte shifts and `pmuludq`
already make.

    import zlib     identical to native
    import gzip     identical
    import pickle, csv, decimal, fractions   identical

### The fixture had nothing in it, and five mutations said so

Written as `lane_case name, movq %rdi, %rdx; notq %rdx; pinsrw $0, %edx,
%xmm0` — and **GAS ends a macro invocation at the first `;`**, so the case
expanded to the prologue, the `movq`, and the fold. No `notq`, no `pinsrw`.
Four of the five `pinsrw` cases contained not one instance of the
instruction they were named for, and every mutation of the translation
passed because nothing reached it.

Found by mutating, not by reading: five in a row surviving is not a
translation that is right. `lane_broadcast_byte` in the same file is written
out longhand and now says why; the insert cases have a macro of their own.

### And then three more mutations, each teaching something different

- **`pinsrw` keeping its source's high bits** survived even after the cases
  were real, because the value inserted was `%rdi` — whose bits 16..31 are
  *already* the destination's next word, so the wrong answer and the right
  one are the same bits. The source is complemented now, which makes them
  differ by construction.
- **`pextrw` writing a word instead of a doubleword** survived, and that one
  was not a missing test: `write_operand` takes the width from the
  *register's own slice* for a register destination and never consults the
  argument. The mutation changed nothing. Which is the promotion plan's
  lesson — a mutation that survives may be testing the wrong claim — and it
  exposed a real gap beside it: SSE4.1's `pextrw` can write **m16**, where
  the width is the whole answer, and four bytes would have gone where two
  were named. Fixed, with a case whose destination slot is pre-filled so a
  wide store shows.
- **`pinsrw` reading four bytes from a memory source** survives and is
  documented rather than tested. The extra two bytes are masked off, so the
  value is identical; what differs is only which bytes were touched, and
  that is observable exactly once — a source in the last two bytes of linear
  memory, where the wider read traps — which no fixture can place.

Everything else was caught: both halves of both shuffles, the selector
stride, the insert's half and its destination mask, the extract's half and
its word.

## 2026-08-30 — work item zero: the baker builds with resume, and what it costs

The setjmp design's premise was that the container pipeline builds with
`--resume`. The plan had said so since M6 and `baker::bake` had never passed
it, which nothing noticed because nothing yet blocks on it — M7's scheduler
is what needs a thread's frames to be a chain of resume IDs, and M7 is not
built. `setjmp` is what brought the debt forward.

Measured on the CPython container, not assumed:

| | resume off | resume on | |
| --- | --- | --- | --- |
| functions | 123,053 | 123,053 | |
| refusals | 540 | 544 | +4, none reached |
| bake | 6.15 s | 8.41 s | 1.37× |
| container | 211 MB | 317 MB | 1.50× |
| boot, wall | 12.2 s | 20.5 s | 1.69× |
| boot, CPU | 209 s | 383 s | 1.83× |

Half the module and two thirds of the boot, for a second body per function.
Affordable at this scale, and now a number the rest of the design can be
priced against instead of a guess.

### Two holes in the loud-error policy, both the same shape

Turning it on did not work. Twice, and each failure was the trap policy
covering **one of a function's two bodies**:

- A *resume body* that could not be translated propagated its error instead
  of standing in. The ordinary body of the same function was refused
  politely and listed; its twin killed the bake.
- A *call-split graph* that could not be built did the same, one stage
  earlier — `ControlFlowGraph::build_resumable` is called before either body
  exists, and its `?` went straight out.

Both are under the policy now. The second one needed a decision rather than
a wrapper: a function whose graph cannot be split refuses **both** bodies,
because the alternative — an ordinary body translated without a site map, so
its call sites store sentinels — is a function a thread can block underneath
and whose frame chain then stops early at a sentinel. Silently. That is
worse than a body that names itself and stops.

Neither hole was reachable before today, and that is the point: a policy
that had only ever been exercised on half the code it governs.

### The premise, observed

`tests/dlopen.rs`'s thorn test asserted that a failed `dlopen` dies on
`RETURN_ADDRESS_SENTINEL`. It now dies on **`0x40000000e`** — table slot 14
in the low half, resume entry 4 in the high. The word `setjmp` saved is a
materialized continuation, saved by code that has no idea that is what it is
doing.

So the test changed what it records: that the value which misses is *not an
address* — a resume ID is a small integer or larger than any address a guest
can hold, never in between, which is the discrimination the design's miss
path turns on. It also now says, if the sentinel ever comes back, that the
container was built without resume rather than that `longjmp` changed.
