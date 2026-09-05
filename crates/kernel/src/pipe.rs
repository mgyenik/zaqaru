//! Pipes: one ring, and the two ends a descriptor can hold of it.
//!
//! Everything that *is* a ring — the buffer, the reference counts, the
//! end-of-file rule, the transfer that parks mid-move — lives in
//! [`crate::ring`], because a connected socket is two of them and the rules
//! had to be stated once. What is left here is the part that is a pipe: the
//! `pipe(2)` row that makes one ring and hands back a descriptor on each
//! end.
//!
//! Why the ring arena is shared across the process tree — the state POSIX
//! shares across a fork, kept where both processes can reach it — is in
//! [`crate::ring`]'s comment on [`crate::ring::Shared`].

use crate::errno::Errno;
use crate::ring::End;

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
        let id = self.rings.borrow_mut().create();
        let reader = match self.files.open(
            crate::fd::Backing::Pipe { ring: id, end: End::Read },
            open_flags::READ_ONLY | status,
            cloexec,
        ) {
            Ok(fd) => fd,
            Err(errno) => {
                self.rings.borrow_mut().release(id, End::Read);
                self.rings.borrow_mut().release(id, End::Write);
                return Outcome::Done(errno.as_result());
            }
        };
        let writer = match self.files.open(
            crate::fd::Backing::Pipe { ring: id, end: End::Write },
            open_flags::WRITE_ONLY | status,
            cloexec,
        ) {
            Ok(fd) => fd,
            Err(errno) => {
                let _ = self.files.close(reader);
                self.rings.borrow_mut().release(id, End::Read);
                self.rings.borrow_mut().release(id, End::Write);
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
                self.rings.borrow_mut().release(id, End::Read);
                self.rings.borrow_mut().release(id, End::Write);
                Outcome::Done(errno.as_result())
            }
        }
    }

    /// The pipe end a descriptor names, or `None`.
    pub(crate) fn pipe_of(&self, fd: i32) -> Option<(u32, End, i32)> {
        let file = self.files.description(fd).ok()?;
        match file.backing {
            crate::fd::Backing::Pipe { ring, end } => Some((ring, end, file.flags)),
            _ => None,
        }
    }

}
