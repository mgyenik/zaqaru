//! More than one process: the table, the switch, and what a fork is.
//!
//! A process is an address space, a kernel, and the threads running in it.
//! Two live processes cannot share one address space — a guest address is a
//! linear-memory offset and the child needs the parent's addresses — so a
//! process here is an address space, and a switch is what moves one aside
//! for another (see [`crate::resident`]).
//!
//! A fork is cheap because the machine is data: the child's state is a
//! control block and its address space is bytes, and the child resumes by
//! *being interpreted*, which is the only thing the loop ever does.
//!
//! So a fork is three steps and none of them is subtle:
//!
//! 1. duplicate the address space — one kernel-side copy of a file;
//! 2. copy the kernel's own state, field by field, which is
//!    [`crate::syscall::Kernel::fork`];
//! 3. in the child, `%rax = 0`. `%rip` is already past the `syscall`.


use crate::abi::Store;
use crate::machine::Interpreted;
use crate::run::{Exit, Process, Progress};
use crate::syscall::Request;

/// The first process's identifier.
pub const FIRST: i32 = 1;

/// The longest the container will sleep before looking at the host again.
///
/// **Invisible to the guest**, which is what makes it a free knob: waking
/// early does not return anything to a parked thread, it only re-checks
/// readiness and goes back to sleep. What it bounds is how long the *host*
/// waits to be noticed — a shutdown request arrives on a path nothing is
/// waiting on, so without a cap a container asleep until nginx's
/// sixty-second `epoll_wait` takes sixty seconds to answer a Ctrl-C.
///
/// A quarter of a second: four wakeups a second while idle, which measured
/// at 0% of a core on the demo stack, against a shutdown that feels
/// immediate.
pub const PATIENCE: u64 = 250;

/// How many thread quanta a process gets before another one is considered.
///
/// A *process* switch is not the free thing a thread switch is. Natively it
/// is one `MAP_FIXED` and costs nothing; inside the module there is no page
/// table to swap and it is a copy of everything the process has mapped — so
/// rotating on every thread quantum would put a memcpy of the guest's whole
/// heap between every hundred thousand instructions, which is not a
/// scheduler, it is a memcpy benchmark.
///
/// The same number in both builds, deliberately. It is denominated in
/// retired instructions like everything else here, so the interleaving stays
/// a pure function of execution — and a native run and a module run of the
/// same container schedule identically, which is what makes the native tests
/// evidence about the module.
///
/// Sixteen, so a process holds the processor for 1.6 million instructions:
/// long enough that the copy is amortised even when two processes are both
/// compute-bound, short enough that a container of them still interleaves at
/// a granularity a human would call fair. The shapes that actually matter —
/// a `fork` whose parent immediately waits, a `fork` and `exec` — never
/// reach it, because a process that blocks yields at once.
pub const SLICE: u64 = 16;

/// One process: a kernel with its own address space, and the blocks decoded
/// out of it.
///
/// The block cache is per process because the *bytes* are: two processes
/// with different memory at one address must not share a decode, and after
/// a fork they diverge the moment either writes.
pub struct Container<'a, S: Store> {
    pub process: Process<'a, S>,
    pub pid: i32,
    pub parent: i32,
    /// Set when it has ended and nothing has reaped it — a zombie, which
    /// exists so that a parent can still ask what happened.
    pub status: Option<Ending>,
}

/// Every process, and which one is running.
pub struct System<'a, S: Store> {
    containers: Vec<Container<'a, S>>,
    current: usize,
    next: i32,
    /// How many thread quanta the running process has used since it was
    /// given the processor. See [`SLICE`].
    slice: u64,
    /// Whether the host's shutdown request has already been delivered.
    stopping: bool,
    /// How the *first* process ended, in full.
    ///
    /// [`Ending`] keeps only what `wait4` can report — a code or a signal
    /// number — because that is all a parent may see. The container's own
    /// report is not a `wait4`: when a run stops because something
    /// segmentation-faulted, the address and the program counter are the
    /// entire diagnosis, and rebuilding an `Exit` from an `Ending` throws
    /// them away. Measured: a fault at `0x60012d0` in glibc's
    /// `_dl_non_dynamic_init` was reported as `address: 0, rip: 0`, and
    /// finding it again took a debugging session that the numbers would
    /// have ended in a line.
    ending: Option<Exit>,
    /// What processes that are gone retired and decoded before they went.
    ///
    /// Kept here because ending destroys the only other place those numbers
    /// lived — a process lets go of its control blocks and its block cache
    /// the moment it exits — and a container that forks does most of its
    /// work in children: a rate computed from the survivors would understate
    /// the engine by however much the guest chose to fan out.
    departed: u64,
    departed_accelerated: u64,
    departed_blocks: u64,
}

impl<'a, S: Store + Clone> System<'a, S> {
    /// One process, running.
    pub fn new(process: Process<'a, S>) -> Self {
        Self {
            containers: vec![Container {
                process,
                pid: FIRST,
                parent: 0,
                status: None,
            }],
            current: 0,
            next: FIRST + 1,
            slice: 0,
            stopping: false,
            ending: None,
            departed: 0,
            departed_accelerated: 0,
            departed_blocks: 0,
        }
    }

    pub fn current(&mut self) -> &mut Process<'a, S> {
        &mut self.containers[self.current].process
    }

    pub fn current_pid(&self) -> i32 {
        self.containers[self.current].pid
    }

    /// Instructions retired by every process, living or gone.
    pub fn retired(&self) -> u64 {
        self.containers
            .iter()
            .flat_map(|container| container.process.kernel.machine.threads.all())
            .map(|thread| thread.tcb.retired)
            .fold(self.departed, u64::saturating_add)
    }

    /// Instructions retired inside bytecode traces, every process.
    pub fn accelerated(&self) -> u64 {
        self.containers
            .iter()
            .flat_map(|container| container.process.kernel.machine.threads.all())
            .map(|thread| thread.tcb.accelerated)
            .fold(self.departed_accelerated, u64::saturating_add)
    }

    /// Blocks decoded by every process, living or gone.
    pub fn decoded(&self) -> u64 {
        self.containers
            .iter()
            .map(|container| container.process.cache.decoded as u64)
            .fold(self.departed_blocks, u64::saturating_add)
    }

    /// Forks the running process, and answers the child's identifier.
    ///
    /// The child is not made current: the parent returns from its `fork`
    /// first, exactly as a scheduler that put the child on the run queue
    /// would leave it.
    pub fn fork(&mut self) -> Option<i32> {
        let parent = self.current_pid();
        let (kernel, cache) = {
            let process = &self.containers[self.current].process;
            let machine = process.kernel.machine.fork(&process.kernel.mapped_ranges())?;
            // The child's bytes are the parent's, but its cache is its own:
            // a block cache is keyed by address and the two address spaces
            // will diverge. The policy is inherited.
            (process.kernel.fork(machine), process.cache.fresh())
        };
        let pid = self.next;
        self.next += 1;
        let mut child = Process { kernel, cache };
        // The numbers the kernel could not know: it is a copy of the
        // parent's, and only the table decides who a new process is.
        child.kernel.pid = pid;
        child.kernel.parent = parent;
        // `fork` returns zero in the child. `%rip` is already past the
        // `syscall`, so the child simply continues.
        child.kernel.machine.thread_mut().registers[0] = 0;
        self.containers.push(Container {
            process: child,
            pid,
            parent,
            status: None,
        });
        Some(pid)
    }

    /// Makes `index` the running process, moving its address space to the
    /// guest's addresses.
    fn switch(&mut self, index: usize) {
        if index == self.current {
            return;
        }
        self.containers[self.current].process.kernel.deactivate();
        self.current = index;
        self.containers[index].process.kernel.activate();
    }

    /// Picks the next process with something to run, round-robin.
    ///
    /// Round-robin over *processes* on top of round-robin over threads, and
    /// both are driven by the retired-instruction quantum — so the whole
    /// interleaving, across processes as well as threads, is a pure function
    /// of execution.
    fn schedule(&mut self, why: Yield) -> bool {
        // A quantum expiring is a *thread* scheduling point. It becomes a
        // process scheduling point once the process has had a whole slice of
        // them — see [`SLICE`]. A process that cannot continue gives up its
        // turn whatever is left of the slice, which is not the same thing
        // and must not be counted as one.
        self.slice += u64::from(why == Yield::Quantum);
        let rotate = match why {
            Yield::Given => true,
            _ => self.slice >= SLICE,
        };
        if rotate {
            self.slice = 0;
        }
        let count = self.containers.len();
        // From the next one when the quantum expired, and from this one
        // otherwise: a process that has just returned from a syscall keeps
        // the processor, and a process that has used its quantum gives it
        // up. Switching on every syscall would be two mappings per `write`.
        let first = usize::from(rotate);
        for step in first..first + count {
            let candidate = (self.current + step) % count;
            if self.ready(candidate) {
                if candidate != self.current {
                    // Somebody else's turn, which restarts the count
                    // whether the switch was the slice's doing or a block's.
                    self.slice = 0;
                }
                self.switch(candidate);
                self.collect();
                return true;
            }
        }
        // Nothing else; the current one may still be able to go.
        if !rotate && self.ready(self.current) {
            self.collect();
            return true;
        }
        false
    }

    /// Whether a process could be given the processor.
    ///
    /// Either a thread is runnable, or one is parked in `wait4` with a child
    /// that has finished — which is a thread that will become runnable the
    /// moment it is looked at, and so is a reason to look at it.
    fn ready(&self, index: usize) -> bool {
        let container = &self.containers[index];
        if container.status.is_some() {
            return false;
        }
        if container.process.runnable() {
            return true;
        }
        self.parked(index)
            .into_iter()
            .any(|(_, wanted, _)| self.reapable(container.pid, wanted))
    }

    /// The threads of `index` parked in `wait4`, with what each asked for.
    fn parked(&self, index: usize) -> Vec<(usize, i32, u64)> {
        self.containers[index]
            .process
            .kernel
            .machine
            .threads
            .all()
            .iter()
            .enumerate()
            .filter_map(|(slot, thread)| match thread.state {
                crate::thread::State::WaitingForChild { wanted, status_at } => {
                    Some((slot, wanted, status_at))
                }
                _ => None,
            })
            .collect()
    }

    /// Whether `parent` has a finished child matching `wanted`.
    fn reapable(&self, parent: i32, wanted: i32) -> bool {
        self.containers.iter().any(|container| {
            container.parent == parent
                && (wanted <= 0 || container.pid == wanted)
                && container.status.is_some()
        })
    }

    /// Completes the running process's parked `wait4` calls.
    ///
    /// **Here, and not when the child exits.** A process's bytes exist at
    /// the guest's addresses only while it is the current one, so the status
    /// word a `wait4` was told to fill can only be written after the switch
    /// — which is also where a real `wait4` returns, in the caller's own
    /// address space, rather than in whatever process happened to die.
    fn collect(&mut self) {
        let parent = self.current_pid();
        for (slot, wanted, status_at) in self.parked(self.current) {
            let Reaped::Exited { pid, status } = self.reap(parent, wanted) else {
                continue;
            };
            self.report(status_at, status);
            let threads = &mut self.current().kernel.machine.threads;
            let thread = &mut threads.all_mut()[slot];
            thread.state = crate::thread::State::Runnable;
            thread.tcb.registers[0] = pid as u64;
        }
    }

    /// Reaps a child that has exited, for `wait4`.
    ///
    /// Answers the identifier and status of one, or `None` when the caller
    /// has no children at all — which is the difference between `ECHILD` and
    /// "wait a moment".
    pub fn reap(&mut self, parent: i32, wanted: i32) -> Reaped {
        let mut any = false;
        for index in 0..self.containers.len() {
            let container = &self.containers[index];
            if container.parent != parent {
                continue;
            }
            if wanted > 0 && container.pid != wanted {
                continue;
            }
            any = true;
            if let Some(status) = container.status {
                let pid = container.pid;
                // Its counts were taken when it ended, which is also when it
                // let go of everything else — so removing it here adds
                // nothing and must not add it twice.
                self.containers.remove(index);
                if self.current > index {
                    self.current -= 1;
                }
                return Reaped::Exited { pid, status };
            }
        }
        match any {
            true => Reaped::Running,
            false => Reaped::None,
        }
    }

    /// Runs until the container is finished, and answers what the first
    /// process ended with — which is the container's status, exactly as a
    /// process tree's is on Linux.
    /// Runs the container to completion.
    pub fn run(&mut self) -> Exit {
        loop {
            if let Turn::Finished(exit) = self.turn() {
                return exit;
            }
        }
    }

    /// One turn of the scheduler: the running process gets a quantum, the
    /// edge is pumped where the rules say, and the next process is chosen.
    ///
    /// The whole run loop is this, repeated. It is one call rather than a
    /// loop so that a host can hold the machine between turns — with no
    /// state on the wasm stack, linear memory between two turns *is* the
    /// machine, which is what a snapshot copies and a debugger stops at.
    pub fn turn(&mut self) -> Turn {
        let why = match self.current().step(cpu::QUANTUM) {
            Progress::Running => Yield::Kept,
            // The quantum expired, so every process gets a turn — the
            // same rule the threads inside one already follow, and with
            // the same consequence: the whole interleaving, across
            // processes as well as threads, is a function of how many
            // instructions have retired.
            Progress::Preempted => Yield::Quantum,
            // This process is waiting for something only another one can
            // do. Give somebody else a turn; if nobody can take it, the
            // loop below is what says so.
            Progress::Idle => Yield::Given,
            Progress::Requested(request) => {
                self.answer(request);
                Yield::Kept
            }
            Progress::Finished(exit) => {
                // The edge, one last time. A process that writes a
                // reply and exits has put those bytes in a ring and
                // nowhere else, and the two scheduled pump points are
                // both about a container that keeps running: a whole
                // slice used, or nothing left to run. Neither happens
                // again after the last process goes, so without this
                // the final write of a run is dropped on the floor.
                //
                // Before `finish`, because that is what tears the
                // process down — and closing its descriptors is what
                // releases the ring the bytes are still sitting in.
                //
                // A long-lived server never showed this: nginx is
                // always still there to be pumped on somebody else's
                // turn. It took a program that answers once and exits.
                self.current().kernel.pump(None);
                if let Some(ending) = self.finish(exit) {
                    return Turn::Finished(ending);
                }
                Yield::Given
            }
        };
        // The edge, at a point that is a pure function of execution: a
        // process has used a whole slice. Kernel state only — rings, the
        // socket arena, the store — so it may run on any turn without
        // breaking the rule the process table is built on.
        // Every quantum, not every slice. The slice is about *fairness*
        // — how long a process holds the processor before another gets a
        // turn — and tying the edge to it made a response the guest had
        // already written wait up to 1.6 million instructions, some
        // fifty milliseconds of interpretation, before it reached the
        // host.
        //
        // Sequentially that never showed: a container with nothing left
        // to run goes idle, and the idle path pumps at once. Under
        // concurrency nothing is idle — nginx and gunicorn always have
        // work — so everything waited for the slice boundary, and four
        // clients at once measured *worse* throughput than one at a
        // time. Honest queueing behind a single worker would have held
        // it flat.
        //
        // Still denominated in retired instructions, which is the
        // property that matters: scheduling stays a pure function of
        // execution, so a recorded run still replays. What it costs is
        // sixteen times as many `/iso/net/events` reads — a few hundred
        // a second, against a syscall budget in the millions.
        if why == Yield::Quantum {
            // Sending, every quantum. This is what a response waits on,
            // and tying it to the slice made one sit in a ring for 1.6
            // million instructions — some fifty milliseconds of
            // interpretation — before it reached the host.
            self.current().kernel.flush_edges();
        }
        // Receiving, and the clock, and the shutdown switch: once a
        // slice. The inbound read is a host call, and doing it sixteen
        // times as often cost more throughput than the promptness was
        // worth, because it scales with the number of open connections.
        if why == Yield::Quantum && self.slice == 0 {
            self.current().kernel.pump(None);
            self.current().kernel.refresh_timebase();
            self.take_shutdown();
        }
        if !self.schedule(why) {
            // Nothing runnable anywhere, which is three different things
            // and only one of them is a deadlock.
            if self.idle() {
                return Turn::Idle;
            }
            return Turn::Finished(match self.ending.clone() {
                Some(ending) => ending,
                None => Exit::Deadlocked,
            });
        }
        Turn::Ran
    }

    /// Why nothing can run, process by process and thread by thread.
    ///
    /// A container that stops with "deadlocked" and nothing else sends
    /// whoever reads that to a debugger, and the information they will look
    /// for is exactly this: which processes exist, which threads they have,
    /// and what each one is parked on. It is cheap — the state is right
    /// there — and it is the difference between a bug report and a bisect.
    pub fn stall(&self) -> String {
        use core::fmt::Write;
        let mut into = String::new();
        for container in &self.containers {
            let _ = write!(
                &mut into,
                "  process {} (parent {})",
                container.pid, container.parent
            );
            if let Some(status) = container.status {
                let _ = write!(&mut into, " {status:?}, unreaped");
            }
            into.push('\n');
            for thread in container.process.kernel.machine.threads.all() {
                let _ = write!(&mut into, "    thread {} ", thread.tid);
                match thread.state {
                    crate::thread::State::Runnable => into.push_str("runnable"),
                    crate::thread::State::Waiting {
                        word,
                        bitset,
                        deadline,
                    } => {
                        let _ = write!(&mut into, "parked on futex {word:#x} bitset {bitset:#x}");
                        if let Some(deadline) = deadline {
                            let _ = write!(&mut into, " until {deadline} ns");
                        }
                    }
                    crate::thread::State::WaitingForChild { wanted, .. } => {
                        let _ = write!(&mut into, "waiting for child {wanted}");
                    }
                    crate::thread::State::Transferring(transfer) => {
                        let rings = container.process.kernel.rings.borrow();
                        let _ = write!(
                            &mut into,
                            "{:?} on pipe {} ({} of {} bytes; {} queued, {} readers, {} writers)",
                            transfer.end,
                            transfer.ring,
                            transfer.done,
                            transfer.length,
                            rings.queued(transfer.ring),
                            rings.readers(transfer.ring),
                            rings.writers(transfer.ring),
                        );
                    }
                    crate::thread::State::Watching(watching) => {
                        let _ = write!(&mut into, "in {:?}", watching.watch);
                        if watching.deadline.is_some() {
                            into.push_str(" with a deadline");
                        }
                    }
                    crate::thread::State::Accepting(waiting) => {
                        let sockets = container.process.kernel.sockets.borrow();
                        let _ = write!(
                            &mut into,
                            "in `accept` on listener {} ({} queued)",
                            waiting.listener,
                            sockets.queued(waiting.listener),
                        );
                    }
                    crate::thread::State::Eventing(waiting) => {
                        let _ = write!(
                            &mut into,
                            "on eventfd {} ({}), counter {}",
                            waiting.event,
                            match waiting.writing {
                                true => "writing",
                                false => "reading",
                            },
                            container.process.kernel.events.borrow().count(waiting.event),
                        );
                    }
                    crate::thread::State::Sleeping { deadline } => {
                        let _ = write!(&mut into, "asleep until {deadline} ns");
                    }
                    crate::thread::State::Paused => {
                        into.push_str("in `pause`, waiting for a signal");
                    }
                    crate::thread::State::Exited { status } => {
                        let _ = write!(&mut into, "exited {status}");
                    }
                }
                into.push('\n');
            }
            let open: Vec<String> = container
                .process
                .kernel
                .files
                .open_descriptors()
                .map(|(fd, what)| format!("{fd}:{what}"))
                .collect();
            if !open.is_empty() {
                let _ = writeln!(&mut into, "    open {}", open.join(" "));
            }
        }
        into
    }

    /// Nothing can run. Answers whether anything might, later.
    ///
    /// **This is the only place a clock is read for scheduling**, and it is
    /// read once for the whole container. A deadline is not readiness: a
    /// process parked on a sixty-second `epoll_wait` is not runnable, it is
    /// *waiting*, and treating it as runnable is how a container spends its
    /// whole processor re-checking timeouts while the one process with work
    /// to do gets a fifth of the turns. That is not a slowdown, it is a
    /// hang: measured on the demo stack, nginx proxying to gunicorn timed
    /// out at sixty seconds, repeatedly, while gunicorn's worker had the
    /// request in hand.
    ///
    /// So deadlines are collected here, at the one moment they matter, and
    /// the container's whole processor goes to whoever *can* run.
    ///
    /// **And this is where the wait goes.** A blocking store read belongs at
    /// exactly this point — nothing is runnable and
    /// the earliest deadline is known, so "wait for a host event or that
    /// long" is the one call that turns this spin into a sleep. Until then
    /// it spins, which costs a core and is correct.
    fn idle(&mut self) -> bool {
        // Nothing can run, so this is the other moment the container is
        // already crossing the boundary — and the clock a sleeping container
        // wakes to must be the time it woke, not the time it went to sleep.
        self.current().kernel.refresh_timebase();
        // First, because a pending shutdown is exactly what makes a
        // container that looks stuck not stuck: every process parked on a
        // signal that has not come is a deadlock until somebody sends one.
        self.take_shutdown();
        if self.stopping && self.anything_runnable() {
            return true;
        }
        let deadline = self
            .containers
            .iter()
            .filter(|container| container.status.is_none())
            .filter_map(|container| container.process.earliest_deadline())
            .min();
        // Something outside may still change things. A container with a
        // listener the host answers for is *at rest*, not deadlocked, and
        // this is where the difference is decided.
        let listening = self.containers.iter().any(|container| {
            container.status.is_none() && container.process.kernel.has_edge()
        });
        let Some(deadline) = deadline else {
            if listening {
                // Wait for the host rather than spin: nothing here is runnable
                // and there is no deadline, so
                // "wait for an event or a good long while" *is* a sleep.
                self.current().kernel.pump(Some(PATIENCE));
                self.take_shutdown();
                return true;
            }
            // Nothing is waiting for time and nothing is listening, so
            // nothing will change on its own. Whatever the processes are
            // parked on, no process will ever post it — which is the honest
            // deadlock the stall report is written for.
            return false;
        };
        let Some(now) = self.current().kernel.monotonic() else {
            // A deadline with no clock to expire against. Reporting it as a
            // deadlock is right and the stall report names the threads.
            return false;
        };
        let mut woken = false;
        for index in 0..self.containers.len() {
            if self.containers[index].status.is_some() {
                continue;
            }
            woken |= self.containers[index].process.expire(now);
        }
        if !woken && listening {
            // Nothing has expired yet and the host may still have something.
            // Waiting until the earliest deadline turns the spin into a
            // sleep without ever waiting past a timeout the guest set.
            // Never longer than [`PATIENCE`], and never past the guest's own
            // deadline — whichever comes first.
            let until = deadline.saturating_sub(now) / 1_000_000;
            woken |= self
                .current()
                .kernel
                .pump(Some(until.clamp(1, PATIENCE)));
        }
        // Nothing expired *yet*. The deadline is still ahead, so the
        // container is idle rather than stuck, and going round again is
        // what waiting looks like without something to wait on.
        let _ = deadline;
        woken || true
    }

    /// Does the thing only the system can do, and writes the answer back.
    fn answer(&mut self, request: Request) {
        match request {
            Request::Fork => {
                let answer = match self.fork() {
                    Some(pid) => i64::from(pid),
                    // The address space could not be copied. `ENOMEM`, which
                    // is what Linux says when it cannot make a child.
                    None => crate::errno::Errno::NoMemory.as_result(),
                };
                self.current().answer(answer);
            }
            Request::Wait {
                pid,
                status,
                options,
            } => self.wait(pid, status, options),
            Request::Execute { path, argv, envp } => self.execute(&path, &argv, &envp),
            Request::Kill { pid, signal } => self.signal_process(pid, signal),
        }
    }

    /// `kill` aimed at somebody else.
    ///
    /// The disposition is consulted in the *target's* kernel, because that
    /// is where it lives — a signal's meaning is a property of the process
    /// receiving it, which is why this could not be answered by the caller.
    fn signal_process(&mut self, pid: i32, signal: i32) {
        let answer = match self.deliver(pid, signal) {
            true => 0,
            false => crate::errno::Errno::NoProcess.as_result(),
        };
        self.current().answer(answer);
    }

    /// Sends a signal to a process, and says whether there was one.
    ///
    /// Separate from the row above because **not every signal has a
    /// caller**. A shutdown request from the host becomes a `SIGTERM` at the
    /// first process with no syscall in flight, and writing an answer into
    /// `%rax` there would overwrite a register the running thread was in the
    /// middle of using.
    fn deliver(&mut self, pid: i32, signal: i32) -> bool {
        let Some(index) = self.containers.iter().position(|held| held.pid == pid) else {
            return false;
        };
        // A zombie is still a process id until it is reaped, and a signal
        // sent to one succeeds and does nothing — which is what Linux does
        // and what keeps a `kill` racing a child's exit from becoming an
        // error the caller has to distinguish.
        if self.containers[index].status.is_some() {
            return true;
        }
        // Signal zero asks whether the process exists and sends nothing.
        if signal == 0 {
            return true;
        }
        let fatal = self.containers[index].process.kernel.signal_process(signal);
        if fatal {
            // Nothing caught it and its default action ends the process.
            // What a shell reports for a process killed by a signal is 128
            // plus the number, and a parent's `wait4` reads the same.
            self.finish_at(index, Exit::Signalled {
                signal,
                address: 0,
                rip: 0,
                access: None,
            });
        }
        true
    }

    /// Whether anything anywhere could run now.
    fn anything_runnable(&self) -> bool {
        (0..self.containers.len()).any(|index| self.ready(index))
    }

    /// Turns a host shutdown request into a `SIGTERM` at the first process.
    ///
    /// Once: a request that fired twice would send a second `SIGTERM` to a
    /// tree already shutting down, which is what `docker stop` escalating to
    /// `SIGKILL` means and is not what one Ctrl-C asked for.
    ///
    /// It goes to the *first* process because that is what a container's
    /// init is, and delivering it there is what makes an init script's
    /// `trap` run and its children get told — rather than the host
    /// reaching past the container to signal processes it did not start.
    /// Nothing is forced: a container that ignores it keeps running, exactly
    /// as one under `docker` does until the timeout runs out.
    fn take_shutdown(&mut self) {
        if self.stopping {
            return;
        }
        let mut answer = Vec::new();
        let asked = self
            .current()
            .kernel
            .store
            .read(crate::paths::SHUTDOWN_REQUESTED, &mut answer);
        if asked != crate::abi::StoreOutcome::Present
            || answer.first().is_none_or(|byte| *byte == b'0')
        {
            return;
        }
        self.stopping = true;
        const SIGTERM: i32 = 15;
        self.deliver(FIRST, SIGTERM);
    }

    /// `execve`: the running process, with a different program in it.
    ///
    /// **A failed `execve` returns to the caller.** That is the whole
    /// difficulty, and it is what decides the order here: the old address
    /// space is not torn down until the new program has loaded, because
    /// `execvp` walking `PATH` calls this once per directory and expects
    /// `ENOENT` back from every one but the last. Tearing down first would
    /// make the first miss fatal.
    ///
    /// So both address spaces exist for the length of the load. Only one can
    /// be *at the guest's addresses* — that is the whole reason a dormant
    /// process's bytes live in a file — so the old one steps aside, and
    /// steps back if the load fails.
    fn execute(&mut self, path: &[u8], argv: &[Vec<u8>], envp: &[Vec<u8>]) {
        let argv: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
        let envp: Vec<&[u8]> = envp.iter().map(Vec::as_slice).collect();
        self.current().kernel.deactivate();
        let machine = Interpreted::new();
        let kernel = self.current().kernel.execed(machine);
        let cache = self.current().cache.fresh();
        match Process::enter(kernel, path, &argv, &envp, cache) {
            Ok(process) => {
                // The old address space goes out of scope with the old
                // process, which unmaps nothing — it was already dormant —
                // and closes the file the bytes lived in.
                self.containers[self.current].process = process;
            }
            Err(error) => {
                // The new one is dropped, dormant, having never been at the
                // guest's addresses for longer than the load took.
                self.current().kernel.activate();
                self.current().answer(error.errno().as_result());
            }
        }
    }

    /// `wait4`, which needs the process table and so lands here.
    fn wait(&mut self, wanted: i32, status_at: u64, options: i32) {
        /// `WNOHANG`.
        const NOHANG: i32 = 1;
        let parent = self.current_pid();
        match self.reap(parent, wanted) {
            Reaped::Exited { pid, status } => {
                self.report(status_at, status);
                self.current().answer(i64::from(pid));
            }
            // Children exist and none has finished. Without `WNOHANG` the
            // caller waits — parked, with where to put the answer, so that
            // the child's exit can complete the call rather than the caller
            // having to ask again.
            Reaped::Running if options & NOHANG == 0 => {
                let thread = self.current().kernel.machine.threads.current_mut();
                thread.state = crate::thread::State::WaitingForChild {
                    wanted,
                    status_at,
                };
            }
            Reaped::Running => self.current().answer(0),
            Reaped::None => self
                .current()
                .answer(crate::errno::Errno::NoChild.as_result()),
        }
    }

    /// Writes a child's status where `wait4` was told to put it.
    ///
    /// The encoding is [`Ending`]'s, which is Linux's.
    fn report(&mut self, at: u64, status: Ending) {
        if at == 0 {
            return;
        }
        let _ = self
            .current()
            .kernel
            .pages
            .write(at, &status.wait_status().to_le_bytes());
    }

    /// Records that the running process has ended, and tells whoever was
    /// waiting.
    ///
    /// Answers `Some` when the *container* is over, which is when the first
    /// process ends: a container is a process tree and its status is the
    /// root's, exactly as a `docker run`'s is.
    fn finish(&mut self, exit: Exit) -> Option<Exit> {
        self.finish_at(self.current, exit)
    }

    /// The same for a process that is not the running one — which is what a
    /// `kill` from another process produces.
    ///
    /// Safe to do to a dormant process because none of it touches guest
    /// memory: closing descriptors is the fd table and the shared arenas,
    /// and letting go of the address space is dropping the bytes rather
    /// than reading them.
    fn finish_at(&mut self, index: usize, exit: Exit) -> Option<Exit> {
        let pid = self.containers[index].pid;
        let status = match exit {
            Exit::Status(code) => Ending::Exited(code),
            // Died of something, which the parent hears about as a
            // *signal* rather than as a code — see [`Ending`].
            Exit::Signalled { signal, .. } => Ending::Signalled(signal),
            _ => {
                // An engine or kernel limit, which is not a process ending
                // in any sense the guest would recognise. It ends the
                // container wherever it happens.
                return Some(exit);
            }
        };
        self.containers[index].status = Some(status);
        // Everything it was holding, let go of — descriptors first, then the
        // address space. A zombie is a status and a process id: it exists so
        // a parent can still ask what happened, and nothing more. Keeping
        // its descriptors would keep a pipe's writer count standing, and
        // keeping its address space would keep every byte of it.
        {
            let process = &mut self.containers[index].process;
            // Counted before the state that holds them goes.
            self.departed = process
                .kernel
                .machine
                .threads
                .all()
                .iter()
                .map(|thread| thread.tcb.retired)
                .fold(self.departed, u64::saturating_add);
            self.departed_accelerated = process
                .kernel
                .machine
                .threads
                .all()
                .iter()
                .map(|thread| thread.tcb.accelerated)
                .fold(self.departed_accelerated, u64::saturating_add);
            self.departed_blocks = self
                .departed_blocks
                .saturating_add(process.cache.decoded as u64);
            process.kernel.relinquish();
            process.kernel.deactivate();
            process.kernel.machine.relinquish();
            // Nothing decoded here is worth anything to the new program: the
            // bytes at every address just changed.
            process.cache = process.cache.fresh();
            // Banked above, so zeroed here: a number counted in two places
            // is a number that will be added twice by whoever comes next.
            for thread in process.kernel.machine.threads.all_mut() {
                thread.tcb.retired = 0;
                thread.tcb.accelerated = 0;
            }
        }
        let parent = self.containers[index].parent;
        // Whatever this process was the parent of is now `init`'s, which is
        // the first process — the same rule Linux has, and here it is what
        // keeps an orphan reapable at all: `reap` looks for a container by
        // its parent, and a parent that no longer exists would leave the
        // child a zombie nothing could ever collect.
        for container in &mut self.containers {
            if container.parent == pid {
                container.parent = FIRST;
            }
        }
        // `SIGCHLD`, which is how a program that does not sit in `wait4`
        // finds out. Ignored by default, so a program that never asked hears
        // nothing; a shell that installed a handler runs it.
        if let Some(at) = self.containers.iter().position(|c| c.pid == parent)
            && self.containers[at].status.is_none()
        {
            const SIGCHLD: i32 = 17;
            // Touches control blocks and the disposition table, never guest
            // memory — which is what makes it safe to do to a process whose
            // address space is not currently mapped.
            let _ = self.containers[at].process.kernel.signal_process(SIGCHLD);
        }
        match pid == FIRST {
            true => Some(exit),
            false => None,
        }
    }

}

/// Why the running process stopped, which is what decides whether another
/// one gets a turn.
/// What one [`System::turn`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Turn {
    /// A process ran, or was chosen; call again.
    Ran,
    /// Nothing was runnable and the container waited on the host, or found
    /// a deadline to sleep towards; call again.
    Idle,
    /// The container is finished.
    Finished(Exit),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Yield {
    /// It returned from a syscall and can carry on. Switching here would put
    /// a mapping — or, in the module, a copy — between every `write` and the
    /// next instruction.
    Kept,
    /// Its quantum expired. One of a slice; see [`SLICE`].
    Quantum,
    /// It cannot continue at all: parked, or finished. Whatever is left of
    /// its slice is not something it can use.
    Given,
}

/// How a process ended, which is not one number.
///
/// Linux packs both into one status word and they are not the same field:
/// an exit code lives in bits 8..16 and a terminating signal in the low
/// seven, and `WIFEXITED` versus `WIFSIGNALED` is how a caller tells which
/// one it is looking at. Storing an exit *code* for both — which is what
/// "128 plus the signal" amounts to — makes every killed process look to
/// its parent like one that chose to fail, and a shell cannot tell a
/// segfaulting program from one that returned 139.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ending {
    Exited(i32),
    Signalled(i32),
}

impl Ending {
    /// The status word `wait4` writes, in Linux's encoding.
    pub fn wait_status(self) -> i32 {
        match self {
            Ending::Exited(code) => (code & 0xff) << 8,
            // No core-dump bit: nothing here writes one, and setting it
            // would be a claim about a file that does not exist.
            Ending::Signalled(signal) => signal & 0x7f,
        }
    }

    /// What the *container* reports when its first process ends this way.
    pub fn as_exit(self) -> Exit {
        match self {
            Ending::Exited(code) => Exit::Status(code),
            Ending::Signalled(signal) => Exit::Signalled {
                signal,
                address: 0,
                rip: 0,
                access: None,
            },
        }
    }
}

/// What `wait4` found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reaped {
    /// A child that has exited, now reaped.
    Exited { pid: i32, status: Ending },
    /// Children exist and none has exited.
    Running,
    /// No children at all: `ECHILD`.
    None,
}

impl Interpreted {
    /// A child's machine: a copy of this address space, and only the thread
    /// that called `fork`.
    ///
    /// POSIX says the child has one thread — the caller's — and that is a
    /// table cleanup here rather than the torment it is for a pthread
    /// implementation, because the other threads' control blocks are simply
    /// not copied.
    ///
    /// The child is born *displaced*: it maps every page the parent does,
    /// at the same addresses, and the parent is the one running — so the
    /// child holds a copy of each and gets them back when it first runs.
    /// The one shape that stays a copy of the whole address space, paid
    /// once per fork rather than on every switch; see `crate::resident`.
    pub fn fork(&self, ranges: &[(u64, u64)]) -> Option<Self> {
        let token = crate::resident::new_token();
        crate::resident::fork(self.token, token, ranges);
        Some(Self {
            threads: self.threads.only_current(),
            token,
        })
    }

    /// Makes this process the one whose bytes are at every page it maps.
    ///
    /// The invariant the whole process table rests on — **exactly one
    /// process's bytes are at any page it maps** — is kept per page now
    /// rather than per address space: what comes back here is only what
    /// somebody else took while this process was not running.
    pub fn activate(&mut self, pages: &cpu::space::Space) {
        let _ = pages;
        crate::resident::activate(self.token);
    }

    /// Lets go of this process's address space entirely, which is what
    /// ending does: its copies are dropped and whatever it owned is
    /// garbage to the next claimant.
    pub fn relinquish(&mut self) {
        crate::resident::retire(self.token);
    }

    /// Nothing leaves when a process stops running: its pages stay where
    /// they are until somebody else maps the same address, and are saved
    /// then, by that process's claim.
    pub fn deactivate(&mut self, pages: &cpu::space::Space) {
        let _ = pages;
    }
}
