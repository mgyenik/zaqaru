//! Asking which descriptors are ready, and waiting until one is.
//!
//! Three calls with one question behind them — `poll`, `ppoll` and the
//! `epoll` family — so there is one answer here and three shapes of it. What
//! differs between them is where the set of descriptors lives: `poll` is
//! handed the whole set on every call, and `epoll` keeps it in the kernel
//! across calls, which is the entire reason it exists and is also why an
//! `epoll` instance is a descriptor of its own.
//!
//! **Readiness is a property of what is behind the descriptor**, so it is
//! computed rather than stored. A pipe is ready to read when something is
//! queued *or* when no writer is left, because a read that returns zero is a
//! read that returned. A regular file is always ready in both directions,
//! which is POSIX and not a simplification: `poll` is about whether an
//! operation would block, and a file read never does.
//!
//! # Waiting, and the one thing it cannot do
//!
//! A wait with no timeout parks until some descriptor is ready, and that
//! costs nothing: the thread is not runnable, and the process is scheduled
//! again exactly when the answer changes. A wait *with* a timeout is the
//! awkward case. There is no way to sleep from inside the module — the host
//! boundary is two `ll-store` imports and neither of them waits — so a
//! deadline is checked by looking at the clock, which means a container
//! whose only pending work is a timeout spins until it fires. Correct, and
//! hot; naming it here rather than leaving it to be discovered.

use crate::errno::Errno;
use crate::fd::{Backing, Console};
use crate::syscall::{Arguments, Outcome};

/// The `poll` event bits, which `epoll` shares the low half of.
pub mod event {
    pub const IN: i16 = 0x001;
    pub const PRI: i16 = 0x002;
    pub const OUT: i16 = 0x004;
    pub const ERR: i16 = 0x008;
    pub const HUP: i16 = 0x010;
    pub const NVAL: i16 = 0x020;
    pub const RDNORM: i16 = 0x040;
    pub const WRNORM: i16 = 0x100;
    pub const RDHUP: i16 = 0x2000;
}

/// `epoll_ctl`'s operations.
pub mod control {
    pub const ADD: i64 = 1;
    pub const DEL: i64 = 2;
    pub const MOD: i64 = 3;
}

/// `EPOLLET`, and the other flags that change *when* a report happens rather
/// than what is being asked about.
pub const EDGE_TRIGGERED: u32 = 1 << 31;
pub const ONESHOT: u32 = 1 << 30;

/// `struct epoll_event`: a bitmask and the caller's own word, packed — which
/// on x86-64 means twelve bytes and not sixteen, because the attribute is
/// there precisely to stop the compiler aligning the second field.
pub const EPOLL_EVENT: u64 = 12;

/// `struct pollfd`.
pub const POLLFD: u64 = 8;

/// What Linux refuses to poll more than.
const MAX_DESCRIPTORS: i64 = 1024 * 1024;

/// One registration in an `epoll` set.
///
/// **Two keys, and the difference between them is the whole of `epoll`'s
/// most famous behaviour.** `epoll_ctl` names a *descriptor*, so `MOD` and
/// `DEL` match on `fd`. Readiness follows the *description*, because that is
/// what Linux registers interest against — which is why a registered
/// descriptor closed while a `dup` of it survives goes on firing. Real
/// software depends on that, and so do its test suites, so it is built
/// rather than tidied away.
#[derive(Clone, Copy, Debug)]
struct Watched {
    /// What `epoll_ctl` named. Only ever compared, never dereferenced.
    fd: i32,
    /// What readiness is asked of, and what has to *go* for this
    /// registration to end.
    description: usize,
    events: u32,
    /// The caller's own word, handed back untouched. Usually a pointer to
    /// whatever the program wanted to find again.
    data: u64,
}

/// One `epoll` instance: the set a descriptor stands for.
#[derive(Clone, Default, Debug)]
pub struct EpollSet {
    watched: Vec<Watched>,
    /// How many descriptors name this instance.
    references: u32,
    /// Set on the child's copy at `fork`. See [`Epolls`].
    inherited: bool,
}

/// Every `epoll` instance in one process.
///
/// **Per process, unlike [`crate::ring::Rings`]**, and the reason is
/// structural rather than a preference: a registration names an open file
/// *description*, and a description is an index into a table each process
/// has its own copy of. An arena shared across the tree keyed by those
/// indices would let one process's `close` cancel another's registration —
/// which is not a subtle wrongness, it is a `poll` that never wakes.
///
/// Linux does share an `epoll` instance across a `fork`, at the description
/// level, and it is famous as a footgun. `container-plan.md` decided that
/// case explicitly: "using an inherited epoll fd from the child after plain
/// fork is a loud, documented error, revisited if something real trips it".
/// So a fork copies the sets and marks them, and the child is told rather
/// than quietly given a private one.
#[derive(Clone, Default, Debug)]
pub struct Epolls {
    sets: Vec<Option<EpollSet>>,
}

impl Epolls {
    pub fn create(&mut self) -> u32 {
        self.hold(Vec::new())
    }

    /// A set with its registrations already in it, for a `poll` — which is
    /// an `epoll` set that lives for exactly one call.
    ///
    /// It exists so that **readiness can be answered without touching the
    /// caller's memory**. The `pollfd` array is a guest address, and a guest
    /// address means the caller's bytes only while the caller is the process
    /// at the guest's addresses — but the question "could this process run
    /// now" is asked while deciding *which* process to run, when some other
    /// one's memory is mapped. Reading the array there does not fault: the
    /// forked child has a stack at the same address, so it answers with
    /// somebody else's bytes and the wait never wakes.
    fn hold(&mut self, watched: Vec<Watched>) -> u32 {
        let set = EpollSet {
            watched,
            references: 1,
            inherited: false,
        };
        if let Some(free) = self.sets.iter().position(Option::is_none) {
            self.sets[free] = Some(set);
            return free as u32;
        }
        self.sets.push(Some(set));
        (self.sets.len() - 1) as u32
    }

    pub fn acquire(&mut self, id: u32) {
        if let Some(Some(set)) = self.sets.get_mut(id as usize) {
            set.references += 1;
        }
    }

    pub fn release(&mut self, id: u32) {
        let Some(Some(set)) = self.sets.get_mut(id as usize) else {
            return;
        };
        set.references = set.references.saturating_sub(1);
        if set.references == 0 {
            self.sets[id as usize] = None;
        }
    }

    fn set_mut(&mut self, id: u32) -> Option<&mut EpollSet> {
        self.sets.get_mut(id as usize)?.as_mut()
    }

    fn watched(&self, id: u32) -> &[Watched] {
        match self.sets.get(id as usize).and_then(Option::as_ref) {
            Some(set) => &set.watched,
            None => &[],
        }
    }

    /// Drops every registration on a description that no longer exists.
    ///
    /// Linux's `eventpoll_release`, and it is not tidiness: a description's
    /// slot is *reused*, so a registration left behind would start watching
    /// whichever file opened next and report it under the old caller's
    /// data word. Called when the last descriptor naming a description
    /// closes, which is exactly when Linux frees the file.
    /// Marks every set as the child's copy of its parent's. See [`Epolls`].
    pub fn inherit(&mut self) {
        for set in self.sets.iter_mut().flatten() {
            set.inherited = true;
        }
    }

    pub fn is_inherited(&self, id: u32) -> bool {
        self.sets
            .get(id as usize)
            .and_then(Option::as_ref)
            .is_some_and(|set| set.inherited)
    }

    pub fn forget(&mut self, description: usize) {
        for set in self.sets.iter_mut().flatten() {
            set.watched.retain(|entry| entry.description != description);
        }
    }
}

/// A borrow-checker convenience, not a sharing one: the kernel reaches its
/// own `epoll` sets while holding a borrow of its descriptor table, which a
/// plain field would forbid. Contrast [`crate::ring::Shared`], which is an
/// `Rc` because the pipes genuinely *are* shared.
pub type Held = std::cell::RefCell<Epolls>;

impl<S: crate::abi::Store, M: crate::machine::Machine> crate::syscall::Kernel<'_, S, M> {
    /// What a descriptor would answer right now, given what is asked about.
    ///
    /// `POLLERR`, `POLLHUP` and `POLLNVAL` are reported whether or not they
    /// were asked for, which is `poll(2)`'s rule and not an accident: a
    /// program polling for `POLLIN` on a pipe whose writer has gone needs to
    /// hear about it, and it did not think to ask.
    pub(crate) fn readiness(&self, fd: i32, interest: i16) -> i16 {
        match self.files.description_index(fd) {
            Ok(description) => self.readiness_of(description, interest),
            // Not a descriptor this process has, which `poll` reports as
            // `POLLNVAL`. An `epoll` registration cannot arrive here at
            // all, because it names a description rather than a number.
            Err(_) => event::NVAL,
        }
    }

    /// The same, of an open file description rather than of a descriptor.
    ///
    /// This is the one `epoll` uses, and the split is the whole of why a
    /// closed descriptor with a surviving `dup` goes on firing: nothing
    /// here ever looks up a number.
    pub(crate) fn readiness_of(&self, description: usize, interest: i16) -> i16 {
        let Some(file) = self.files.at(description) else {
            return event::NVAL;
        };
        let ready = match file.backing {
            Backing::Pipe { ring, end } => {
                let rings = self.rings.borrow();
                match end {
                    crate::ring::End::Read => {
                        let mut bits = 0;
                        if rings.queued(ring) > 0 {
                            bits |= event::IN | event::RDNORM;
                        }
                        // The writer is gone and nothing more will arrive.
                        // Reported alongside `POLLIN` when bytes are still
                        // queued, because both are true.
                        if rings.writers(ring) == 0 {
                            bits |= event::HUP;
                        }
                        bits
                    }
                    crate::ring::End::Write => {
                        let mut bits = 0;
                        if rings.room(ring) > 0 {
                            bits |= event::OUT | event::WRNORM;
                        }
                        // Nobody will ever read it, which is the writer's
                        // half of the same fact.
                        if rings.readers(ring) == 0 {
                            bits |= event::ERR;
                        }
                        bits
                    }
                }
            }
            // Standard input's bytes come from the host, which answers
            // immediately with whatever it has — including nothing, which is
            // end of file and is itself a completed read.
            Backing::Console(Console::Input) => event::IN | event::RDNORM,
            Backing::Console(_) => event::OUT | event::WRNORM,
            // A regular file never blocks in either direction. This is
            // `poll(2)`'s answer and not a shortcut: the question is whether
            // an operation would wait, and a file read does not.
            Backing::Image(_) => event::IN | event::OUT | event::RDNORM | event::WRNORM,
            // An `epoll` descriptor is ready to read when its own set has
            // something, which is what makes nesting one inside another
            // work — and what a program does when it wants one wait to cover
            // two sets it keeps separately.
            Backing::Epoll(id) => match self.epoll_ready(id).is_empty() {
                true => 0,
                false => event::IN | event::RDNORM,
            },
        };
        let always = event::ERR | event::HUP | event::NVAL;
        ready & (interest | always)
    }

    /// Refuses an `epoll` descriptor a `fork` handed down, by name.
    ///
    /// Linux shares the interest list across a fork at the description
    /// level. Here a description is a per-process index, so sharing the
    /// list would mean one process's `close` cancelling another's
    /// registration. `container-plan.md` chose the loud error over the
    /// complexity knot, and this is it: a divergence stated on the box
    /// rather than a `poll` that mysteriously never wakes.
    fn refuse_inherited(&mut self, number: i64, arguments: Arguments) -> Option<Outcome> {
        let id = self.epoll_of(arguments.get(0) as i32)?;
        if !self.epolls.borrow().is_inherited(id) {
            return None;
        }
        Some(Outcome::Fault(crate::syscall::Fault::detailed(
            number,
            arguments,
            "an `epoll` descriptor inherited across a `fork`, whose interest \
             list Linux shares between the two processes — a shape \
             `container-plan.md` refuses by name rather than approximate, \
             because the alternative is a registration one process can \
             cancel out from under the other",
        )))
    }

    /// The `epoll` instance a descriptor names, or `None`.
    pub(crate) fn epoll_of(&self, fd: i32) -> Option<u32> {
        match self.files.description(fd).ok()?.backing {
            Backing::Epoll(id) => Some(id),
            _ => None,
        }
    }

    /// Every registration in a set that has something to report.
    fn epoll_ready(&self, id: u32) -> Vec<(u64, u32)> {
        let watched: Vec<Watched> = self.epolls.borrow().watched(id).to_vec();
        watched
            .into_iter()
            .filter_map(|entry| {
                // The low bits of an `epoll` mask are the `poll` bits, which
                // is why one readiness function serves both.
                let asked = (entry.events & 0xffff) as i16;
                let ready = self.readiness_of(entry.description, asked);
                match ready {
                    0 => None,
                    bits => Some((entry.data, bits as u16 as u32)),
                }
            })
            .collect()
    }

    /// `poll(2)` and `ppoll(2)`.
    pub(crate) fn poll(&mut self, number: i64, arguments: Arguments) -> Outcome {
        let at = arguments.get(0) as u64;
        let count = arguments.get(1);
        if !(0..=MAX_DESCRIPTORS).contains(&count) {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        if let Err(errno) = self.memory().check(at, count as u64 * POLLFD) {
            return Outcome::Done(errno.as_result());
        }
        // `poll`'s timeout is milliseconds in a register; `ppoll`'s is a
        // `timespec` the caller points at, and a null pointer means forever.
        let timeout = match number == crate::syscall::number::PPOLL {
            false => match arguments.get(2) {
                milliseconds if milliseconds < 0 => Wait::Forever,
                0 => Wait::Immediately,
                milliseconds => Wait::Until(milliseconds as u64 * 1_000_000),
            },
            true => match self.timespec_at(arguments.get(2)) {
                Ok(Some(nanoseconds)) if nanoseconds == 0 => Wait::Immediately,
                Ok(Some(nanoseconds)) => Wait::Until(nanoseconds),
                Ok(None) => Wait::Forever,
                Err(errno) => return Outcome::Done(errno.as_result()),
            },
        };
        self.begin_wait(Watch::Poll { set: 0, at, count: count as u64 }, timeout)
    }

    /// `epoll_create` and `epoll_create1`.
    pub(crate) fn epoll_create(&mut self, number: i64, arguments: Arguments) -> Outcome {
        use crate::file::open_flags;
        // The size argument of the original call is advisory and has been
        // ignored by Linux since 2.6.8, but it still has to be positive —
        // which is the one thing a program can observe about it.
        if number == crate::syscall::number::EPOLL_CREATE && arguments.get(0) <= 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let flags = match number == crate::syscall::number::EPOLL_CREATE1 {
            true => arguments.get(0) as i32,
            false => 0,
        };
        if flags & !open_flags::CLOEXEC != 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let id = self.epolls.borrow_mut().create();
        match self.files.open(
            Backing::Epoll(id),
            open_flags::READ_WRITE,
            flags & open_flags::CLOEXEC != 0,
        ) {
            Ok(fd) => Outcome::Done(fd as i64),
            Err(errno) => {
                self.epolls.borrow_mut().release(id);
                Outcome::Done(errno.as_result())
            }
        }
    }

    /// `epoll_ctl(2)`: add, change or remove one registration.
    pub(crate) fn epoll_control(&mut self, arguments: Arguments) -> Outcome {
        if let Some(refusal) = self.refuse_inherited(crate::syscall::number::EPOLL_CTL, arguments) {
            return refusal;
        }
        let Some(id) = self.epoll_of(arguments.get(0) as i32) else {
            // Either the descriptor is closed or it is not an epoll
            // instance, and Linux distinguishes them.
            return Outcome::Done(match self.files.is_open(arguments.get(0) as i32) {
                true => Errno::Invalid.as_result(),
                false => Errno::BadFile.as_result(),
            });
        };
        let operation = arguments.get(1);
        let fd = arguments.get(2) as i32;
        let Ok(description) = self.files.description_index(fd) else {
            return Outcome::Done(Errno::BadFile.as_result());
        };
        // An epoll instance watching itself is a cycle with no bottom.
        if self.epoll_of(fd) == Some(id) {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let (events, data) = match operation {
            control::DEL => (0, 0),
            _ => match self.epoll_event_at(arguments.get(3)) {
                Ok(pair) => pair,
                Err(errno) => return Outcome::Done(errno.as_result()),
            },
        };
        let mut epolls = self.epolls.borrow_mut();
        let Some(set) = epolls.set_mut(id) else {
            return Outcome::Done(Errno::BadFile.as_result());
        };
        let existing = set.watched.iter().position(|entry| entry.fd == fd);
        match operation {
            control::ADD => match existing {
                // Already registered, which is the error `epoll_ctl` has
                // instead of silently replacing.
                Some(_) => Outcome::Done(Errno::Exists.as_result()),
                None => {
                    set.watched.push(Watched {
                        fd,
                        description,
                        events,
                        data,
                    });
                    Outcome::Done(0)
                }
            },
            control::MOD => match existing {
                Some(index) => {
                    set.watched[index] = Watched {
                        fd,
                        description,
                        events,
                        data,
                    };
                    Outcome::Done(0)
                }
                None => Outcome::Done(Errno::NoEntry.as_result()),
            },
            control::DEL => match existing {
                Some(index) => {
                    set.watched.remove(index);
                    Outcome::Done(0)
                }
                None => Outcome::Done(Errno::NoEntry.as_result()),
            },
            _ => Outcome::Done(Errno::Invalid.as_result()),
        }
    }

    /// `epoll_wait` and `epoll_pwait`.
    pub(crate) fn epoll_wait(&mut self, arguments: Arguments) -> Outcome {
        if let Some(refusal) = self.refuse_inherited(crate::syscall::number::EPOLL_WAIT, arguments) {
            return refusal;
        }
        let Some(id) = self.epoll_of(arguments.get(0) as i32) else {
            return Outcome::Done(match self.files.is_open(arguments.get(0) as i32) {
                true => Errno::Invalid.as_result(),
                false => Errno::BadFile.as_result(),
            });
        };
        let at = arguments.get(1) as u64;
        let max = arguments.get(2);
        if max <= 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        if let Err(errno) = self.memory().check(at, max as u64 * EPOLL_EVENT) {
            return Outcome::Done(errno.as_result());
        }
        let timeout = match arguments.get(3) {
            milliseconds if milliseconds < 0 => Wait::Forever,
            0 => Wait::Immediately,
            milliseconds => Wait::Until(milliseconds as u64 * 1_000_000),
        };
        self.begin_wait(
            Watch::Epoll {
                epoll: id,
                at,
                max: max as u64,
            },
            timeout,
        )
    }

    /// The half `poll` and `epoll_wait` share: answer now, or park.
    fn begin_wait(&mut self, watch: Watch, timeout: Wait) -> Outcome {
        let reported = self.report_ready(watch);
        if reported > 0 || timeout == Wait::Immediately {
            return Outcome::Done(reported);
        }
        // About to park, so the set has to move somewhere the scheduler can
        // read it — see `Epolls::hold`. An `epoll_wait` already has one.
        let watch = match watch {
            Watch::Poll { at, count, .. } => match self.hold_poll(at, count) {
                Ok(set) => Watch::Poll { set, at, count },
                Err(errno) => return Outcome::Done(errno.as_result()),
            },
            other => other,
        };
        let deadline = match timeout {
            Wait::Until(nanoseconds) => match self.monotonic() {
                Some(now) => Some(now + nanoseconds),
                // No clock mounted, so a deadline cannot be told from a
                // wait. Waiting forever is the safe direction: it blocks
                // where Linux would block, rather than returning early and
                // making a program spin.
                None => None,
            },
            _ => None,
        };
        if !self.machine.park_on_watch(crate::thread::Watching { watch, deadline }) {
            // A machine with no scheduler cannot wait, and answering zero
            // would look like a timeout that did not happen.
            return Outcome::Done(Errno::NoSys.as_result());
        }
        Outcome::Blocked
    }

    /// Writes out whatever is ready, and answers how many — which is what
    /// both calls return.
    pub(crate) fn report_ready(&mut self, watch: Watch) -> i64 {
        match watch {
            Watch::Poll { at, count, .. } => {
                let mut ready = 0;
                for index in 0..count {
                    let entry = at + index * POLLFD;
                    let mut bytes = [0u8; POLLFD as usize];
                    if self.pages.read(entry, &mut bytes).is_err() {
                        return Errno::Fault.as_result();
                    }
                    let fd = i32::from_le_bytes(bytes[0..4].try_into().expect("four bytes"));
                    let interest = i16::from_le_bytes(bytes[4..6].try_into().expect("two bytes"));
                    // A negative descriptor is how a caller keeps a slot in
                    // the array without polling it, and it is answered with
                    // zero rather than `POLLNVAL`.
                    let revents = match fd < 0 {
                        true => 0,
                        false => self.readiness(fd, interest),
                    };
                    bytes[6..8].copy_from_slice(&revents.to_le_bytes());
                    if self.pages.write(entry, &bytes).is_err() {
                        return Errno::Fault.as_result();
                    }
                    ready += i64::from(revents != 0);
                }
                ready
            }
            Watch::Epoll { epoll, at, max } => {
                let ready = self.epoll_ready(epoll);
                let mut written = 0;
                for (data, events) in ready.into_iter().take(max as usize) {
                    let mut bytes = [0u8; EPOLL_EVENT as usize];
                    bytes[0..4].copy_from_slice(&events.to_le_bytes());
                    bytes[4..12].copy_from_slice(&data.to_le_bytes());
                    if self.pages.write(at + written * EPOLL_EVENT, &bytes).is_err() {
                        return Errno::Fault.as_result();
                    }
                    written += 1;
                }
                written as i64
            }
        }
    }

    /// Whether a parked wait could answer now.
    ///
    /// Deliberately *not* the same code as reporting: this one may not write
    /// to the guest's memory, because it is asked while deciding which
    /// process to run and the answer decides whether that process's memory
    /// is even at the guest's addresses.
    pub(crate) fn watch_ready(&self, watch: Watch) -> bool {
        match watch {
            // Both from the kernel's own copy of the set, and never from
            // the caller's array — see [`Watch`].
            Watch::Poll { set, .. } => !self.epoll_ready(set).is_empty(),
            Watch::Epoll { epoll, .. } => !self.epoll_ready(epoll).is_empty(),
        }
    }

    /// Copies a `pollfd` array into a set the scheduler can read.
    ///
    /// Done here, in the caller's own turn, which is the only moment the
    /// array is reachable.
    fn hold_poll(&mut self, at: u64, count: u64) -> Result<u32, Errno> {
        let mut watched = Vec::new();
        for index in 0..count {
            let mut bytes = [0u8; POLLFD as usize];
            self.pages
                .read(at + index * POLLFD, &mut bytes)
                .map_err(|_| Errno::Fault)?;
            let fd = i32::from_le_bytes(bytes[0..4].try_into().expect("four bytes"));
            // A negative descriptor holds a slot in the array without being
            // polled, so it is not something to wake for either.
            if fd < 0 {
                continue;
            }
            let interest = i16::from_le_bytes(bytes[4..6].try_into().expect("two bytes"));
            let Ok(description) = self.files.description_index(fd) else {
                // A descriptor that is not open answers `POLLNVAL` at once
                // rather than being waited for — which `begin_wait` has
                // already reported, so reaching here means the caller asked
                // to wait on nothing.
                continue;
            };
            watched.push(Watched {
                fd,
                description,
                events: interest as u16 as u32,
                data: 0,
            });
        }
        Ok(self.epolls.borrow_mut().hold(watched))
    }

    /// Lets go of the set a parked `poll` was holding.
    pub(crate) fn release_watch(&mut self, watch: Watch) {
        if let Watch::Poll { set, .. } = watch {
            self.epolls.borrow_mut().release(set);
        }
    }

    /// The monotonic clock in nanoseconds, or `None` when none is mounted.
    fn monotonic(&mut self) -> Option<u64> {
        let mut bytes = Vec::new();
        if self.store.read(crate::paths::TIME_MONOTONIC, &mut bytes)
            != crate::abi::StoreOutcome::Present
        {
            return None;
        }
        // The same decoding `clock_gettime` uses, through the same helper,
        // so a deadline and a `CLOCK_MONOTONIC` the guest reads itself
        // cannot disagree about what time it is.
        let nanoseconds = crate::syscall::parse_nanoseconds(&bytes)?;
        u64::try_from(nanoseconds).ok()
    }

    /// Whether a deadline has passed.
    pub(crate) fn expired(&mut self, deadline: Option<u64>) -> bool {
        match (deadline, self.monotonic()) {
            (Some(deadline), Some(now)) => now >= deadline,
            _ => false,
        }
    }

    /// Reads a `timespec`, answering `None` for a null pointer — which is
    /// what `ppoll` means by "no timeout".
    fn timespec_at(&self, at: i64) -> Result<Option<u64>, Errno> {
        if at == 0 {
            return Ok(None);
        }
        let mut bytes = [0u8; 16];
        self.pages
            .read(at as u64, &mut bytes)
            .map_err(|_| Errno::Fault)?;
        let seconds = u64::from_le_bytes(bytes[0..8].try_into().expect("eight bytes"));
        let nanoseconds = u64::from_le_bytes(bytes[8..16].try_into().expect("eight bytes"));
        if nanoseconds >= 1_000_000_000 {
            return Err(Errno::Invalid);
        }
        Ok(Some(seconds.saturating_mul(1_000_000_000).saturating_add(nanoseconds)))
    }

    /// Reads a `struct epoll_event`.
    fn epoll_event_at(&self, at: i64) -> Result<(u32, u64), Errno> {
        let mut bytes = [0u8; EPOLL_EVENT as usize];
        self.pages
            .read(at as u64, &mut bytes)
            .map_err(|_| Errno::Fault)?;
        Ok((
            u32::from_le_bytes(bytes[0..4].try_into().expect("four bytes")),
            u64::from_le_bytes(bytes[4..12].try_into().expect("eight bytes")),
        ))
    }
}

/// What a parked wait is watching.
///
/// Both carry a *set identifier*, which is kernel state, and an address,
/// which is guest state — and the split is the whole point. The identifier
/// is what readiness is computed from, because that question is asked while
/// choosing which process to run and must not reach into any process's
/// memory. The address is where the answer is written, which happens on the
/// waiting process's own turn and nowhere else.
///
/// So a `poll` is an `epoll` set that lives for one call. It is not a trick:
/// the two calls differ in where the set is kept and for how long, and this
/// makes that the only difference.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Watch {
    Poll { set: u32, at: u64, count: u64 },
    Epoll { epoll: u32, at: u64, max: u64 },
}

/// How long a wait is willing to wait.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Wait {
    Immediately,
    Forever,
    /// Nanoseconds from now.
    Until(u64),
}
