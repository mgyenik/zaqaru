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
use targum::{Engine, Outcome as Step, QUANTUM};

use crate::abi::Store;
use crate::machine::Interpreted;
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
    /// A syscall blocked and there is no second thread to run.
    Deadlocked,
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
        &mut self.kernel.machine.thread
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
        let entry = kernel.exec(path, argv, envp)?;
        // `exec` already wrote the stack pointer and reset the unit through
        // the machine, which in this world *is* the control block — so the
        // only thing left is where to start.
        kernel.machine.thread.rip = entry;
        Ok(Self {
            kernel,
            cache: BlockCache::new(),
        })
    }

    /// Runs until the process finishes.
    pub fn run(&mut self) -> Exit {
        loop {
            match self.advance(QUANTUM) {
                Some(exit) => return exit,
                None => continue,
            }
        }
    }

    /// One turn of the loop: run a quantum, then decide what stopped it.
    ///
    /// Separate from [`Process::run`] so that a test can take one turn at a
    /// time, and so that the thing a scheduler will call is already a
    /// function rather than the middle of a loop body.
    pub fn advance(&mut self, quantum: u64) -> Option<Exit> {
        // Three disjoint fields of one owner, which is what makes the
        // engine able to hold no state of its own.
        let outcome = Engine::run(
            &mut self.kernel.machine.thread,
            &mut self.kernel.pages,
            &mut self.cache,
            quantum,
        );
        match outcome {
            // The thread is still runnable. With one thread there is nobody
            // else to pick, so this is where a scheduler would choose and
            // this loop simply continues.
            Step::Preempted => None,
            Step::Syscall => self.serve(),
            Step::Trap(trap) => Some(self.fault(trap)),
        }
    }

    /// Serves one syscall.
    fn serve(&mut self) -> Option<Exit> {
        // Whatever the host placed in guest memory for the previous call is
        // dead now. The arena's lifetime is one call, and that is what stops
        // the boundary leaking — the ahead-of-time seam does this at the top
        // of its dispatch and so must this one. Without it a container runs
        // perfectly well until the arena fills, which for a Python process
        // is about thirty-eight seconds in and looks like the host refusing
        // a forty-four byte read.
        crate::abi::reset_transfer_arena();
        let thread = &self.kernel.machine.thread;
        let number = thread.registers[NUMBER] as i64;
        let raw = ARGUMENTS.map(|index| thread.registers[index] as i64);
        let arguments = Arguments::new(raw);
        let answer = self.kernel.dispatch(number, arguments);
        self.record(number, &raw, &answer);
        match answer {
            Outcome::Done(value) => {
                self.kernel.machine.thread.registers[NUMBER] = value as u64;
                None
            }
            Outcome::Exit(status) => Some(Exit::Status(status)),
            Outcome::Fault(fault) => Some(Exit::Unimplemented(fault)),
            // Reachable only once there is more than one thread: with one,
            // a wait that cannot be satisfied has nothing to wait for.
            Outcome::Blocked => Some(Exit::Deadlocked),
        }
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
            Outcome::Exit(status) => format!("<exit {status}>"),
        };
        let line = crate::traced(&mut self.kernel, number, arguments, &rendered);
        crate::trace(&mut self.kernel, &line);
    }

    /// Turns a trap into what the guest sees.
    ///
    /// Signal *delivery* is not built yet, so an unhandled fault ends the
    /// process the way an unhandled fault ends one on Linux — which is
    /// already more than the ahead-of-time path can do, where a null
    /// dereference reads whatever happens to be at address zero and carries
    /// on. What is missing is the handler path, not the fault.
    fn fault(&mut self, trap: Trap) -> Exit {
        let rip = self.kernel.machine.thread.rip;
        /// The numbers a shell reports and a `siginfo` carries.
        const SIGILL: i32 = 4;
        const SIGTRAP: i32 = 5;
        const SIGFPE: i32 = 8;
        const SIGSEGV: i32 = 11;
        match trap {
            Trap::Fault(fault) => Exit::Signalled {
                signal: SIGSEGV,
                address: fault.address,
                rip,
                access: Some(fault.access),
            },
            Trap::Privileged { address } | Trap::Misaligned { address } => Exit::Signalled {
                signal: SIGSEGV,
                address,
                rip,
                access: None,
            },
            Trap::Undefined { address } => Exit::Signalled {
                signal: SIGILL,
                address,
                rip,
                access: None,
            },
            Trap::DivideError { address } => Exit::Signalled {
                signal: SIGFPE,
                address,
                rip,
                access: None,
            },
            Trap::Breakpoint { address } => Exit::Signalled {
                signal: SIGTRAP,
                address,
                rip,
                access: None,
            },
            Trap::Unsupported(unsupported) => Exit::Unsupported(unsupported),
        }
    }
}
