//! `eventfd`: a counter you can wait on.
//!
//! The smallest waitable object there is — no bytes, no addressing, just a
//! number that a write adds to and a read takes away — and it is here rather
//! than folded into [`crate::ring`] because its read is not a transfer: it
//! consumes *everything* and reports the total, or consumes one and reports
//! one, and neither is what taking bytes off the front of a buffer does.
//!
//! What it shares with everything else is the shape of the wait. Readiness
//! is the counter, which is arena state, so a scheduling decision can ask
//! it; the eight bytes go into the guest's memory, so the completion happens
//! on the parked process's own turn. The same split, for the same reason, as
//! the rings, the listeners and the poll sets.
//!
//! nginx reaches it through its notification path, and CPython's `asyncio`
//! through `socketpair` instead — which is why this arrives late and small.

use crate::errno::Errno;

/// `EFD_SEMAPHORE`: a read takes one rather than all.
pub const SEMAPHORE: i32 = 1;
/// `EFD_NONBLOCK` and `EFD_CLOEXEC`, which are `O_NONBLOCK` and `O_CLOEXEC`
/// under the names the call gives them.
pub const NONBLOCK: i32 = 0o4000;
pub const CLOEXEC: i32 = 0o2000000;

/// The largest a counter may reach. A write that would pass it waits, which
/// is the one place an `eventfd` blocks a writer.
pub const CEILING: u64 = u64::MAX - 1;

#[derive(Clone, Copy, Debug)]
struct Event {
    count: u64,
    semaphore: bool,
    references: u32,
}

/// Every `eventfd` in the container, shared for the same reason the rings
/// are: it is a notification between processes, and a copy of one notifies
/// nobody.
#[derive(Clone, Default, Debug)]
pub struct Events {
    entries: Vec<Option<Event>>,
}

impl Events {
    pub fn create(&mut self, count: u64, semaphore: bool) -> u32 {
        let event = Event {
            count,
            semaphore,
            references: 1,
        };
        if let Some(free) = self.entries.iter().position(Option::is_none) {
            self.entries[free] = Some(event);
            return free as u32;
        }
        self.entries.push(Some(event));
        (self.entries.len() - 1) as u32
    }

    pub fn acquire(&mut self, id: u32) {
        if let Some(Some(event)) = self.entries.get_mut(id as usize) {
            event.references += 1;
        }
    }

    pub fn release(&mut self, id: u32) {
        let Some(Some(event)) = self.entries.get_mut(id as usize) else {
            return;
        };
        event.references = event.references.saturating_sub(1);
        if event.references == 0 {
            self.entries[id as usize] = None;
        }
    }

    pub fn count(&self, id: u32) -> u64 {
        self.entries
            .get(id as usize)
            .and_then(|held| held.as_ref())
            .map_or(0, |event| event.count)
    }

    /// Whether a read would answer rather than wait.
    pub fn readable(&self, id: u32) -> bool {
        self.count(id) > 0
    }

    /// Whether a write of `value` would fit.
    pub fn writable(&self, id: u32, value: u64) -> bool {
        self.count(id).saturating_add(value) <= CEILING
    }

    /// Takes the counter down and answers what was taken — all of it, or one
    /// if this is a semaphore.
    pub fn take(&mut self, id: u32) -> Option<u64> {
        let Some(Some(event)) = self.entries.get_mut(id as usize) else {
            return None;
        };
        if event.count == 0 {
            return None;
        }
        match event.semaphore {
            true => {
                event.count -= 1;
                Some(1)
            }
            false => Some(core::mem::take(&mut event.count)),
        }
    }

    /// Adds to the counter, or says it would overflow.
    pub fn give(&mut self, id: u32, value: u64) -> Result<(), Errno> {
        let Some(Some(event)) = self.entries.get_mut(id as usize) else {
            return Err(Errno::BadFile);
        };
        if event.count.saturating_add(value) > CEILING {
            return Err(Errno::TryAgain);
        }
        event.count += value;
        Ok(())
    }
}

/// See [`crate::ring::Shared`].
pub type Shared = std::rc::Rc<std::cell::RefCell<Events>>;

impl<S: crate::abi::Store, M: crate::machine::Machine> crate::syscall::Kernel<'_, S, M> {
    /// `eventfd2(2)`.
    pub(crate) fn make_eventfd(&mut self, arguments: crate::syscall::Arguments) -> crate::syscall::Outcome {
        use crate::file::open_flags;
        use crate::syscall::Outcome;
        let count = arguments.get(0) as u64;
        let flags = arguments.get(1) as i32;
        if flags & !(SEMAPHORE | NONBLOCK | CLOEXEC) != 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let id = self
            .events
            .borrow_mut()
            .create(count, flags & SEMAPHORE != 0);
        let open = open_flags::READ_WRITE
            | match flags & NONBLOCK != 0 {
                true => open_flags::NONBLOCK,
                false => 0,
            };
        match self
            .files
            .open(crate::fd::Backing::Event(id), open, flags & CLOEXEC != 0)
        {
            Ok(fd) => Outcome::Done(i64::from(fd)),
            Err(errno) => {
                self.events.borrow_mut().release(id);
                Outcome::Done(errno.as_result())
            }
        }
    }

    /// The `eventfd` a descriptor names, with the description's flags.
    pub(crate) fn event_of(&self, fd: i32) -> Option<(u32, i32)> {
        let file = self.files.description(fd).ok()?;
        match file.backing {
            crate::fd::Backing::Event(id) => Some((id, file.flags)),
            _ => None,
        }
    }

    /// `read` and `write` on an `eventfd`, which move a number and not bytes.
    pub(crate) fn transfer_event(
        &mut self,
        id: u32,
        flags: i32,
        writing: bool,
        buffer: u64,
        count: u64,
    ) -> crate::syscall::Outcome {
        use crate::syscall::Outcome;
        // Eight bytes exactly. A shorter buffer is `EINVAL` and not a short
        // read, because the value is one number and half of one is nothing.
        if count < 8 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        if let Err(errno) = self.memory().check(buffer, 8) {
            return Outcome::Done(errno.as_result());
        }
        let value = match writing {
            true => {
                let mut bytes = [0u8; 8];
                if self.pages.read(buffer, &mut bytes).is_err() {
                    return Outcome::Done(Errno::Fault.as_result());
                }
                let value = u64::from_le_bytes(bytes);
                // The maximum is reserved as the "would block forever"
                // value, which Linux refuses rather than parks on.
                if value == u64::MAX {
                    return Outcome::Done(Errno::Invalid.as_result());
                }
                value
            }
            false => 0,
        };
        let waiting = crate::thread::Eventing {
            event: id,
            writing,
            buffer,
            value,
        };
        if let Some(answer) = self.complete_event(waiting) {
            return Outcome::Done(answer);
        }
        if flags & crate::file::open_flags::NONBLOCK != 0 {
            return Outcome::Done(Errno::TryAgain.as_result());
        }
        if !self.machine.park_on_event(waiting) {
            return Outcome::Done(Errno::TryAgain.as_result());
        }
        Outcome::Blocked
    }

    /// Moves the number if it can move, on the calling process's own turn.
    pub(crate) fn complete_event(&mut self, waiting: crate::thread::Eventing) -> Option<i64> {
        match waiting.writing {
            true => {
                self.events
                    .borrow_mut()
                    .give(waiting.event, waiting.value)
                    .ok()?;
                Some(8)
            }
            false => {
                let taken = self.events.borrow_mut().take(waiting.event)?;
                match self.pages.write(waiting.buffer, &taken.to_le_bytes()) {
                    Ok(()) => Some(8),
                    // The counter is already down and there is nowhere to put
                    // it, which is `EFAULT` and a lost notification — the
                    // same shape a pipe read has when its buffer goes away.
                    Err(_) => Some(Errno::Fault.as_result()),
                }
            }
        }
    }
}
