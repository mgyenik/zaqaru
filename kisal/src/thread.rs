//! Threads: what a process has more than one of.
//!
//! The design's claim about threads is that they are cheap here, and this
//! module is where that gets tested. Under the ahead-of-time seam a new
//! thread means a fabricated resume chain on a fresh stack, a first genuine
//! unwind, and a dispatcher that has to be entered at the right body — the
//! subtlest machinery in the tree. Under the loop a thread is a
//! [`targum::state::Tcb`] with `rip` and `rsp` set, and a context switch is
//! choosing a different index.
//!
//! What is *not* cheap and is not skipped: the kernel half. Which thread is
//! runnable, what a futex wait parks on and what a wake releases, what
//! `clear_child_tid` does when a thread ends, and how a thread that exits
//! differs from a process that does — all of it is ordinary Rust and all of
//! it is testable without emulating anything.

use targum::state::Tcb;

/// The first thread's identifier, and the process's.
///
/// One process, so one process id; the first thread's id is the same number,
/// which is what Linux does and what `getpid() == gettid()` in the main
/// thread means.
pub const FIRST: i32 = 1;

/// Why a thread is not running.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    /// It could run now.
    Runnable,
    /// It is parked on a futex word, waiting for a wake that names it.
    ///
    /// The bitset is `FUTEX_WAIT_BITSET`'s; a plain `FUTEX_WAIT` parks with
    /// every bit set, so one code path serves both and the plain case is
    /// the all-ones case rather than a second one.
    Waiting { word: u64, bitset: u32 },
    /// It is in `wait4`, and a child ending completes the call: the answer
    /// is written where the caller asked and the thread becomes runnable
    /// again, rather than the syscall being re-run.
    WaitingForChild { wanted: i32, status_at: u64 },
    /// It is part-way through a transfer on a pipe that could not finish.
    ///
    /// A record rather than a syscall to re-run, and `write` is why: a write
    /// of more than a pipe holds moves in pieces, and POSIX says the caller
    /// sees one count at the end. Re-running would move the first piece
    /// twice.
    Transferring(crate::pipe::Transfer),
    /// It is in `poll` or `epoll_wait`, waiting for a descriptor to become
    /// ready or for a deadline to pass.
    Watching(Watching),
    /// It called `pause`, and is waiting for a signal and nothing else.
    ///
    /// The one wait with no object: every other parked thread is waiting
    /// for a futex word, a child, a pipe or a descriptor, and this one is
    /// waiting for the *interruption* those others merely tolerate.
    Paused,
    /// It called `exit`. Its control block is kept until something reaps
    /// the identifier, because a thread that has ended is still a thread
    /// that existed.
    Exited { status: i32 },
}

/// A parked `poll` or `epoll_wait`.
///
/// The set is addresses rather than a copy, which is what keeps a thread's
/// state a plain value: the `pollfd` array lives in the caller's own memory
/// and is still there while the caller is parked, so re-reading it costs
/// nothing and copying it would only be a second version of the truth.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Watching {
    pub watch: crate::poll::Watch,
    /// Nanoseconds on the monotonic clock, or `None` for a wait with no
    /// timeout — which is the case that costs nothing, because the thread
    /// is simply not runnable until the answer changes.
    pub deadline: Option<u64>,
}

/// The kernel's own per-thread cells.
///
/// Separate from [`Thread`] because both worlds have them and only one has a
/// [`Tcb`]: the ahead-of-time machine keeps its registers in wasm globals
/// and has exactly one thread, and it still has a signal mask and a
/// `clear_child_tid`. So this is what [`crate::machine::Machine`] hands out,
/// and the two worlds differ only in where it is stored.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Owned {
    /// Where to write a zero when this thread ends, from
    /// `set_tid_address(2)` or `clone`'s `CLONE_CHILD_CLEARTID`. The kernel
    /// clears that word and wakes anything waiting on it, which is how
    /// `pthread_join` learns a thread is gone.
    pub clear_child_tid: u64,
    /// The head of this thread's robust futex list.
    pub robust_list: u64,
    /// Signals this thread has blocked, as a bitmask indexed from zero.
    pub blocked_signals: u64,
    /// Signals raised at this thread and not yet delivered.
    pub pending_signals: u64,
    /// The stack a handler runs on when its disposition asks for one.
    ///
    /// The whole reason `sigaltstack` exists is the case where the ordinary
    /// stack cannot be used — a stack overflow, whose `SIGSEGV` would fault
    /// again the moment a frame was pushed. That case only became reachable
    /// here when the address space gained page permissions.
    pub altstack: crate::signal::Altstack,
    /// Whether a handler is currently running on it, which `sigaltstack`
    /// reports and which stops a nested delivery from restarting the stack
    /// under the handler already using it.
    pub on_altstack: bool,
    /// What the frame the current handler is running on says the mask was.
    pub interrupted_mask: u64,
}

/// One thread.
pub struct Thread {
    pub tcb: Tcb,
    pub tid: i32,
    pub state: State,
    pub owned: Owned,
}

impl Thread {
    pub fn new(tid: i32, tcb: Tcb) -> Self {
        Self {
            tcb,
            tid,
            state: State::Runnable,
            owned: Owned::default(),
        }
    }

    pub fn is_runnable(&self) -> bool {
        self.state == State::Runnable
    }

    /// Whether a signal is pending and not blocked — the question the run
    /// loop asks at every block boundary.
    pub fn deliverable(&self) -> Option<i32> {
        let ready = self.owned.pending_signals & !self.owned.blocked_signals;
        match ready {
            0 => None,
            _ => Some(ready.trailing_zeros() as i32 + 1),
        }
    }
}

/// What `clone` asks for, reduced to what a thread actually needs.
///
/// The flags a caller passes are mostly statements about what is *shared*,
/// and on this machine everything is: one address space, one filesystem
/// view, one descriptor table. What is left is a stack, somewhere to put
/// thread-local storage, and a word to clear on the way out.
#[derive(Clone, Copy, Default, Debug)]
pub struct Spawn {
    pub stack: u64,
    pub tls: Option<u64>,
    pub clear_child_tid: u64,
}

/// Every thread a process has, and which one is running.
///
/// A `Vec` rather than a fixed array because the count is the guest's to
/// choose, and an index rather than a pointer because a context switch that
/// is an integer assignment cannot dangle.
pub struct Threads {
    threads: Vec<Thread>,
    current: usize,
    /// The next identifier to hand out. Never reused within a run, so that a
    /// stale `tgkill` names a thread that is gone rather than a thread that
    /// has taken its number.
    next: i32,
}

impl Default for Threads {
    fn default() -> Self {
        Self::new()
    }
}

impl Threads {
    pub fn new() -> Self {
        Self {
            threads: vec![Thread::new(FIRST, Tcb::new())],
            current: 0,
            next: FIRST + 1,
        }
    }

    pub fn current(&self) -> &Thread {
        &self.threads[self.current]
    }

    pub fn current_mut(&mut self) -> &mut Thread {
        &mut self.threads[self.current]
    }

    pub fn all(&self) -> &[Thread] {
        &self.threads
    }

    pub fn all_mut(&mut self) -> &mut [Thread] {
        &mut self.threads
    }

    pub fn find_mut(&mut self, tid: i32) -> Option<&mut Thread> {
        self.threads.iter_mut().find(|thread| thread.tid == tid)
    }

    /// Just the running thread, as a fork's child gets.
    ///
    /// POSIX: only the calling thread survives into the child. The others
    /// are not stopped or unwound — they are not copied, which is the
    /// difference between a table and a stack.
    pub fn only_current(&self) -> Self {
        let current = self.current();
        let mut thread = Thread::new(FIRST, current.tcb.clone());
        thread.owned = current.owned;
        Self {
            threads: vec![thread],
            current: 0,
            next: FIRST + 1,
        }
    }

    /// Adds a thread, and answers its identifier.
    pub fn spawn(&mut self, tcb: Tcb) -> i32 {
        let tid = self.next;
        self.next += 1;
        self.threads.push(Thread::new(tid, tcb));
        tid
    }

    /// How many threads have not exited.
    pub fn live(&self) -> usize {
        self.threads
            .iter()
            .filter(|thread| !matches!(thread.state, State::Exited { .. }))
            .count()
    }

    /// Picks the next runnable thread, round-robin from the one after the
    /// current.
    ///
    /// Round-robin and not a priority: the quantum is denominated in retired
    /// instructions, so the order is a pure function of execution and two
    /// runs of the same container interleave identically. A scheduler that
    /// consulted a clock would make replay a property of luck.
    ///
    /// Answers false when nothing is runnable, which the caller reads as
    /// either "everything exited" or "everything is waiting" — two different
    /// endings that it can tell apart from the states.
    pub fn schedule(&mut self) -> bool {
        let count = self.threads.len();
        for step in 1..=count {
            let candidate = (self.current + step) % count;
            if self.threads[candidate].is_runnable() {
                self.current = candidate;
                return true;
            }
        }
        false
    }

    /// Wakes up to `count` threads parked on `word` whose bitset overlaps.
    ///
    /// In thread order, which makes which waiter wins a fact about the
    /// program rather than about a hash table's iteration.
    pub fn wake(&mut self, word: u64, bitset: u32, count: usize) -> usize {
        let mut woken = 0;
        for thread in &mut self.threads {
            if woken >= count {
                break;
            }
            if let State::Waiting {
                word: parked,
                bitset: mask,
            } = thread.state
                && parked == word
                && mask & bitset != 0
            {
                thread.state = State::Runnable;
                // The wait's answer, written now because *now* is when it is
                // known: the parked thread's `syscall` already advanced its
                // program counter, so when the scheduler picks it up again
                // it resumes at the instruction after the call and reads
                // whatever is in `%rax`. Leaving it alone leaves the syscall
                // number there, and glibc reads a `futex` that answered 202
                // as "the futex facility returned an unexpected error code"
                // and aborts — which is exactly what it did.
                thread.tcb.registers[0] = 0;
                woken += 1;
            }
        }
        woken
    }
}
