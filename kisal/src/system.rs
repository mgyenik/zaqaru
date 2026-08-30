//! More than one process: the table, the switch, and what a fork is.
//!
//! A process is an address space, a kernel, and the threads running in it.
//! Two live processes cannot share one address space — a guest address is a
//! linear-memory offset and the child needs the parent's addresses — which
//! is the same structural fact `container-plan.md` records for the
//! ahead-of-time world, and it is why a process is an instance there and an
//! address space here.
//!
//! **What the interpreter deletes is the expensive half.** A fork on the
//! other path is a snapshot *plus a way back into the frames it was taken
//! from*: resume IDs threaded through the guest stack, a resume body for
//! every function, a driver that walks the chain re-entering each frame at
//! its post-call block — the machinery `tests/fork_resume.rs` exists to
//! prove, and the doubled code section that is its bill. Here the child's
//! machine state is a control block and its address space is bytes. The
//! child resumes by *being interpreted*, which is the only thing the loop
//! ever does.
//!
//! So a fork is three steps and none of them is subtle:
//!
//! 1. duplicate the address space — one kernel-side copy of a file;
//! 2. copy the kernel's own state, field by field, which is
//!    [`crate::syscall::Kernel::fork`];
//! 3. in the child, `%rax = 0`. `%rip` is already past the `syscall`.

use targum::block::BlockCache;

use crate::abi::Store;
use crate::machine::Interpreted;
use crate::run::{Exit, Process, Progress};
use crate::syscall::Request;

/// The first process's identifier.
pub const FIRST: i32 = 1;

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
    /// Set when it has exited and nothing has reaped it — a zombie, which
    /// exists so that a parent can still ask what happened.
    pub status: Option<i32>,
}

/// Every process, and which one is running.
pub struct System<'a, S: Store> {
    containers: Vec<Container<'a, S>>,
    current: usize,
    next: i32,
    /// How many thread quanta the running process has used since it was
    /// given the processor. See [`SLICE`].
    slice: u64,
    /// What processes that are gone retired before they went.
    ///
    /// Kept here because reaping destroys the only other place the number
    /// lived, and a container that forks does most of its work in children:
    /// a rate computed from the survivors would understate the engine by
    /// however much the guest chose to fan out.
    departed: u64,
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
            departed: 0,
        }
    }

    pub fn current(&mut self) -> &mut Process<'a, S> {
        &mut self.containers[self.current].process
    }

    pub fn current_pid(&self) -> i32 {
        self.containers[self.current].pid
    }

    /// Instructions retired by every process, living or reaped.
    pub fn retired(&self) -> u64 {
        self.containers
            .iter()
            .flat_map(|container| container.process.kernel.machine.threads.all())
            .map(|thread| thread.tcb.retired)
            .fold(self.departed, u64::saturating_add)
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
            let machine = process.kernel.machine.fork(&process.kernel.pages)?;
            (process.kernel.fork(machine), BlockCache::new())
        };
        let pid = self.next;
        self.next += 1;
        let mut child = Process { kernel, cache };
        // `fork` returns zero in the child. `%rip` is already past the
        // `syscall`, which is the whole of what the other path needed a
        // resume driver for.
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
    fn schedule(&mut self, preempted: bool) -> bool {
        // A quantum expiring is a *thread* scheduling point. It becomes a
        // process scheduling point once the process has had a whole slice of
        // them — see [`SLICE`].
        self.slice += u64::from(preempted);
        let rotate = self.slice >= SLICE;
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
                let gone = self.containers.remove(index);
                self.departed = gone
                    .process
                    .kernel
                    .machine
                    .threads
                    .all()
                    .iter()
                    .map(|thread| thread.tcb.retired)
                    .fold(self.departed, u64::saturating_add);
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
    pub fn run(&mut self) -> Exit {
        loop {
            let rotate = match self.current().step(targum::QUANTUM) {
                Progress::Running => false,
                // The quantum expired, so every process gets a turn — the
                // same rule the threads inside one already follow, and with
                // the same consequence: the whole interleaving, across
                // processes as well as threads, is a function of how many
                // instructions have retired.
                Progress::Preempted => true,
                Progress::Requested(request) => {
                    self.answer(request);
                    false
                }
                Progress::Finished(exit) => {
                    if let Some(ending) = self.finish(exit) {
                        return ending;
                    }
                    true
                }
            };
            if !self.schedule(rotate) {
                // Nothing runnable anywhere. Either everything has ended, or
                // everything is waiting for something that will not come.
                return match self.containers.first().and_then(|first| first.status) {
                    Some(status) => Exit::Status(status),
                    None => Exit::Deadlocked,
                };
            }
        }
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
        }
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
        match Process::enter(kernel, path, &argv, &envp) {
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
    /// The encoding is Linux's: a status byte in bits 8..16 for a process
    /// that exited, which is what `WEXITSTATUS` shifts back out.
    fn report(&mut self, at: u64, status: i32) {
        if at == 0 {
            return;
        }
        let encoded = ((status & 0xff) as u32) << 8;
        let _ = self
            .current()
            .kernel
            .pages
            .write(at, &encoded.to_le_bytes());
    }

    /// Records that the running process has ended, and tells whoever was
    /// waiting.
    ///
    /// Answers `Some` when the *container* is over, which is when the first
    /// process ends: a container is a process tree and its status is the
    /// root's, exactly as a `docker run`'s is.
    fn finish(&mut self, exit: Exit) -> Option<Exit> {
        let index = self.current;
        let pid = self.containers[index].pid;
        let status = match exit {
            Exit::Status(status) => status,
            // Died of something. The parent still hears about it, and the
            // encoding is what a shell reports.
            Exit::Signalled { signal, .. } => 128 + signal,
            _ => {
                // An engine or kernel limit, which is not a process ending
                // in any sense the guest would recognise. It ends the
                // container wherever it happens.
                return Some(exit);
            }
        };
        self.containers[index].status = Some(status);
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
            true => Some(Exit::Status(status)),
            false => None,
        }
    }

}

/// What `wait4` found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reaped {
    /// A child that has exited, now reaped.
    Exited { pid: i32, status: i32 },
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
    /// The child is born *dormant*: the parent returns from `fork` first, so
    /// the parent is the one that stays at the guest's addresses.
    pub fn fork(&self, pages: &targum::space::Space) -> Option<Self> {
        let _ = pages;
        Some(Self {
            threads: self.threads.only_current(),
            // One kernel-side copy of a file, and no bytes through this
            // process at all.
            #[cfg(not(target_arch = "wasm32"))]
            memory: self.memory.duplicate()?,
            // The bytes the parent has right now, which is what `fork`
            // means.
            #[cfg(target_arch = "wasm32")]
            dormant: Some(crate::machine::Dormant::taken(pages)),
        })
    }

    /// Puts this process's address space at the guest's addresses.
    ///
    /// Natively one `MAP_FIXED` mapping of the file the bytes live in;
    /// inside the module a copy of the pages the table describes. Same
    /// invariant either way, and it is the one the whole process table rests
    /// on: **exactly one address space is at the guest's addresses**, so
    /// touching a process's memory is only ever done to the current one.
    pub fn activate(&mut self, pages: &targum::space::Space) {
        let _ = pages;
        #[cfg(not(target_arch = "wasm32"))]
        self.memory.activate();
        #[cfg(target_arch = "wasm32")]
        if let Some(held) = self.dormant.take() {
            held.restore();
        }
    }

    pub fn deactivate(&mut self, pages: &targum::space::Space) {
        let _ = pages;
        #[cfg(not(target_arch = "wasm32"))]
        self.memory.deactivate();
        #[cfg(target_arch = "wasm32")]
        if self.dormant.is_none() {
            self.dormant = Some(crate::machine::Dormant::taken(pages));
        }
    }
}
