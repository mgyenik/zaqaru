//! The lockstep oracle: the interpreter and a real process, one instruction
//! at a time.
//!
//! An interpreter can be single-stepped, and so can a native process under
//! `ptrace`. Running the same bytes both ways and comparing the *entire*
//! register file after every instruction — flags included — is the
//! maximal-resolution differential this project can run, and it is
//! structurally impossible for the transpiler, whose observable granularity
//! is a whole run. It changes what a breadth failure costs: instead of
//! "implement, run the corpus, debug a divergence three layers downstream",
//! it is "implement, and the oracle names the first wrong instruction".
//!
//! Native-only, like the x87 crate's hardware oracle, and for the same
//! reason: the ground truth is the machine under the test suite.
//!
//! # How a comparison is set up
//!
//! Matching Linux's process startup byte for byte — the auxiliary vector,
//! `AT_RANDOM`, the environment — would be a second full-fidelity problem
//! standing between us and the first comparison, so the oracle does not try.
//! Instead it synchronises at a point of its own choosing:
//!
//! 1. The corpus program is `exec`ed under `ptrace` and stops before its
//!    first instruction, with its segments mapped and nothing else run.
//! 2. The harness writes the child's registers directly: `%rip` at the
//!    probe, `%rsp` in the program's own static stack, arguments in the
//!    argument registers, and a return address that is the `lockstep_stop`
//!    symbol.
//! 3. Every readable mapping below four gigabytes is mirrored into an arena
//!    at its own addresses, with the same protections, and the child's
//!    register file is copied into the [`Tcb`]. Both machines now hold the
//!    same state by construction rather than by argument.
//! 4. Both single-step until `%rip` reaches `lockstep_stop`.
//!
//! Because the harness chooses `%rsp`, everything the compared region
//! touches is in the program's own image and its static stack — all of it
//! low, all of it inside the address space the engine models.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use object::{Object, ObjectSymbol};
use targum::arena::Arena;
use targum::exec::Trap;
use targum::space::{Protection, Space};
use targum::state::Tcb;
use targum::block::BlockCache;
use targum::{Engine, Outcome};

/// Corpus programs are linked position-dependently at the architecture's
/// default base, so two comparisons running at once would want the same
/// addresses in this process. One at a time, then.
static EXCLUSIVE: Mutex<()> = Mutex::new(());

/// Flags every lockstep corpus program is compiled with.
///
/// The first three switch off features that are explicit non-goals — the
/// control-flow-protection landing pads, the stack protector's TLS access
/// (which would read a `%fs` base no one has set), and unwind tables.
/// `-static -no-pie` is what puts the whole image below four gigabytes at
/// addresses that do not move, which is what makes the arena mirror
/// possible at all; `-nostdlib` keeps libc's startup out of an image nobody
/// initialises.
pub const COMPILE_FLAGS: &[&str] = &[
    "-fcf-protection=none",
    "-fno-stack-protector",
    "-fno-asynchronous-unwind-tables",
    // `sqrt` without this is a call into libm, and there is no libm here.
    "-fno-math-errno",
    "-static",
    "-no-pie",
    "-nostdlib",
    "-nostartfiles",
    "-Wl,--build-id=none",
    "-e",
    "lockstep_stop",
];

/// A scratch directory that removes itself when dropped.
pub struct WorkingDirectory {
    path: PathBuf,
}

impl WorkingDirectory {
    pub fn new(label: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is before the epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("zaqaru-vm-{label}-{unique}"));
        std::fs::create_dir_all(&path).expect("create working directory");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkingDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

pub fn corpus_source(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/corpus")
        .join(name)
}

/// Compiles a corpus program at one optimisation level, restricted to the
/// general-purpose registers.
///
/// `-mgeneral-regs-only` is what keeps an *integer* corpus about integers:
/// without it gcc vectorises a byte loop into `movdqa` and the probe becomes
/// a test of SSE that happens to be written in integer C. Vector
/// instructions get their own corpus, where a failure names the vector
/// instruction rather than the loop it was hiding in.
///
/// The level is not thoroughness for its own sake: `-O0` and `-O2` emit
/// different idioms for the same source — different jump-table shapes,
/// different string-instruction use, different flag consumers — and an
/// engine meant for binaries it did not compile has to handle what it is
/// given.
pub fn compile(workspace: &WorkingDirectory, name: &str, optimisation: &str) -> PathBuf {
    compile_with(workspace, name, optimisation, &["-mgeneral-regs-only"])
}

/// As [`compile`], with extra flags — the vector corpus turns the
/// restriction above back off.
pub fn compile_with(
    workspace: &WorkingDirectory,
    name: &str,
    optimisation: &str,
    extra: &[&str],
) -> PathBuf {
    let source = corpus_source(name);
    let output = workspace
        .path()
        .join(format!("{}-{optimisation}", name.trim_end_matches(".c")));
    let mut command = Command::new("gcc");
    command
        .arg(&source)
        .arg(optimisation)
        .args(COMPILE_FLAGS)
        .args(extra)
        .arg("-o")
        .arg(&output);
    let outcome = command.output().expect("run gcc");
    assert!(
        outcome.status.success(),
        "compiling {name} at {optimisation} failed:\n{}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    output
}

/// The symbols a corpus program exposes, by name.
pub fn symbols(path: &Path) -> BTreeMap<String, u64> {
    let bytes = std::fs::read(path).expect("read the corpus program");
    let file = object::File::parse(&*bytes).expect("parse the corpus program");
    file.symbols()
        .filter_map(|symbol| {
            let name = symbol.name().ok()?;
            (!name.is_empty()).then(|| (name.to_string(), symbol.address()))
        })
        .collect()
}

/// Every `probe_` symbol in a corpus program, in a stable order.
pub fn probes(path: &Path) -> Vec<(String, u64)> {
    symbols(path)
        .into_iter()
        .filter(|(name, _)| name.starts_with("probe_"))
        .collect()
}

// ---- the traced child ----------------------------------------------------

/// A stopped child process the harness drives one instruction at a time.
pub struct Tracee {
    pid: libc::pid_t,
    memory: std::fs::File,
    finished: bool,
}

impl Tracee {
    /// Starts `program` under `ptrace` and returns it stopped before its
    /// first instruction.
    pub fn start(program: &Path) -> Self {
        let path = CString::new(program.as_os_str().to_str().expect("a utf-8 path"))
            .expect("a path without interior nuls");
        // SAFETY: between `fork` and `execv` the child touches only
        // async-signal-safe calls, which is the rule that makes forking a
        // threaded process survivable.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
        if pid == 0 {
            // SAFETY: the child, before `exec`.
            unsafe {
                libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0);
                // The image is position-dependent, so this changes nothing
                // about where it lands; it is here so that anything the
                // kernel *does* place — the heap — lands where it landed
                // last time, and a failure is reproducible.
                libc::personality(libc::ADDR_NO_RANDOMIZE as libc::c_ulong);
                let arguments = [path.as_ptr(), std::ptr::null()];
                libc::execv(path.as_ptr(), arguments.as_ptr());
                libc::_exit(127);
            }
        }
        let status = wait_for(pid);
        assert!(
            libc::WIFSTOPPED(status),
            "the child did not stop at exec (status {status:#x})"
        );
        let memory = std::fs::File::open(format!("/proc/{pid}/mem"))
            .expect("open the child's memory");
        Self {
            pid,
            memory,
            finished: false,
        }
    }

    pub fn registers(&self) -> libc::user_regs_struct {
        let mut registers = unsafe { std::mem::zeroed::<libc::user_regs_struct>() };
        // SAFETY: the child is stopped, and the buffer is the size the
        // request writes.
        let answer = unsafe {
            libc::ptrace(
                libc::PTRACE_GETREGS,
                self.pid,
                0,
                &raw mut registers as *mut libc::c_void,
            )
        };
        assert_ne!(answer, -1, "PTRACE_GETREGS: {}", std::io::Error::last_os_error());
        registers
    }

    pub fn set_registers(&self, registers: &libc::user_regs_struct) {
        // SAFETY: as above.
        let answer = unsafe {
            libc::ptrace(
                libc::PTRACE_SETREGS,
                self.pid,
                0,
                registers as *const _ as *mut libc::c_void,
            )
        };
        assert_ne!(answer, -1, "PTRACE_SETREGS: {}", std::io::Error::last_os_error());
    }

    /// Retires exactly one instruction — which for a repeated string
    /// instruction is one *iteration*, exactly as the interpreter does —
    /// and reports the signal the stop carried.
    ///
    /// `SIGTRAP` is the step completing. Anything else is the instruction
    /// faulting, which is not a harness failure but a *result*: the
    /// interpreter has to fault the same way, and checking that is the only
    /// way the engine's fault semantics are ever held to hardware.
    pub fn step(&self) -> libc::c_int {
        // SAFETY: the child is stopped and traced by this process.
        let answer = unsafe { libc::ptrace(libc::PTRACE_SINGLESTEP, self.pid, 0, 0) };
        assert_ne!(
            answer, -1,
            "PTRACE_SINGLESTEP: {}",
            std::io::Error::last_os_error()
        );
        let status = wait_for(self.pid);
        assert!(
            libc::WIFSTOPPED(status),
            "the child died mid-comparison (status {status:#x})"
        );
        libc::WSTOPSIG(status)
    }

    /// The floating-point register block: the XMM registers and the x87
    /// stack, which are not in the general-purpose one.
    pub fn float_registers(&self) -> libc::user_fpregs_struct {
        let mut registers = unsafe { std::mem::zeroed::<libc::user_fpregs_struct>() };
        // SAFETY: the child is stopped, and the buffer is the size the
        // request writes.
        let answer = unsafe {
            libc::ptrace(
                libc::PTRACE_GETFPREGS,
                self.pid,
                0,
                &raw mut registers as *mut libc::c_void,
            )
        };
        assert_ne!(
            answer, -1,
            "PTRACE_GETFPREGS: {}",
            std::io::Error::last_os_error()
        );
        registers
    }

    pub fn read(&self, address: u64, into: &mut [u8]) -> std::io::Result<()> {
        self.memory.read_exact_at(into, address)
    }

    /// The child's mappings, as `(start, end, protection)`.
    pub fn mappings(&self) -> Vec<(u64, u64, Protection)> {
        let mut text = String::new();
        std::fs::File::open(format!("/proc/{}/maps", self.pid))
            .expect("open the child's maps")
            .read_to_string(&mut text)
            .expect("read the child's maps");
        text.lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let range = fields.next()?;
                let permissions = fields.next()?;
                let (start, end) = range.split_once('-')?;
                let start = u64::from_str_radix(start, 16).ok()?;
                let end = u64::from_str_radix(end, 16).ok()?;
                let bytes = permissions.as_bytes();
                Some((
                    start,
                    end,
                    Protection {
                        read: bytes[0] == b'r',
                        write: bytes[1] == b'w',
                        execute: bytes[2] == b'x',
                    },
                ))
            })
            .collect()
    }
}

impl Drop for Tracee {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // SAFETY: killing a process this harness started and still owns.
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
            let mut status = 0;
            libc::waitpid(self.pid, &raw mut status, 0);
        }
    }
}

fn wait_for(pid: libc::pid_t) -> libc::c_int {
    let mut status = 0;
    // SAFETY: waiting on a child of this process.
    let answer = unsafe { libc::waitpid(pid, &raw mut status, 0) };
    assert_eq!(answer, pid, "waitpid: {}", std::io::Error::last_os_error());
    status
}

// ---- the comparison ------------------------------------------------------

/// The x87 stack, in logical order — `ST(0)` first — with each register's
/// ten significant bytes.
///
/// The `ptrace` block stores them the way `fxsave` does: eight sixteen-byte
/// slots in stack order, of which ten bytes carry the value. The engine
/// holds them in *physical* order with a separate `TOP`, which is what the
/// architecture actually has, so the comparison rotates one onto the other
/// rather than pretending the two layouts are the same.
fn native_x87(registers: &libc::user_fpregs_struct) -> [[u8; 10]; 8] {
    let mut stack = [[0u8; 10]; 8];
    let mut bytes = [0u8; 128];
    for (index, word) in registers.st_space.iter().enumerate() {
        bytes[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    for (logical, register) in stack.iter_mut().enumerate() {
        register.copy_from_slice(&bytes[logical * 16..logical * 16 + 10]);
    }
    stack
}

/// The same, out of the engine.
fn engine_x87(state: &x87::state::X87State) -> [[u8; 10]; 8] {
    let mut image = [0u8; x87::state::IMAGE_SIZE];
    state.save_image(&mut image);
    let top = usize::from(image[4] & 7);
    let mut stack = [[0u8; 10]; 8];
    for (logical, register) in stack.iter_mut().enumerate() {
        let physical = (top + logical) & 7;
        register.copy_from_slice(&image[8 + physical * 10..8 + physical * 10 + 10]);
    }
    stack
}

/// The XMM registers, as the two halves the machine model holds them in.
fn native_vectors(registers: &libc::user_fpregs_struct) -> [[u64; 2]; 16] {
    let mut vectors = [[0u64; 2]; 16];
    for (number, vector) in vectors.iter_mut().enumerate() {
        let words = &registers.xmm_space[number * 4..number * 4 + 4];
        vector[0] = u64::from(words[0]) | (u64::from(words[1]) << 32);
        vector[1] = u64::from(words[2]) | (u64::from(words[3]) << 32);
    }
    vectors
}

/// The general-purpose registers, in the interpreter's encoding order, read
/// out of a `ptrace` register block.
fn native_registers(registers: &libc::user_regs_struct) -> [u64; 16] {
    [
        registers.rax,
        registers.rcx,
        registers.rdx,
        registers.rbx,
        registers.rsp,
        registers.rbp,
        registers.rsi,
        registers.rdi,
        registers.r8,
        registers.r9,
        registers.r10,
        registers.r11,
        registers.r12,
        registers.r13,
        registers.r14,
        registers.r15,
    ]
}

const REGISTER_NAMES: [&str; 16] = [
    "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13",
    "r14", "r15",
];

/// The six status flags: CF, PF, AF, ZF, SF, OF, in RFLAGS positions.
const STATUS_FLAGS: u64 = 0x0000_08d5;

/// The flag bits worth comparing: the six status flags plus the direction
/// flag. The rest of the word is the kernel's business — the interrupt flag
/// a userspace process cannot change, the resume and I/O-privilege bits it
/// cannot see — and is not part of the machine this engine models.
const COMPARED_FLAGS: u64 = STATUS_FLAGS | (1 << 10);

/// One case: a probe, its arguments, and what the two machines did.
pub struct Comparison {
    pub program: PathBuf,
    pub probe: String,
    pub arguments: [u64; 6],
    /// How many instructions were compared before the probe returned.
    pub retired: u64,
    /// Which mnemonics the comparison actually put through both machines.
    ///
    /// A passing comparison is only worth what it covered, and "the corpus
    /// compiled to something other than what it was written to test" is a
    /// failure mode that looks exactly like success — a probe written for
    /// `paddsw` whose compiler folded it away passes every assertion in
    /// this file. So the instrument reports its own reach, and the tests
    /// assert against it.
    pub mnemonics: BTreeSet<&'static str>,
}

/// Runs one probe under both machines, comparing after every instruction.
///
/// Panics on the first disagreement, naming the instruction that caused it,
/// which is the whole point of the instrument.
pub fn lockstep(program: &Path, probe: &str, arguments: [u64; 6], limit: u64) -> Comparison {
    run(program, probe, arguments, limit, false)
}

/// The same comparison with the two machines deliberately pushed out of
/// step: the native side takes one extra instruction that the interpreter
/// does not.
///
/// This exists so the oracle can be shown to *see*. A harness that compared
/// nothing — a mask that swallowed every bit, a loop that never ran — passes
/// every honest test and fails only this one.
pub fn lockstep_desynchronised(
    program: &Path,
    probe: &str,
    arguments: [u64; 6],
    limit: u64,
) -> Comparison {
    run(program, probe, arguments, limit, true)
}

fn run(
    program: &Path,
    probe: &str,
    arguments: [u64; 6],
    limit: u64,
    desynchronise: bool,
) -> Comparison {
    let _exclusive = hold();
    let table = symbols(program);
    let name = probe;
    let entry = *table
        .get(name)
        .unwrap_or_else(|| panic!("{name} is not in {}", program.display()));
    let stop = *table
        .get("lockstep_stop")
        .expect("the corpus program defines `lockstep_stop`");
    let stack = *table
        .get("lockstep_stack")
        .expect("the corpus program defines `lockstep_stack`");

    // Every message below names the case, not just the probe: a divergence
    // that only appears for one argument pair is the common shape, and
    // "probe_arithmetic failed" without the operands sends the reader back
    // to re-derive which of twelve it was.
    let probe = &format!("{probe}({:#x}, {:#x})", arguments[0], arguments[1]);
    let probe = probe.as_str();

    let tracee = Tracee::start(program);

    // Point the child at the probe, on its own static stack, with the stop
    // symbol as the return address. Sixteen-byte alignment *before* the
    // return address is pushed is what the ABI guarantees a callee.
    let mut registers = tracee.registers();
    let top = (stack + 0xf000) & !0xfu64;
    registers.rip = entry;
    registers.rsp = top - 8;
    registers.rdi = arguments[0];
    registers.rsi = arguments[1];
    registers.rdx = arguments[2];
    registers.rcx = arguments[3];
    registers.r8 = arguments[4];
    registers.r9 = arguments[5];
    // A word the interpreter and the child both start from, rather than
    // whatever `execve` happened to leave.
    registers.eflags = 0x202;
    tracee.set_registers(&registers);
    poke(&tracee, registers.rsp, stop);
    let registers = tracee.registers();

    // Mirror the child's low address space, and copy its register file.
    // The harness owns the machine state, because under the VM the kernel
    // does and the harness is standing in for it.
    let (mut machine, _arenas) = mirror(&tracee, &registers);
    let engine = &mut machine;
    let float = tracee.float_registers();
    engine.tcb.vectors = native_vectors(&float);
    // The engine's floating-point state starts where `execve` leaves a
    // process's: the control word at its default, no exceptions, an empty
    // stack. Asserted rather than mirrored, because a mismatch here would
    // mean the kernel no longer resets the unit on exec and the comparison
    // would quietly start from two different machines.
    assert_eq!(
        (float.cwd, float.swd, float.ftw),
        (0x037f, 0, 0),
        "the child's floating-point unit is not in its post-exec state"
    );

    let mut retired = 0u64;
    let mut mnemonics = BTreeSet::new();
    // Flags no instruction has defined since one left them undefined.
    //
    // Masking only at the instruction that produced them is not enough: an
    // undefined flag *stays* in the register, so the divergence surfaces at
    // the next instruction that touches no flags at all. The poison is
    // cleared per flag when something defines it, which — because almost
    // every arithmetic instruction defines all six — happens within a few
    // instructions of any spill.
    let mut poisoned = 0u64;
    loop {
        let native = tracee.registers();
        if native.rip == stop {
            assert_eq!(
                engine.tcb.rip, stop,
                "{probe}: the child returned after {retired} instructions and the \
                 interpreter is at {:#x}",
                engine.tcb.rip
            );
            break;
        }
        assert!(
            retired < limit,
            "{probe}: still running after {limit} instructions"
        );

        // What the instruction about to run leaves undefined is not part of
        // the comparison: the architecture says nothing about those bits and
        // hardware differs between vendors, so pinning them would be pinning
        // this machine rather than the machine.
        let effects = flag_effects(&engine, native.rip);
        poisoned = (poisoned & !effects.defined) | effects.undefined;
        // And what the engine deliberately does not compute. A writer whose
        // six status flags are overwritten before anything reads them skips
        // the lazy record (`Quick::flags_dead`), so after it the record is
        // the previous writer's. That is unobservable to the guest except
        // through the flags word in a signal frame delivered at a quantum
        // boundary, which `docs/fidelity.md` records; here it is a
        // comparison the oracle has to withhold, flag by flag, until a
        // later instruction defines each bit again — exactly the treatment
        // an architecturally undefined bit gets, for the same reason: the
        // engine makes no promise about the value.
        if flags_dead_at(engine, native.rip) {
            poisoned |= STATUS_FLAGS;
        }
        mnemonics.insert(effects.name);

        let signal = tracee.step();
        if desynchronise && retired == 2 {
            tracee.step();
        }
        let outcome = engine.step();
        match (signal, &outcome) {
            (libc::SIGTRAP, Outcome::Preempted) => {}
            (libc::SIGTRAP, Outcome::Syscall) => panic!(
                "{probe}: a syscall at {:#x} — a lockstep probe runs on the machine alone",
                native.rip
            ),
            (libc::SIGTRAP, Outcome::Trap(Trap::Unsupported(unsupported))) => {
                panic!("{probe}: {unsupported}")
            }
            (libc::SIGTRAP, Outcome::Trap(trap)) => panic!(
                "{probe}: `{}` at {:#x} trapped in the interpreter with {trap:?}, \
                 and hardware executed it",
                effects.name, native.rip
            ),
            // The instruction faulted on hardware, so it has to fault here,
            // with the same meaning.
            (signal, Outcome::Trap(trap)) => {
                let expected = expected_signal(trap);
                assert_eq!(
                    expected, signal,
                    "{probe}: `{}` at {:#x} raised signal {signal} on hardware and \
                     {trap:?} here",
                    effects.name, native.rip
                );
                // A fault leaves the program counter on the faulting
                // instruction, which is what makes a handler able to retry
                // it. Hardware says so; so must we.
                let native = tracee.registers();
                assert_eq!(
                    engine.tcb.rip, native.rip,
                    "{probe}: after the fault at {:#x}, %rip is {:#x} and hardware \
                     says {:#x}",
                    native.rip, engine.tcb.rip, native.rip
                );
                return Comparison {
                    program: program.to_path_buf(),
                    probe: name.to_string(),
                    arguments,
                    retired,
                    mnemonics,
                };
            }
            (signal, outcome) => panic!(
                "{probe}: `{}` at {:#x} raised signal {signal} on hardware and the \
                 interpreter answered {outcome:?}",
                effects.name, native.rip
            ),
        }
        retired += 1;
        // Re-asserted after every step for the same reason it is set at the
        // start: the kernel forces it back on before each step, whatever the
        // guest's own `popf` had to say about it.
        engine.tcb.flags.set_trap(true);

        let after = tracee.registers();
        compare(
            probe,
            native.rip,
            effects.name,
            retired,
            &after,
            &engine.tcb,
            poisoned,
        );
        // The floating-point stack and the words that describe it.
        let float = tracee.float_registers();
        let expected = native_x87(&float);
        let observed = engine_x87(&engine.tcb.x87);
        for (number, (expected, observed)) in expected.iter().zip(observed.iter()).enumerate() {
            assert_eq!(
                observed,
                expected,
                "{probe}: after `{}` at {:#x} (instruction {retired}), %st({number}) is \
                 {} and hardware says {}",
                effects.name,
                native.rip,
                hex(observed),
                hex(expected)
            );
        }
        assert_eq!(
            engine.tcb.x87.control(),
            float.cwd,
            "{probe}: after `{}` at {:#x} (instruction {retired}), the x87 control word \
             is {:#06x} and hardware says {:#06x}",
            effects.name,
            native.rip,
            engine.tcb.x87.control(),
            float.cwd
        );
        // Bit fifteen is the busy flag, which is a property of a real
        // unit's pipeline and not of the machine this models.
        let status = engine.tcb.x87.status_word();
        assert_eq!(
            status,
            float.swd & !0x8000,
            "{probe}: after `{}` at {:#x} (instruction {retired}), the x87 status word \
             is {status:#06x} and hardware says {:#06x}",
            effects.name,
            native.rip,
            float.swd & !0x8000
        );

        // The vector file, whenever the instruction could have touched it.
        // Read unconditionally rather than guessed at: `PTRACE_GETFPREGS`
        // is one call, and a rule about which instructions are "vector
        // instructions" is a rule that can be wrong.
        let vectors = native_vectors(&tracee.float_registers());
        for (number, (expected, observed)) in
            vectors.iter().zip(engine.tcb.vectors.iter()).enumerate()
        {
            assert_eq!(
                observed,
                expected,
                "{probe}: after `{}` at {:#x} (instruction {retired}), %xmm{number} is \
                 {:032x} and hardware says {:032x}",
                effects.name,
                native.rip,
                u128::from(observed[0]) | (u128::from(observed[1]) << 64),
                u128::from(expected[0]) | (u128::from(expected[1]) << 64),
            );
        }
    }

    Comparison {
        program: program.to_path_buf(),
        probe: name.to_string(),
        arguments,
        retired,
        mnemonics,
    }
}

fn hex(bytes: &[u8; 10]) -> String {
    bytes
        .iter()
        .rev()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Fails naming every instruction the corpus was written to exercise and
/// did not.
pub fn require_coverage(label: &str, seen: &BTreeSet<&'static str>, required: &[&str]) {
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|name| !seen.contains(name))
        .collect();
    assert!(
        missing.is_empty(),
        "{label}: the corpus never reached {missing:?} — {} mnemonics were covered: {:?}",
        seen.len(),
        seen
    );
}

#[allow(clippy::too_many_arguments)]
/// The signal Linux delivers for each kind of trap.
fn expected_signal(trap: &Trap) -> libc::c_int {
    match trap {
        // A general-protection fault reaches userspace as `SIGSEGV`, which
        // is why the privileged case and the access case answer the same.
        Trap::Fault(_) | Trap::Privileged { .. } | Trap::Misaligned { .. } => libc::SIGSEGV,
        Trap::Undefined { .. } => libc::SIGILL,
        Trap::Breakpoint { .. } => libc::SIGTRAP,
        Trap::DivideError { .. } => libc::SIGFPE,
        Trap::Unsupported(_) => 0,
    }
}

#[allow(clippy::too_many_arguments)]
fn compare(
    probe: &str,
    address: u64,
    name: &'static str,
    retired: u64,
    native: &libc::user_regs_struct,
    tcb: &Tcb,
    undefined: u64,
) {
    let expected = native_registers(native);
    for (index, (expected, observed)) in expected.iter().zip(tcb.registers.iter()).enumerate() {
        assert_eq!(
            observed,
            expected,
            "{probe}: after `{name}` at {address:#x} (instruction {retired}), \
             %{} is {observed:#x} and hardware says {expected:#x}",
            REGISTER_NAMES[index]
        );
    }
    assert_eq!(
        tcb.rip, native.rip,
        "{probe}: after `{name}` at {address:#x} (instruction {retired}), \
         %rip is {:#x} and hardware says {:#x}",
        tcb.rip, native.rip
    );
    let mask = COMPARED_FLAGS & !undefined;
    let ours = tcb.flags.materialized() & mask;
    let theirs = native.eflags & mask;
    assert_eq!(
        ours,
        theirs,
        "{probe}: after `{name}` at {address:#x} (instruction {retired}), \
         the flags are {ours:#x} and hardware says {theirs:#x} \
         (compared bits {mask:#x}, differing {})",
        flag_names(ours ^ theirs)
    );
}

/// What an instruction does to the flags, and what it is called.
struct Effects {
    /// Bits it leaves with a value the architecture specifies.
    defined: u64,
    /// Bits it leaves with a value the architecture does not specify.
    undefined: u64,
    name: &'static str,
}

/// Reads the flag effects of the instruction at `address` out of the
/// decoder's own tables, so the oracle's idea of what is defined comes from
/// the same reference the engine is written against rather than from a
/// second hand-maintained list.
fn flag_effects(engine: &Machine, address: u64) -> Effects {
    use iced_x86::{Decoder, DecoderOptions};
    let Ok(bytes) = engine.space.fetch(address, 16) else {
        return Effects {
            defined: 0,
            undefined: 0,
            name: "?",
        };
    };
    let mut decoder = Decoder::with_ip(64, bytes, address, DecoderOptions::NONE);
    let instruction = decoder.decode();
    Effects {
        defined: rflags(
            instruction.rflags_written() | instruction.rflags_cleared() | instruction.rflags_set(),
        ),
        undefined: rflags(instruction.rflags_undefined()),
        name: mnemonic_name(instruction.mnemonic()),
    }
}

/// Whether the engine will skip the lazy-flags record for the instruction
/// at `address`, as the block it decodes there decides.
///
/// Under a quantum of one the engine runs exactly the first instruction of
/// the block it fetches at `rip`, so that block's first lowered op is the
/// decision that governs this step. Fetching it here first is harmless: the
/// engine finds it in the cache, or re-decodes it after an invalidation the
/// same way it would have anyway.
fn flags_dead_at(engine: &mut Machine, address: u64) -> bool {
    match engine.cache.entry(address, &mut engine.space) {
        Ok(index) => engine
            .cache
            .block(index)
            .quick
            .first()
            .is_some_and(|quick| quick.flags_dead),
        Err(_) => false,
    }
}

/// iced's flag bits are its own; these are the architecture's positions.
fn rflags(bits: u32) -> u64 {
    use iced_x86::RflagsBits;
    let mut mask = 0u64;
    for (bit, position) in [
        (RflagsBits::CF, 0),
        (RflagsBits::PF, 2),
        (RflagsBits::AF, 4),
        (RflagsBits::ZF, 6),
        (RflagsBits::SF, 7),
        (RflagsBits::DF, 10),
        (RflagsBits::OF, 11),
    ] {
        if bits & bit != 0 {
            mask |= 1 << position;
        }
    }
    mask
}

fn flag_names(mask: u64) -> String {
    let mut names = Vec::new();
    for (position, name) in [
        (0, "CF"),
        (2, "PF"),
        (4, "AF"),
        (6, "ZF"),
        (7, "SF"),
        (10, "DF"),
        (11, "OF"),
    ] {
        if mask & (1u64 << position) != 0 {
            names.push(name);
        }
    }
    match names.is_empty() {
        true => "none".to_string(),
        false => names.join("|"),
    }
}

/// A mnemonic's name without a formatter in the build.
///
/// `Debug` is derived on the enum and always available, so a loud message
/// can say `Imul` without the crate carrying an assembly formatter into
/// every container.
fn mnemonic_name(mnemonic: iced_x86::Mnemonic) -> &'static str {
    // Leaked once per distinct mnemonic in a test process, which is bounded
    // by the instruction set and costs nothing that matters here.
    static NAMES: Mutex<Option<BTreeMap<iced_x86::Mnemonic, &'static str>>> = Mutex::new(None);
    let mut names = NAMES.lock().unwrap_or_else(|poison| poison.into_inner());
    let names = names.get_or_insert_with(BTreeMap::new);
    names
        .entry(mnemonic)
        .or_insert_with(|| Box::leak(format!("{mnemonic:?}").into_boxed_str()))
}

/// Copies every readable mapping below four gigabytes into an arena at its
/// own addresses, and the child's register file into a fresh [`Tcb`].
/// The state a comparison runs over: everything the kernel would own.
pub struct Machine {
    pub tcb: Tcb,
    pub space: Space,
    pub cache: BlockCache,
}

impl Machine {
    fn step(&mut self) -> Outcome {
        Engine::run(&mut self.tcb, &mut self.space, &mut self.cache, 1)
    }
}

fn mirror(tracee: &Tracee, registers: &libc::user_regs_struct) -> (Machine, Vec<Arena>) {
    /// The wasm32 ceiling. A static, position-dependent image lives far
    /// below it; the process stack and the vDSO do not, and the probe never
    /// touches them because the harness moved `%rsp`.
    const CEILING: u64 = 1 << 32;

    let mappings: Vec<_> = tracee
        .mappings()
        .into_iter()
        .filter(|(start, end, protection)| *end <= CEILING && protection.read && *start != 0)
        .collect();
    assert!(
        !mappings.is_empty(),
        "the child has no mappings below four gigabytes"
    );
    let limit = mappings.iter().map(|(_, end, _)| *end).max().unwrap();

    let mut space = Space::new(limit);
    let mut arenas = Vec::new();
    let mut buffer = Vec::new();
    for (start, end, protection) in mappings {
        arenas.push(Arena::at(start, end - start));
        buffer.clear();
        buffer.resize((end - start) as usize, 0);
        tracee
            .read(start, &mut buffer)
            .unwrap_or_else(|error| panic!("reading {start:#x}..{end:#x}: {error}"));
        // Everything is mapped writable first so the copy can land, then
        // given the protection the child has. A read-only page the guest
        // must not write is read-only for the guest, not for the harness.
        space.protect(start, end - start, Protection::ALL);
        space.write(start, &buffer).expect("mirror a mapping");
        space.protect(start, end - start, protection);
    }

    let mut engine = Machine {
        tcb: Tcb::new(),
        space,
        cache: BlockCache::new(),
    };
    // The mirror is the harness placing bytes, not the guest storing them:
    // nothing may be queued for invalidation before the first fetch.
    engine.cache.drain_invalidations(&mut engine.space);
    engine.tcb.registers = native_registers(registers);
    engine.tcb.rip = registers.rip;
    engine.tcb.fs_base = registers.fs_base;
    engine.tcb.flags.load(registers.eflags);
    // The machine on the other side is being single-stepped, which means the
    // kernel has forced its trap flag on. `ptrace` hides that from the
    // register block — but not from the guest, whose `pushf` sees the real
    // bit. So the interpreter carries it too, and the comparison is between
    // two single-stepped machines rather than between one of each.
    engine.tcb.flags.set_trap(true);
    (engine, arenas)
}

/// Writes one word into the child, for the return address the harness
/// pushes.
fn poke(tracee: &Tracee, address: u64, value: u64) {
    // SAFETY: the child is stopped and traced by this process.
    let answer = unsafe {
        libc::ptrace(
            libc::PTRACE_POKEDATA,
            tracee.pid,
            address as *mut libc::c_void,
            value as *mut libc::c_void,
        )
    };
    assert_ne!(
        answer, -1,
        "PTRACE_POKEDATA at {address:#x}: {}",
        std::io::Error::last_os_error()
    );
}

/// Serialises comparisons, because they all want the same addresses.
fn hold() -> MutexGuard<'static, ()> {
    EXCLUSIVE.lock().unwrap_or_else(|poison| poison.into_inner())
}
