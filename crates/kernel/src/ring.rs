//! A ring: bytes with a producer end and a consumer end.
//!
//! Split out of `pipe.rs` because a pipe is not the only thing made of one.
//! A pipe *is* a ring — one direction, a reader end and a writer end. A
//! connected socket is **two**, crossed: what one endpoint writes, the other
//! reads, in both directions at once. So the buffer, the reference counts,
//! the end-of-file rule and the transfer that parks mid-move all belong
//! here, and `pipe.rs` and `socket.rs` are the two things that arrange
//! rings into an object a descriptor can name.
//!
//! Which is also what keeps the run loop from growing a second case:
//! [`Transfer`] names a *ring*, so `resume_transfers` in `kernel/src/run.rs`
//! moves bytes for a socket and for a pipe with the same code, and the rule
//! it obeys — a parked transfer completes on the parked process's own turn,
//! never on the waker's — is stated once.
//!
//! The half-close matrix falls out of the counts rather than being a matrix.
//! `shutdown(SHUT_WR)` on a socket endpoint drops its *writer* reference on
//! the ring it transmits into, so the peer drains and then reads zero —
//! which is exactly what the last writer of a pipe closing already does.
//! `shutdown(SHUT_RD)` drops its reader reference on the ring it receives
//! from, so the peer's next write is `EPIPE`. One rule, applied per
//! direction.

use std::collections::VecDeque;
use std::rc::Rc;

use crate::errno::Errno;

/// How much a ring holds before a writer has to wait.
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
    /// How many socket endpoints name this ring, whether or not they still
    /// hold an end of it open.
    ///
    /// The counts above say who can still read and write; this says who
    /// can still *ask about* it, and the two part company at a half-close.
    /// An endpoint that has `shutdown(SHUT_WR)` has given up its writer, and
    /// when the peer then closes its reader the ring has no ends left — but
    /// the endpoint still names it, to answer whether the peer is reading.
    /// Freeing the slot at that moment let the next `connect` reuse it, and
    /// the half-closed endpoint then saw the new connection's reader as its
    /// own peer come back to life: no `POLLHUP`, a `poll` that slept out its
    /// whole timeout, and a request that took two seconds under concurrent
    /// load and nothing sequentially.
    ///
    /// Pipes never attach, and are freed as they always were.
    attached: u32,
}

/// Every pipe in the container.
///
/// Shared by every process in it — see the module comment — through an `Rc`
/// on the kernel, so a fork clones a pointer and an `execve` keeps it. Which
/// is the whole of hoisting, stated once.
#[derive(Clone, Default, Debug)]
pub struct Rings {
    /// An arena, so an identifier is an index and stays valid while anything
    /// holds it. A freed slot is reused, which is safe only because a slot
    /// is freed when both counts reach zero *and* no endpoint names it —
    /// see [`Ring::attached`] for the day the second condition was missing.
    rings: Vec<Option<Ring>>,
}

impl Rings {
    /// A new pipe with one descriptor on each end, which is what `pipe(2)`
    /// hands back.
    pub fn create(&mut self) -> u32 {
        let ring = Ring {
            bytes: VecDeque::new(),
            readers: 1,
            writers: 1,
            attached: 0,
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
        self.free_if_unused(id);
    }

    /// One more endpoint names the ring. See [`Ring::attached`].
    pub fn attach(&mut self, id: u32) {
        if let Some(ring) = self.ring_mut(id) {
            ring.attached += 1;
        }
    }

    /// One fewer endpoint names the ring, which may be what frees it.
    pub fn detach(&mut self, id: u32) {
        if let Some(ring) = self.ring_mut(id) {
            ring.attached = ring.attached.saturating_sub(1);
        }
        self.free_if_unused(id);
    }

    /// A ring goes when nothing can read it, nothing can write it, and
    /// nothing names it — all three, because any one of them alone is a
    /// slot something still expects to find its own ring in.
    fn free_if_unused(&mut self, id: u32) {
        if let Some(ring) = self.ring(id)
            && ring.readers == 0
            && ring.writers == 0
            && ring.attached == 0
        {
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

    /// Copies bytes out *without* removing them, which is `MSG_PEEK`.
    ///
    /// A separate operation rather than a flag on `take`, because the
    /// difference is the whole of what the flag means and a boolean threaded
    /// through the taking loop is how one of the two ends up consuming when
    /// it should not.
    pub fn peek(&self, id: u32, into: &mut [u8]) -> usize {
        let Some(ring) = self.ring(id) else {
            return 0;
        };
        let moved = into.len().min(ring.bytes.len());
        for (slot, byte) in into.iter_mut().zip(ring.bytes.iter()).take(moved) {
            *slot = *byte;
        }
        moved
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
pub type Shared = Rc<std::cell::RefCell<Rings>>;

/// What a transfer that could not finish is waiting for.
///
/// Recorded on the thread rather than re-derived by re-running the syscall,
/// and the reason is `write`: a write of more than a pipe holds moves in
/// pieces, and POSIX says the caller sees one count at the end. A restarted
/// syscall would either move the first piece twice or report the last piece
/// as the whole.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Transfer {
    pub ring: u32,
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
    pub fn ready(&self, rings: &Rings) -> bool {
        match self.end {
            End::Read => rings.readable(self.ring),
            End::Write => rings.writable(self.ring, self.length - self.done),
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
    /// `read` and `write` against a ring, which is one function because the
    /// two differ in three lines and agree in everything that is hard — and
    /// which serves a pipe and a socket for the same reason.
    ///
    /// What is hard is the four ways a transfer ends: it moved something, so
    /// it answers a count; it cannot move anything and the other end is
    /// gone, so it answers zero or `EPIPE`; it cannot move anything and the
    /// descriptor is non-blocking, so it answers `EAGAIN`; or it cannot move
    /// anything and has to *wait*, which is the only one that is not a
    /// return value.
    pub(crate) fn transfer_ring(
        &mut self,
        ring: u32,
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
            ring,
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
                    let available = self.rings.borrow().queued(transfer.ring) as u64;
                    if available == 0 {
                        // No writer will ever put anything there again, so
                        // this is end of file — which is a completed read of
                        // zero bytes, not a wait.
                        if self.rings.borrow().writers(transfer.ring) == 0 {
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
                    let moved = self.rings.borrow_mut().take(transfer.ring, &mut bytes);
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
                    if self.rings.borrow().readers(transfer.ring) == 0 {
                        // Nobody will ever read it. Linux also raises
                        // `SIGPIPE`, whose default action ends the process —
                        // so a program that has not ignored it never sees
                        // this errno, and one that has sees exactly it.
                        const SIGPIPE: i32 = 13;
                        // `MSG_NOSIGNAL` suppresses the signal and not the
                        // errno, which is what every server that writes to a
                        // socket it might outlive depends on.
                        if !self.machine.owned().no_sigpipe && self.signal_process(SIGPIPE) {
                            return Progress::Done(BROKEN.as_result());
                        }
                        return match transfer.done {
                            0 => Progress::Done(BROKEN.as_result()),
                            done => Progress::Done(done as i64),
                        };
                    }
                    let room = self.rings.borrow().room(transfer.ring) as u64;
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
                    let moved = self.rings.borrow_mut().give(transfer.ring, &bytes) as u64;
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
