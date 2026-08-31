//! The run loop: a scheduler with an interpreter under it.
//!
//! This is the whole of what replaces the ahead-of-time seam. There, a
//! `syscall` is a rewritten call through a generated thunk that marshals six
//! argument registers out of wasm globals, hands the kernel a stack that
//! must dodge the guest's red zone, receives either a result or a leave
//! sentinel, and turns the sentinel into a wasm throw that a catch above
//! reinterprets. Here the loop reaches a `syscall`, reads six fields, calls
//! a Rust function, and writes one field back.
//!
//! The shape a junior developer should recognise from any virtual machine —
//! run a thread until something stops it, decide what the something was,
//! go round again — and deliberately so, because the shape *is* the
//! scheduler. What more threads will add is a choice of which control block
//! to advance; nothing about the loop changes.
//!
//! Two things the interpreter's world gets for free and the seam's cannot:
//! **`%rcx` and `%r11` are faithful** at a syscall, holding the return
//! address and the flags word as hardware leaves them rather than the
//! conformant zeros the transpiler invents; and a fault is a *fault*, with
//! the program counter left on the faulting instruction so that a handler
//! could retry it.

use targum::block::BlockCache;
use targum::exec::Trap;
use targum::state::Tcb;
use targum::{Engine, Outcome as Step};

use crate::abi::Store;
use crate::machine::{Interpreted, Machine};
use crate::syscall::{Arguments, Fault, Kernel, Outcome};

/// Where a syscall's arguments come from, in the order Linux puts them.
///
/// `%rcx` is absent on purpose and `%r10` stands in its place: the `syscall`
/// instruction destroys `%rcx` by putting the return address there, so the
/// ABI moved the fourth argument. Getting this wrong is a fourth argument
/// that is *almost* right — it holds a code address, which is a plausible
/// pointer — and it is the kind of mistake a trace reads past.
const ARGUMENTS: [usize; 6] = [7, 6, 2, 10, 8, 9];

/// The register a syscall's number arrives in and its result leaves in.
const NUMBER: usize = 0;

/// What one turn of the loop did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    /// Nothing to decide; go round again with the same process.
    Running,
    /// The quantum expired. The thread is still runnable, and this is the
    /// moment a *process* scheduler takes its turn — the same instant the
    /// thread scheduler does, and for the same reason.
    Preempted,
    /// No thread in this process can run.
    ///
    /// Not a verdict, which is the point: every thread here is parked on
    /// something, and whether that something will ever happen is a question
    /// about the *container* — a process reading a pipe is waiting for a
    /// process that has not been given a turn yet. Only
    /// [`crate::system::System`], which can see every process, can call it a
    /// deadlock.
    Idle,
    Finished(Exit),
    /// The process needs something only the system can do.
    Requested(crate::syscall::Request),
}

/// What serving one syscall did — the same three answers, one level down.
enum Served {
    Returned,
    Finished(Exit),
    Requested(crate::syscall::Request),
}

/// How a process finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    /// It called `exit_group`, with this status.
    Status(i32),
    /// It took a signal nothing handled.
    ///
    /// `address` is what the access named and `rip` is the instruction that
    /// made it — both, because a wild pointer and a wild jump look the same
    /// with only one of them.
    Signalled {
        signal: i32,
        address: u64,
        rip: u64,
        /// What the access was trying to do, when the signal came from one.
        ///
        /// A wild jump and a wild pointer both arrive as `SIGSEGV` with two
        /// addresses, and without this they are the same report. They are
        /// not the same bug.
        access: Option<targum::space::Access>,
    },
    /// The kernel does not implement something the guest asked for.
    Unimplemented(Fault),
    /// The engine does not implement an instruction the guest reached.
    Unsupported(targum::exec::Unsupported),
    /// Every thread is parked on a futex nothing will wake.
    Deadlocked,
    /// Not an ending at all: the trap became a signal and a handler is
    /// running. Never returned from [`crate::system::System::run`]; it exists
    /// so that
    /// [`Process::fault`] can answer one type.
    Delivered,
}

/// One process: a kernel, the blocks decoded out of its address space, and
/// its threads.
///
/// The address space itself is the kernel's — the mapping rows write it, so
/// the kernel has to own it — and everything here is what the *scheduler*
/// owns. One thread for now; the field is a thread rather than a queue
/// because a queue with one entry teaches nothing and hides what the loop
/// actually does.
pub struct Process<'a, S: Store> {
    pub kernel: Kernel<'a, S, Interpreted>,
    pub cache: BlockCache,
}

impl<'a, S: Store> Process<'a, S> {
    /// The thread being advanced.
    ///
    /// It lives inside the machine, which lives inside the kernel — one
    /// owner, and the borrow checker to prove it.
    pub fn thread(&mut self) -> &mut Tcb {
        self.kernel.machine.thread_mut()
    }

    /// Loads a program and leaves the thread ready to enter it.
    ///
    /// Everything here is kisal's existing `execve`: the segments are placed
    /// at the addresses the ELF states, the auxiliary vector is built the
    /// way `_start` is compiled to read it, the floating-point unit is
    /// reset. What is new is only where the answers go — into a control
    /// block instead of into wasm globals.
    pub fn boot(
        mut kernel: Kernel<'a, S, Interpreted>,
        path: &[u8],
        argv: &[&[u8]],
        envp: &[&[u8]],
    ) -> Result<Self, crate::exec::Error> {
        // An image's entrypoint is usually a bare name — `python3`, not
        // `/usr/local/bin/python3` — because a container runtime resolves it
        // the way a shell does. `execve` does not, and must not: that is
        // `execvpe`'s job and the distinction is the guest's to see. So the
        // search happens here, at the one moment there is no guest yet.
        let resolved = Self::resolve(&mut kernel, path, envp)?;
        let path = resolved.as_slice();

        // The directory the image says to start in, before anything runs —
        // because a relative path in the command line is relative to it.
        // `WORKDIR /app` beside `CMD ["python", "app.py"]` is the shape
        // every application image has.
        let directory = kernel.image_working_directory.clone();
        if !directory.is_empty() {
            let root = kernel.vfs.root();
            let vnode = kernel
                .vfs
                .resolve(root, &directory, crate::vfs::Lookup::FOLLOW)
                .map_err(|_| {
                    crate::exec::Error::NotLoadable(
                        "the image asks to start in a directory the image does not have",
                    )
                })?;
            kernel.vfs.set_working_directory(vnode).map_err(|_| {
                crate::exec::Error::NotLoadable(
                    "the image asks to start somewhere that is not a directory",
                )
            })?;
        }

        // The environment is the caller's, unchanged.
        //
        // Worth saying because the ahead-of-time boot prepends
        // `LD_BIND_NOW=1` here and this deliberately does not. That world
        // has a reason: lazy binding calls `_dl_runtime_resolve`, which
        // saves the vector register file with `fxsave`, and a bake cannot
        // translate that instruction — so binding eagerly is how the
        // function is never reached. None of that is true here. `fxsave` is
        // an instruction like any other, it writes state the control block
        // already holds, and the interpreter executes it. Injecting a
        // variable the guest can read, to avoid an instruction the engine
        // can execute, would be a divergence bought for nothing.
        Self::enter(kernel, path, argv, envp)
    }

    /// Loads a program into a kernel that already has an address space
    /// reserved, and leaves the thread ready to enter it.
    ///
    /// This is `execve(2)` exactly: the path is used as written. No `PATH`
    /// search, because that is `execvp`'s job and doing it here would make
    /// `execve("python3", ...)` succeed where Linux answers `ENOENT` — a
    /// difference the guest can see, from a library that is trying to
    /// implement the search itself.
    pub fn enter(
        mut kernel: Kernel<'a, S, Interpreted>,
        path: &[u8],
        argv: &[&[u8]],
        envp: &[&[u8]],
    ) -> Result<Self, crate::exec::Error> {
        let entry = kernel.exec(path, argv, envp)?;
        // `exec` already wrote the stack pointer and reset the unit through
        // the machine, which in this world *is* the control block — so the
        // only thing left is where to start.
        kernel.machine.thread_mut().rip = entry;
        Ok(Self {
            kernel,
            // Nothing decoded here is worth anything to the new program: the
            // bytes at every address just changed.
            cache: BlockCache::new(),
        })
    }

    /// Finds the program a container was told to start.
    ///
    /// A name with a slash in it is a path and is used as one. A bare name
    /// is searched for along `PATH`, first match wins, exactly as `execvp`
    /// does — and the failure names every directory it looked in, because
    /// "no such file" about a name that was never a path sends the reader
    /// looking for the wrong thing.
    fn resolve(
        kernel: &mut Kernel<'a, S, Interpreted>,
        path: &[u8],
        envp: &[&[u8]],
    ) -> Result<Vec<u8>, crate::exec::Error> {
        if path.contains(&b'/') {
            return Ok(path.to_vec());
        }
        let search = envp
            .iter()
            .find_map(|entry| entry.strip_prefix(b"PATH=".as_slice()))
            .unwrap_or(b"/usr/local/bin:/usr/bin:/bin");
        for directory in search.split(|byte| *byte == b':') {
            let mut candidate = match directory.is_empty() {
                // An empty element means the working directory, which for a
                // container that has not started is the root.
                true => b"/".to_vec(),
                false => directory.to_vec(),
            };
            if !candidate.ends_with(b"/") {
                candidate.push(b'/');
            }
            candidate.extend_from_slice(path);
            let root = kernel.vfs.root();
            if kernel
                .vfs
                .resolve(root, &candidate, crate::vfs::Lookup::FOLLOW)
                .is_ok()
            {
                return Ok(candidate);
            }
        }
        Err(crate::exec::Error::NotLoadable(
            "the entrypoint is a bare name and no directory on `PATH` has it",
        ))
    }

    /// One turn of the loop: run a quantum, then decide what stopped it.
    ///
    /// A turn, and not a run: what drives a container to completion is
    /// [`crate::system::System`], because a `fork` needs the table of every
    /// process and a process is not that. There is deliberately no
    /// `Process::run` — one would be a loop that turns every fork into an
    /// error, which is the shape of refusal this engine spent a section of
    /// `docs/vm.md` getting rid of.
    /// The same, saying what it needs when it needs something.
    pub fn step(&mut self, quantum: u64) -> Progress {
        // Before anything runs, and therefore *between blocks*: the control
        // block is consistent exactly at retirement boundaries, and a frame
        // built from a half-executed block would carry a lazy-flag record
        // and a partly-advanced program counter that are not a machine.
        // Linux delivers on the way back to userspace, which is strictly
        // coarser, so nothing observable is lost.
        // Before anything else this turn: a thread parked on a pipe that
        // has since been written to is runnable again, and it has to be so
        // *before* the scheduler is asked what to run.
        self.resume_transfers();
        self.resume_watches();
        self.resume_accepts();
        self.resume_events();
        self.resume_paused();
        // A process can be given the processor back with its current thread
        // still parked: `Progress::Idle` hands the turn away without
        // choosing a successor, because at that moment there was none. So
        // the choice happens here, on the way in, and a process that still
        // has nothing to run hands the turn straight back rather than
        // interpreting a thread that is in the middle of a syscall.
        if !self.kernel.machine.threads.current().is_runnable()
            && !self.kernel.machine.threads.schedule()
        {
            return Progress::Idle;
        }
        if let Some(signal) = self.kernel.machine.threads.current().deliverable()
            && let Some(exit) = self.raise(signal, crate::signal::Cause {
                code: crate::signal::code::TKILL,
                address: 0,
            })
        {
            return Progress::Finished(exit);
        }

        // Three disjoint fields of one owner, which is what makes the
        // engine able to hold no state of its own.
        let outcome = Engine::run(
            self.kernel.machine.thread_mut(),
            &mut self.kernel.pages,
            &mut self.cache,
            quantum,
        );
        let preempted = matches!(outcome, Step::Preempted);
        let (finished, reschedule) = match outcome {
            // The quantum ran out with the thread still runnable, which is
            // the only reason this loop ever takes a thread off the
            // processor against its will. Denominated in *retired
            // instructions*, so the switch happens at the same point in
            // every run of the same container: preemptive and deterministic
            // at once, which a wall-clock quantum could not be.
            Step::Preempted => (None, true),
            Step::Syscall => {
                let served = self.serve();
                if let Served::Requested(request) = served {
                    return Progress::Requested(request);
                }
                let finished = match served {
                    Served::Finished(exit) => Some(exit),
                    _ => None,
                };
                // A syscall that parked or ended the thread leaves it not
                // runnable, and somebody else has to be chosen. One that
                // returned normally does not: switching on every syscall
                // would be a scheduler that never lets a thread finish
                // anything.
                let runnable = self.kernel.machine.threads.current().is_runnable();
                (finished, !runnable)
            }
            Step::Trap(trap) => match self.fault(trap) {
                // Caught: the thread is now in a handler and the loop goes
                // round again.
                Exit::Delivered => (None, false),
                ending => (Some(ending), false),
            },
        };
        if let Some(exit) = finished {
            return Progress::Finished(exit);
        }
        if reschedule && !self.kernel.machine.threads.schedule() {
            // Nothing here can run. Whether that is a deadlock depends on
            // what the other processes are doing, which this does not know.
            return Progress::Idle;
        }
        match preempted {
            true => Progress::Preempted,
            false => Progress::Running,
        }
    }

    /// Whether any of this process's threads could run now.
    ///
    /// Including one parked mid-transfer on a pipe whose other end has since
    /// moved — that thread is not runnable *yet*, but it will be the moment
    /// it is looked at, which is a reason to look at it. The check reaches
    /// the shared pipe table through this process's own kernel, so a
    /// container never needs a scheduler that knows what a pipe is.
    pub fn runnable(&self) -> bool {
        let rings = self.kernel.rings.borrow();
        self.kernel.machine.threads.all().iter().any(|thread| {
            match thread.state {
                crate::thread::State::Transferring(transfer) => transfer.ready(&rings),
                // A wait with a deadline is always worth a turn, because the
                // only way to find out whether the deadline has passed is to
                // look at the clock — and looking at the clock crosses the
                // host boundary, which a scheduling decision must not do. So
                // a container whose only pending work is a timeout spins
                // until it fires. There is no sleep to call: the boundary is
                // two `ll-store` imports and neither of them waits.
                // A deadline is *not* readiness. Answering that it is makes
                // every process waiting on a timeout look busy, and a
                // container whose processes wait on sixty-second timeouts
                // then spends its whole processor re-checking them —
                // starving the one process with work to do. Deadlines are
                // handled once per idle pass, by the system, which is the
                // only place that can read a clock without a scheduling
                // decision depending on when it did.
                crate::thread::State::Watching(watching) => {
                    self.kernel.watch_ready(watching.watch)
                }
                // Waiting for a connection, and one has queued. All arena
                // state, which is what lets a scheduling decision ask it.
                crate::thread::State::Accepting(waiting) => {
                    self.kernel.sockets.borrow().queued(waiting.listener) > 0
                }
                // The counter has moved, or has room.
                crate::thread::State::Eventing(waiting) => {
                    let events = self.kernel.events.borrow();
                    match waiting.writing {
                        true => events.writable(waiting.event, waiting.value),
                        false => events.readable(waiting.event),
                    }
                }
                // Not runnable, and neither is a sleeper: see the comment on
                // `Watching` above. Both are woken by `expire`.
                crate::thread::State::Waiting { .. }
                | crate::thread::State::Sleeping { .. } => false,
                // Waiting for a signal, and one has arrived.
                crate::thread::State::Paused => thread.deliverable().is_some(),
                _ => thread.is_runnable(),
            }
        })
    }

    /// Moves every parked transfer that can move, and wakes the ones that
    /// finish.
    ///
    /// **In the transferring process's own turn**, which is the rule the
    /// whole process table is built around: the buffer is a guest address,
    /// and a guest address means this process's bytes only while this
    /// process is the one at the guest's addresses. A writer that filled a
    /// pipe cannot complete the reader's `read` on its way past, however
    /// convenient that would be.
    /// Completes every parked `poll` or `epoll_wait` whose answer has
    /// arrived — or whose deadline has passed, which is the same call
    /// answering zero.
    fn resume_watches(&mut self) {
        let parked: Vec<(usize, crate::thread::Watching)> = self
            .kernel
            .machine
            .threads
            .all()
            .iter()
            .enumerate()
            .filter_map(|(slot, thread)| match thread.state {
                crate::thread::State::Watching(watching) => Some((slot, watching)),
                _ => None,
            })
            .collect();
        for (slot, watching) in parked {
            let ready = self.kernel.watch_ready(watching.watch);
            if !ready && !self.kernel.expired(watching.deadline) {
                continue;
            }
            // Writes into the guest's own memory, which is why this is here
            // and not wherever the descriptor became ready.
            let answer = match ready {
                true => self.kernel.report_ready(watching.watch),
                // The deadline passed with nothing ready, which `poll` and
                // `epoll_wait` both report as zero rather than as an error.
                false => 0,
            };
            self.kernel.release_watch(watching.watch);
            let thread = &mut self.kernel.machine.threads.all_mut()[slot];
            thread.state = crate::thread::State::Runnable;
            thread.tcb.registers[0] = answer as u64;
        }
    }

    /// Wakes every thread in `pause` that now has a signal to take.
    ///
    /// With `EINTR` already in `%rax`, before the handler runs — which is
    /// the order the guest observes: the frame the handler returns through
    /// carries the interrupted call's answer, so `sigreturn` lands back in
    /// `pause` with the error it is documented to always give.
    /// Completes every parked `accept` whose connection has arrived.
    ///
    /// On the parked process's own turn, because the peer's address is
    /// written into that process's memory — the same rule the transfers and
    /// the waits obey, and the reason none of them completes on the turn of
    /// whoever made them ready.
    fn resume_accepts(&mut self) {
        let parked: Vec<(usize, crate::thread::Accepting)> = self
            .kernel
            .machine
            .threads
            .all()
            .iter()
            .enumerate()
            .filter_map(|(slot, thread)| match thread.state {
                crate::thread::State::Accepting(waiting) => Some((slot, waiting)),
                _ => None,
            })
            .collect();
        for (slot, waiting) in parked {
            let Some(answer) = self.kernel.complete_accept(waiting) else {
                continue;
            };
            let thread = &mut self.kernel.machine.threads.all_mut()[slot];
            thread.state = crate::thread::State::Runnable;
            thread.tcb.registers[0] = answer as u64;
        }
    }

    /// Completes every parked `eventfd` read or write whose counter moved.
    fn resume_events(&mut self) {
        let parked: Vec<(usize, crate::thread::Eventing)> = self
            .kernel
            .machine
            .threads
            .all()
            .iter()
            .enumerate()
            .filter_map(|(slot, thread)| match thread.state {
                crate::thread::State::Eventing(waiting) => Some((slot, waiting)),
                _ => None,
            })
            .collect();
        for (slot, waiting) in parked {
            let Some(answer) = self.kernel.complete_event(waiting) else {
                continue;
            };
            let thread = &mut self.kernel.machine.threads.all_mut()[slot];
            thread.state = crate::thread::State::Runnable;
            thread.tcb.registers[0] = answer as u64;
        }
    }

    fn resume_paused(&mut self) {
        for thread in self.kernel.machine.threads.all_mut() {
            if thread.state == crate::thread::State::Paused && thread.deliverable().is_some() {
                thread.state = crate::thread::State::Runnable;
                thread.tcb.registers[0] = crate::errno::Errno::Interrupted.as_result() as u64;
                // A suspend put a mask aside for the length of the wait, and
                // it goes back *before* the handler runs — which is what
                // `rt_sigsuspend` promises and what makes it different from
                // `pause`.
                if let Some(previous) = thread.owned.suspended_mask.take() {
                    thread.owned.blocked_signals = previous;
                }
            }
            // A sleep is cut short by a signal too, and Linux reports the
            // time remaining — which nothing here asks for, so the answer is
            // `EINTR` and no remainder.
            if matches!(thread.state, crate::thread::State::Sleeping { .. })
                && thread.deliverable().is_some()
            {
                thread.state = crate::thread::State::Runnable;
                thread.tcb.registers[0] = crate::errno::Errno::Interrupted.as_result() as u64;
            }
        }
    }

    /// The soonest moment anything in this process is waiting for, in
    /// nanoseconds on the monotonic clock.
    ///
    /// What the system asks when nothing can run: it is how long the whole
    /// container may be left alone for, and — once there is a host wait to
    /// hand it to — how long to sleep.
    pub fn earliest_deadline(&self) -> Option<u64> {
        self.kernel
            .machine
            .threads
            .all()
            .iter()
            .filter_map(|thread| match thread.state {
                crate::thread::State::Sleeping { deadline } => Some(deadline),
                crate::thread::State::Waiting { deadline, .. } => deadline,
                crate::thread::State::Watching(watching) => watching.deadline,
                _ => None,
            })
            .min()
    }

    /// Wakes every thread whose deadline has passed, and answers whether any
    /// did.
    ///
    /// Given the time rather than reading it, because the system reads the
    /// clock **once** for the whole container: a clock read crosses the host
    /// boundary, and one per process per pass is a boundary crossing in the
    /// scheduler's inner loop.
    pub fn expire(&mut self, now: u64) -> bool {
        let mut woken = false;
        let mut watches: Vec<(usize, crate::thread::Watching)> = Vec::new();
        for (slot, thread) in self.kernel.machine.threads.all_mut().iter_mut().enumerate() {
            match thread.state {
                crate::thread::State::Sleeping { deadline } if now >= deadline => {
                    thread.state = crate::thread::State::Runnable;
                    thread.tcb.registers[0] = 0;
                    woken = true;
                }
                // `ETIMEDOUT` is what `pthread_cond_timedwait` reads to
                // learn its wait was the whole of the time it asked for.
                crate::thread::State::Waiting {
                    deadline: Some(deadline),
                    ..
                } if now >= deadline => {
                    thread.state = crate::thread::State::Runnable;
                    thread.tcb.registers[0] =
                        crate::errno::Errno::TimedOut.as_result() as u64;
                    woken = true;
                }
                // A `poll` or `select` whose time ran out answers *zero*,
                // not an error: no descriptor was ready, and that is a
                // complete answer to the question it asked.
                crate::thread::State::Watching(watching)
                    if watching.deadline.is_some_and(|deadline| now >= deadline) =>
                {
                    watches.push((slot, watching));
                }
                _ => {}
            }
        }
        for (slot, watching) in watches {
            self.kernel.release_watch(watching.watch);
            let thread = &mut self.kernel.machine.threads.all_mut()[slot];
            thread.state = crate::thread::State::Runnable;
            thread.tcb.registers[0] = 0;
            woken = true;
        }
        woken
    }

    fn resume_transfers(&mut self) {
        let parked: Vec<(usize, crate::ring::Transfer)> = self
            .kernel
            .machine
            .threads
            .all()
            .iter()
            .enumerate()
            .filter_map(|(slot, thread)| match thread.state {
                crate::thread::State::Transferring(transfer) => Some((slot, transfer)),
                _ => None,
            })
            .collect();
        for (slot, transfer) in parked {
            if !transfer.ready(&self.kernel.rings.borrow()) {
                continue;
            }
            match self.kernel.advance_transfer(transfer) {
                crate::ring::Progress::Done(answer) => {
                    let thread = &mut self.kernel.machine.threads.all_mut()[slot];
                    thread.state = crate::thread::State::Runnable;
                    thread.tcb.registers[0] = answer as u64;
                }
                // It moved some and still cannot finish, which is a large
                // write against a reader that is keeping up slowly.
                crate::ring::Progress::Waiting(rest) => {
                    self.kernel.machine.threads.all_mut()[slot].state =
                        crate::thread::State::Transferring(rest);
                }
            }
        }
    }

    /// Serves one syscall.
    fn serve(&mut self) -> Served {
        // Whatever the host placed in guest memory for the previous call is
        // dead now. The arena's lifetime is one call, and that is what stops
        // the boundary leaking — the ahead-of-time seam does this at the top
        // of its dispatch and so must this one. Without it a container runs
        // perfectly well until the arena fills, which for a Python process
        // is about thirty-eight seconds in and looks like the host refusing
        // a forty-four byte read.
        crate::abi::reset_transfer_arena();
        let thread = self.kernel.machine.thread();
        let number = thread.registers[NUMBER] as i64;
        let raw = ARGUMENTS.map(|index| thread.registers[index] as i64);
        let arguments = Arguments::new(raw);
        let answer = self.kernel.dispatch(number, arguments);
        self.record(number, &raw, &answer);
        match answer {
            Outcome::Done(value) => {
                self.kernel.machine.thread_mut().registers[NUMBER] = value as u64;
                Served::Returned
            }
            Outcome::Exit(status) => Served::Finished(Exit::Status(status)),
            Outcome::Fault(fault) => Served::Finished(Exit::Unimplemented(fault)),
            // The thread parked on a futex, or ended. Either way it is not
            // runnable and the caller picks somebody else; there is nothing
            // to write back, because a parked thread has not returned yet.
            Outcome::Blocked => Served::Returned,
            Outcome::Process(request) => Served::Requested(request),
        }
    }

    /// Writes a syscall's answer into the thread that asked, for the
    /// requests the system completes on the kernel's behalf.
    pub fn answer(&mut self, value: i64) {
        self.kernel.machine.thread_mut().registers[NUMBER] = value as u64;
    }

    /// Writes one line of syscall trace, if a trace was asked for.
    ///
    /// The same renderer the ahead-of-time seam uses, and deliberately the
    /// same: the acceptance test for a booted container is a diff against a
    /// real `strace`, and a second format would have to be translated before
    /// it could be compared — which is how two traces come to disagree in
    /// ways nobody can attribute.
    fn record(&mut self, number: i64, arguments: &[i64; 6], answer: &Outcome) {
        if !self.kernel.tracing() {
            return;
        }
        let rendered = match answer {
            Outcome::Done(value) => value.to_string(),
            Outcome::Fault(_) => String::from("<fault>"),
            Outcome::Blocked => String::from("<blocked>"),
            Outcome::Process(_) => String::from("<process>"),
            Outcome::Exit(status) => format!("<exit {status}>"),
        };
        let line = crate::traced(&mut self.kernel, number, arguments, &rendered);
        crate::trace(&mut self.kernel, &line);
    }

    /// Delivers a signal, or reports what its default action does.
    ///
    /// `None` means the thread is now running a handler and the loop should
    /// carry on; `Some` means nothing caught it and the process is over.
    fn raise(&mut self, signal: i32, cause: crate::signal::Cause) -> Option<Exit> {
        let delivery = self.kernel.deliver(signal, cause);
        if delivery == crate::syscall::Delivery::Ran {
            return None;
        }
        self.declined(signal, delivery);
        self.kernel.machine.owned().pending_signals &= !(1u64 << (signal - 1));
        // Pending, and by the time it was reached no longer caught — the
        // program changed the disposition between the two moments, which is
        // exactly what a `SIGCHLD` handler that deinstalls itself does. The
        // default action decides, and for this handful of signals the
        // default action is to do nothing.
        if !crate::syscall::terminates(i64::from(signal)) {
            return None;
        }
        // Nothing caught it. What a shell reports for a process killed by a
        // signal is 128 plus the number.
        Some(Exit::Signalled {
            signal,
            address: cause.address,
            rip: self.kernel.machine.thread().rip,
            access: None,
        })
    }

    /// Turns a trap into what the guest sees.
    ///
    /// **A fault is a signal now, and a handler can catch it.** That is the
    /// fidelity class the ahead-of-time design documents as impossible: a
    /// null dereference there reads whatever is at address zero and carries
    /// on, guard pages cannot be enforced, and a stack overflow corrupts
    /// silently. Here the address space refused the access, the loop turns
    /// the refusal into `SIGSEGV` with a faithful `si_addr`, and a guest
    /// that installed a handler runs it — on its alternate stack, if the
    /// reason its own stack cannot be used is that the stack is what
    /// overflowed.
    ///
    /// The program counter is left on the faulting instruction, so a handler
    /// that fixes the mapping and returns re-runs it, which is how a
    /// copy-on-write page or a guard page is made to work at all.
    fn fault(&mut self, trap: Trap) -> Exit {
        let rip = self.kernel.machine.thread().rip;
        /// The numbers a shell reports and a `siginfo` carries.
        const SIGILL: i32 = 4;
        const SIGTRAP: i32 = 5;
        const SIGFPE: i32 = 8;
        const SIGSEGV: i32 = 11;
        use crate::signal::{Cause, code};
        let (signal, cause, access) = match trap {
            Trap::Fault(fault) => (
                SIGSEGV,
                Cause {
                    // Which kind of refusal it was, which a handler reads to
                    // decide what to do: a page that is not there can be
                    // mapped, a page that refused the access cannot.
                    code: match fault.access {
                        targum::space::Access::Write => code::ACCERR,
                        _ => code::MAPERR,
                    },
                    address: fault.address,
                },
                Some(fault.access),
            ),
            Trap::Privileged { address } | Trap::Misaligned { address } => (
                SIGSEGV,
                Cause {
                    code: code::ACCERR,
                    address,
                },
                None,
            ),
            Trap::Undefined { address } => (
                SIGILL,
                Cause {
                    code: code::ILLOPN,
                    address,
                },
                None,
            ),
            Trap::DivideError { address } => (
                SIGFPE,
                Cause {
                    code: code::INTDIV,
                    address,
                },
                None,
            ),
            Trap::Breakpoint { address } => (
                SIGTRAP,
                Cause {
                    code: code::BRKPT,
                    address,
                },
                None,
            ),
            // Not a guest-visible condition. The engine is incomplete, and
            // no handler the guest installed has anything to say about that.
            Trap::Unsupported(unsupported) => return Exit::Unsupported(unsupported),
        };
        let delivery = self.kernel.deliver(signal, cause);
        if delivery == crate::syscall::Delivery::Ran {
            return Exit::Delivered;
        }
        self.declined(signal, delivery);
        Exit::Signalled {
            signal,
            address: cause.address,
            rip,
            access,
        }
    }

    /// Says why a signal the program asked to catch was not delivered.
    ///
    /// `NotCaught` is silent, because it is the program's own decision. The
    /// rest are the kernel failing to do something it *was* asked to, and a
    /// process that dies of a signal it installed a handler for should not
    /// have to be debugged from the outside.
    fn declined(&mut self, signal: i32, delivery: crate::syscall::Delivery) {
        use crate::syscall::Delivery;
        if delivery == Delivery::NotCaught {
            return;
        }
        let mut message = String::from("kisal: signal ");
        crate::push_decimal(&mut message, i64::from(signal));
        message.push_str(match delivery {
            Delivery::NoRestorer => " has a handler whose disposition named no restorer",
            Delivery::NoStack { .. } => {
                " has a handler, and the stack it would run on is not writable"
            }
            Delivery::NoControlBlock => " has a handler, and this machine has no control block",
            _ => " was not delivered",
        });
        if let Delivery::NoStack { at } = delivery {
            message.push_str(" at ");
            crate::push_hex(&mut message, at);
        }
        message.push('\n');
        crate::report_to(&mut self.kernel, &message);
    }
}
