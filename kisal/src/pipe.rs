//! Pipes: the first thing behind a descriptor that two processes genuinely
//! share.
//!
//! Everything a container could open until now was safe to *copy* into a
//! forked child. An image file is read-only, so two processes reading it
//! independently is the same as two processes reading it together. A console
//! has no position they can disagree about. A pipe is neither: it is a
//! buffer with one producer end and one consumer end, and the entire point
//! is that what one process writes another reads. Copy it at fork and the
//! child writes into a buffer nobody is reading.
//!
//! So this is the fd hoisting `container-plan.md` describes, and it is
//! *structural* rather than a pass over the table: the ring lives in an
//! arena the whole process tree shares, and a descriptor holds an index into
//! it. Forking copies the descriptor table — the numbers, the flags, the
//! close-on-exec bits, all per-process — and shares the arena, which is
//! exactly the POSIX split. The plan's rule that hoisting happens *before*
//! the snapshot is satisfied by there being nothing to snapshot: the bytes
//! were never in either process's address space.
//!
//! The counterpart is that closing has to be accounted for. A reader sees
//! end-of-file when the last *writer* descriptor closes, and a writer gets
//! `EPIPE` when the last *reader* does, so each end carries a count and a
//! fork raises both. Getting that wrong does not corrupt anything; it hangs,
//! which is worse to debug.

use std::collections::VecDeque;
use std::rc::Rc;

use crate::errno::Errno;

/// How much a pipe holds before a writer has to wait.
///
/// Linux's default, and the number matters: `pipe(7)` guarantees that a
/// write of up to `PIPE_BUF` bytes is atomic, and a shell or a build system
/// with several children writing to one pipe depends on it to keep their
/// lines from interleaving. A smaller buffer would keep that guarantee and
/// change how often a writer blocks, which is observable through timing; a
/// larger one would let a program buffer more than Linux does before it
/// discovers the reader is gone.
pub const CAPACITY: usize = 65536;

/// The largest write POSIX requires to be atomic.
pub const ATOMIC: u64 = 4096;

/// Which end of a pipe a descriptor is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum End {
    Read,
    Write,
}

/// One pipe's buffer, and how many descriptors are on each end.
#[derive(Clone, Debug)]
struct Ring {
    /// A deque rather than a fixed array with two cursors, because the
    /// operations are exactly a deque's and writing the cursors by hand is
    /// where ring buffers go wrong.
    bytes: VecDeque<u8>,
    readers: u32,
    writers: u32,
}

/// Every pipe in the container.
///
/// Shared by every process in it — see the module comment — through an `Rc`
/// on the kernel, so a fork clones a pointer and an `execve` keeps it. Which
/// is the whole of hoisting, stated once.
#[derive(Clone, Default, Debug)]
pub struct Pipes {
    /// An arena, so an identifier is an index and stays valid while anything
    /// holds it. A freed slot is reused, which is safe because a slot is
    /// only freed when both counts reach zero and therefore nothing names
    /// it any more.
    rings: Vec<Option<Ring>>,
}

impl Pipes {
    /// A new pipe with one descriptor on each end, which is what `pipe(2)`
    /// hands back.
    pub fn create(&mut self) -> u32 {
        let ring = Ring {
            bytes: VecDeque::new(),
            readers: 1,
            writers: 1,
        };
        if let Some(free) = self.rings.iter().position(Option::is_none) {
            self.rings[free] = Some(ring);
            return free as u32;
        }
        self.rings.push(Some(ring));
        (self.rings.len() - 1) as u32
    }

    fn ring(&self, id: u32) -> Option<&Ring> {
        self.rings.get(id as usize)?.as_ref()
    }

    fn ring_mut(&mut self, id: u32) -> Option<&mut Ring> {
        self.rings.get_mut(id as usize)?.as_mut()
    }

    /// One more descriptor on an end — a `dup`, or a fork copying the table.
    pub fn acquire(&mut self, id: u32, end: End) {
        if let Some(ring) = self.ring_mut(id) {
            match end {
                End::Read => ring.readers += 1,
                End::Write => ring.writers += 1,
            }
        }
    }

    /// One fewer, and the pipe itself when the last of both goes.
    pub fn release(&mut self, id: u32, end: End) {
        let Some(ring) = self.ring_mut(id) else {
            return;
        };
        match end {
            End::Read => ring.readers = ring.readers.saturating_sub(1),
            End::Write => ring.writers = ring.writers.saturating_sub(1),
        }
        if ring.readers == 0 && ring.writers == 0 {
            self.rings[id as usize] = None;
        }
    }

    pub fn readers(&self, id: u32) -> u32 {
        self.ring(id).map_or(0, |ring| ring.readers)
    }

    pub fn writers(&self, id: u32) -> u32 {
        self.ring(id).map_or(0, |ring| ring.writers)
    }

    /// How many bytes are waiting to be read.
    pub fn queued(&self, id: u32) -> usize {
        self.ring(id).map_or(0, |ring| ring.bytes.len())
    }

    /// How much more can be written before a writer has to wait.
    pub fn room(&self, id: u32) -> usize {
        CAPACITY - self.queued(id)
    }

    /// Whether a `read` would return rather than park.
    ///
    /// True with no data when there is no writer left, because that read
    /// returns zero — end of file is a completed read, not a reason to wait.
    pub fn readable(&self, id: u32) -> bool {
        self.queued(id) > 0 || self.writers(id) == 0
    }

    /// Whether a `write` of `length` would make progress rather than park.
    ///
    /// A write of up to [`ATOMIC`] bytes waits for room for *all* of it,
    /// because POSIX says a write that size arrives in one piece and a
    /// partial one would let two writers interleave inside a line. A larger
    /// write only needs somewhere to put its first byte.
    ///
    /// True with no room when there is no reader left, because that write
    /// fails with `EPIPE` — an error is also not a reason to wait.
    pub fn writable(&self, id: u32, length: u64) -> bool {
        if self.readers(id) == 0 {
            return true;
        }
        let room = self.room(id) as u64;
        match length <= ATOMIC {
            true => room >= length,
            false => room > 0,
        }
    }

    /// Moves bytes out, up to what `into` holds. Answers how many.
    pub fn take(&mut self, id: u32, into: &mut [u8]) -> usize {
        let Some(ring) = self.ring_mut(id) else {
            return 0;
        };
        let moved = into.len().min(ring.bytes.len());
        for slot in into.iter_mut().take(moved) {
            *slot = ring.bytes.pop_front().expect("the length was just read");
        }
        moved
    }

    /// Moves bytes in, up to the room there is. Answers how many.
    pub fn give(&mut self, id: u32, from: &[u8]) -> usize {
        let room = self.room(id);
        let Some(ring) = self.ring_mut(id) else {
            return 0;
        };
        let moved = from.len().min(room);
        ring.bytes.extend(&from[..moved]);
        moved
    }
}

/// The table, as the kernel holds it.
///
/// `Rc` and not a plain field: this is the one piece of kernel state that a
/// fork must *share* rather than copy, and putting the sharing in the type
/// means `Kernel::fork` cannot get it wrong by writing one more `.clone()`
/// that means the wrong thing.
pub type Shared = Rc<std::cell::RefCell<Pipes>>;

/// What a transfer that could not finish is waiting for.
///
/// Recorded on the thread rather than re-derived by re-running the syscall,
/// and the reason is `write`: a write of more than a pipe holds moves in
/// pieces, and POSIX says the caller sees one count at the end. A restarted
/// syscall would either move the first piece twice or report the last piece
/// as the whole.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Transfer {
    pub pipe: u32,
    pub end: End,
    /// The guest's buffer, and how much of it the call named.
    pub buffer: u64,
    pub length: u64,
    /// How much has already moved. Always zero for a read, which parks only
    /// when it has nothing at all — Linux returns from a `read` as soon as
    /// one byte is there.
    pub done: u64,
}

impl Transfer {
    /// Whether this transfer could move now, or is finished and only needs
    /// collecting.
    pub fn ready(&self, pipes: &Pipes) -> bool {
        match self.end {
            End::Read => pipes.readable(self.pipe),
            End::Write => pipes.writable(self.pipe, self.length - self.done),
        }
    }
}

/// The error a write to a pipe with no reader gets.
///
/// Linux also raises `SIGPIPE`, whose default action ends the process — so a
/// program that has not ignored it never sees the errno at all, and one that
/// has sees exactly this.
pub const BROKEN: Errno = Errno::Pipe;

impl<S: crate::abi::Store, M: crate::machine::Machine> crate::syscall::Kernel<'_, S, M> {
    /// `pipe(2)` and `pipe2(2)`.
    ///
    /// One buffer, two descriptors, and the pair written back to the
    /// caller's array — which is done *last*, after both descriptors exist,
    /// so a guest whose array address is bad does not end up with two
    /// descriptors it was never told the numbers of.
    pub(crate) fn make_pipe(
        &mut self,
        number: i64,
        arguments: crate::syscall::Arguments,
    ) -> crate::syscall::Outcome {
        use crate::file::open_flags;
        use crate::syscall::Outcome;
        let at = arguments.get(0) as u64;
        // `pipe` has no flags argument at all; `pipe2` takes the two that
        // apply to something with no name.
        let flags = match number == crate::syscall::number::PIPE2 {
            true => arguments.get(1) as i32,
            false => 0,
        };
        const SUPPORTED: i32 = open_flags::NONBLOCK | open_flags::CLOEXEC | open_flags::DIRECT;
        if flags & !SUPPORTED != 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        if flags & open_flags::DIRECT != 0 {
            return Outcome::Fault(crate::syscall::Fault::detailed(
                number,
                arguments,
                "`O_DIRECT` on a pipe, which makes it carry packets rather \
                 than a byte stream — a different data structure, not a flag \
                 on this one",
            ));
        }
        if let Err(errno) = self.memory().check(at, 8) {
            return Outcome::Done(errno.as_result());
        }
        let cloexec = flags & open_flags::CLOEXEC != 0;
        let status = flags & open_flags::NONBLOCK;
        let id = self.pipes.borrow_mut().create();
        let reader = match self.files.open(
            crate::fd::Backing::Pipe { pipe: id, end: End::Read },
            open_flags::READ_ONLY | status,
            cloexec,
        ) {
            Ok(fd) => fd,
            Err(errno) => {
                self.pipes.borrow_mut().release(id, End::Read);
                self.pipes.borrow_mut().release(id, End::Write);
                return Outcome::Done(errno.as_result());
            }
        };
        let writer = match self.files.open(
            crate::fd::Backing::Pipe { pipe: id, end: End::Write },
            open_flags::WRITE_ONLY | status,
            cloexec,
        ) {
            Ok(fd) => fd,
            Err(errno) => {
                let _ = self.files.close(reader);
                self.pipes.borrow_mut().release(id, End::Read);
                self.pipes.borrow_mut().release(id, End::Write);
                return Outcome::Done(errno.as_result());
            }
        };
        let mut bytes = [0u8; 8];
        bytes[0..4].copy_from_slice(&reader.to_le_bytes());
        bytes[4..8].copy_from_slice(&writer.to_le_bytes());
        // SAFETY: bounds-checked above, and nothing has run since.
        match unsafe { self.memory_mut().write(at, &bytes) } {
            Ok(()) => Outcome::Done(0),
            Err(errno) => {
                let _ = self.files.close(reader);
                let _ = self.files.close(writer);
                self.pipes.borrow_mut().release(id, End::Read);
                self.pipes.borrow_mut().release(id, End::Write);
                Outcome::Done(errno.as_result())
            }
        }
    }

    /// The pipe end a descriptor names, or `None`.
    pub(crate) fn pipe_of(&self, fd: i32) -> Option<(u32, End, i32)> {
        let file = self.files.description(fd).ok()?;
        match file.backing {
            crate::fd::Backing::Pipe { pipe, end } => Some((pipe, end, file.flags)),
            _ => None,
        }
    }

    /// `read` and `write` on a pipe, which is one function because the two
    /// differ in three lines and agree in everything that is hard.
    ///
    /// What is hard is the four ways a transfer ends: it moved something, so
    /// it answers a count; it cannot move anything and the other end is
    /// gone, so it answers zero or `EPIPE`; it cannot move anything and the
    /// descriptor is non-blocking, so it answers `EAGAIN`; or it cannot move
    /// anything and has to *wait*, which is the only one that is not a
    /// return value.
    pub(crate) fn transfer_pipe(
        &mut self,
        pipe: u32,
        end: End,
        flags: i32,
        buffer: u64,
        length: u64,
    ) -> crate::syscall::Outcome {
        use crate::syscall::Outcome;
        if let Err(errno) = self.memory().check(buffer, length) {
            return Outcome::Done(errno.as_result());
        }
        // A zero-length transfer is a question about nothing, and Linux
        // answers it without looking at the pipe — including when the other
        // end is gone.
        if length == 0 {
            return Outcome::Done(0);
        }
        let transfer = Transfer {
            pipe,
            end,
            buffer,
            length,
            done: 0,
        };
        match self.advance_transfer(transfer) {
            Progress::Done(answer) => Outcome::Done(answer),
            Progress::Waiting(pending) => {
                if flags & crate::file::open_flags::NONBLOCK != 0 {
                    // Something already moved is a completed short transfer,
                    // not a reason to report that nothing could.
                    return Outcome::Done(match pending.done {
                        0 => Errno::TryAgain.as_result(),
                        done => done as i64,
                    });
                }
                self.machine.park_on_transfer(pending);
                Outcome::Blocked
            }
        }
    }

    /// Moves whatever can move right now, and says whether that finished it.
    ///
    /// The one place bytes cross between a pipe and an address space, so it
    /// is also the one place that has to hold the rule the whole design
    /// rests on: the buffer is *this* process's, and this process is the one
    /// running, which is why a parked transfer is completed on the parked
    /// process's own turn and never on the turn of whoever woke it.
    pub(crate) fn advance_transfer(&mut self, mut transfer: Transfer) -> Progress {
        loop {
            let left = transfer.length - transfer.done;
            let at = transfer.buffer + transfer.done;
            match transfer.end {
                End::Read => {
                    let available = self.pipes.borrow().queued(transfer.pipe) as u64;
                    if available == 0 {
                        // No writer will ever put anything there again, so
                        // this is end of file — which is a completed read of
                        // zero bytes, not a wait.
                        if self.pipes.borrow().writers(transfer.pipe) == 0 {
                            return Progress::Done(transfer.done as i64);
                        }
                        // Linux returns from a `read` as soon as one byte is
                        // there rather than filling the buffer, so anything
                        // already moved is the answer.
                        return match transfer.done {
                            0 => Progress::Waiting(transfer),
                            done => Progress::Done(done as i64),
                        };
                    }
                    let want = left.min(available) as usize;
                    let mut bytes = vec![0u8; want];
                    let moved = self.pipes.borrow_mut().take(transfer.pipe, &mut bytes);
                    // SAFETY: the whole buffer was bounds-checked when the
                    // transfer was created, and linear memory never shrinks.
                    if unsafe { self.memory_mut().write(at, &bytes[..moved]) }.is_err() {
                        // The guest unmapped its own buffer while parked.
                        // The bytes are already out of the pipe and there is
                        // nowhere to put them, which is exactly `EFAULT`.
                        return Progress::Done(Errno::Fault.as_result());
                    }
                    transfer.done += moved as u64;
                    // One read, one answer.
                    return Progress::Done(transfer.done as i64);
                }
                End::Write => {
                    if self.pipes.borrow().readers(transfer.pipe) == 0 {
                        // Nobody will ever read it. Linux also raises
                        // `SIGPIPE`, whose default action ends the process —
                        // so a program that has not ignored it never sees
                        // this errno, and one that has sees exactly it.
                        const SIGPIPE: i32 = 13;
                        if self.signal_process(SIGPIPE) {
                            return Progress::Done(BROKEN.as_result());
                        }
                        return match transfer.done {
                            0 => Progress::Done(BROKEN.as_result()),
                            done => Progress::Done(done as i64),
                        };
                    }
                    let room = self.pipes.borrow().room(transfer.pipe) as u64;
                    // A write of up to `ATOMIC` bytes arrives in one piece
                    // or not at all, which is what keeps two writers' lines
                    // from interleaving.
                    if room == 0 || (left <= ATOMIC && room < left) {
                        return Progress::Waiting(transfer);
                    }
                    let want = left.min(room) as usize;
                    // SAFETY: bounds-checked when the transfer was created.
                    let bytes = match unsafe { self.memory().slice(at, want as u64) } {
                        Ok(bytes) => bytes.to_vec(),
                        Err(_) => return Progress::Done(Errno::Fault.as_result()),
                    };
                    let moved = self.pipes.borrow_mut().give(transfer.pipe, &bytes) as u64;
                    transfer.done += moved;
                    if transfer.done >= transfer.length {
                        return Progress::Done(transfer.done as i64);
                    }
                    // More to write than the pipe held. Round again: either
                    // a reader has drained some in the meantime, or this
                    // parks.
                    if moved == 0 {
                        return Progress::Waiting(transfer);
                    }
                }
            }
        }
    }
}

/// What one turn of a transfer produced.
pub enum Progress {
    /// The value to put in `%rax`.
    Done(i64),
    /// Nothing more can move yet; here is what is left.
    Waiting(Transfer),
}
