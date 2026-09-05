//! Signal delivery: the frame Linux builds and the frame this builds.
//!
//! A handler is a program counter. The loop stops between blocks, the frame
//! is a block of bytes written to the guest stack through the same address
//! space every other store goes through, `%rip` is set to the handler, and
//! interpretation continues. `sigreturn` reads the block back. The rest of
//! this module is the *dispositions table* and the routing rules — Linux
//! semantics rather than machinery.
//!
//! # The frame
//!
//! Byte for byte Linux's `struct rt_sigframe`, because the guest reads it:
//! a handler declared `SA_SIGINFO` is handed a `siginfo_t` and a
//! `ucontext_t` and may read either, `sigreturn` is glibc's own code
//! reading back what the kernel wrote, and `siglongjmp` out of a handler
//! walks it. A layout that is nearly right is a layout that works until
//! something looks.
//!
//! ```text
//!   0   pretcode   the restorer's address, which the handler returns to
//!   8   ucontext   flags, link, altstack, mcontext, mask
//! 296   siginfo    128 bytes: number, errno, code, and the union
//! ```

use cpu::state::Tcb;

/// Where the machine's registers sit inside the frame's `ucontext`.
///
/// The order is `struct sigcontext_64`'s, which is not the encoding order
/// and not alphabetical — it is the order the kernel's structure declares,
/// and glibc's `mcontext_t.gregs` is indexed by the same sequence. A handler
/// that reads `gregs[REG_RIP]` is reading offset 168 of the ucontext, and it
/// is reading it because this put it there.
const CONTEXT: [(usize, usize); 16] = [
    // (register number in the interpreter's file, offset in sigcontext)
    (8, 0),    // r8
    (9, 8),    // r9
    (10, 16),  // r10
    (11, 24),  // r11
    (12, 32),  // r12
    (13, 40),  // r13
    (14, 48),  // r14
    (15, 56),  // r15
    (7, 64),   // rdi
    (6, 72),   // rsi
    (5, 80),   // rbp
    (3, 88),   // rbx
    (2, 96),   // rdx
    (0, 104),  // rax
    (1, 112),  // rcx
    (4, 120),  // rsp
];

/// `%rip` and the flags word, which are not general-purpose registers and so
/// are not in the table above.
const RIP: usize = 128;
const EFLAGS: usize = 136;

/// The alternate-stack description, inside the `ucontext`. `uc_flags` and
/// `uc_link` come before it.
const ALTSTACK: usize = 16;
/// Where the `sigcontext` begins inside the `ucontext` — which is also
/// where `mcontext_t.gregs` begins, so a handler's `gregs[REG_RIP]` is this
/// plus `RIP`.
const MCONTEXT: usize = 40;
/// How long a `sigcontext` is.
const SIGCONTEXT: usize = 256;
/// The *kernel's* `sigset_t`, which is one word — not glibc's, which is
/// sixteen. The frame is the kernel's structure, and a handler that reads
/// past the word is reading past what the kernel wrote on Linux too.
const SIGSET: usize = 8;
/// Where the blocked mask the handler must restore sits.
const SIGMASK: usize = MCONTEXT + SIGCONTEXT;
/// The whole `ucontext`.
const UCONTEXT_SIZE: usize = SIGMASK + SIGSET;

/// Where the `ucontext` begins inside the frame, after the return address.
pub const UCONTEXT: usize = 8;
/// Where the `siginfo` begins.
pub const SIGINFO: usize = UCONTEXT + UCONTEXT_SIZE;
/// The whole frame.
pub const FRAME: usize = SIGINFO + 128;

// Measured against the host's own headers, 2026-08-30: `uc_mcontext` at 40,
// `uc_sigmask` at 296, `REG_RIP` = 16 and `REG_RSP` = 15 — which put the
// program counter at 168 and the stack pointer at 160 from the `ucontext`,
// and those are the offsets the test below pins. An earlier version had the
// `siginfo` eight bytes low, overlapping the mask, and the symptom was a
// handler restoring the signal number as its blocked set.
const _: () = {
    assert!(SIGMASK + SIGSET <= UCONTEXT_SIZE);
    assert!(UCONTEXT + SIGMASK + SIGSET <= SIGINFO);
};

/// What a signal is delivered *about*, beyond its number.
///
/// A fault carries the address that caused it, which is the whole reason a
/// handler for `SIGSEGV` is worth installing: `si_addr` is how a guard-page
/// handler knows which page to make writable.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Cause {
    /// `si_code`, which says *why* — a mapping error, a permission error, a
    /// division by zero.
    pub code: i32,
    /// `si_addr`, for the faults that have one.
    pub address: u64,
}

/// Builds the frame a handler runs on top of.
///
/// Answers the bytes and where they go; the caller writes them, because
/// writing to guest memory is the address space's job and not this module's.
pub fn frame(thread: &Tcb, signal: i32, cause: Cause, restorer: u64, mask: u64) -> [u8; FRAME] {
    let mut bytes = [0u8; FRAME];
    bytes[0..8].copy_from_slice(&restorer.to_le_bytes());

    let context = UCONTEXT + MCONTEXT;
    for (register, offset) in CONTEXT {
        let at = context + offset;
        bytes[at..at + 8].copy_from_slice(&thread.registers[register].to_le_bytes());
    }
    let at = context + RIP;
    bytes[at..at + 8].copy_from_slice(&thread.rip.to_le_bytes());
    let at = context + EFLAGS;
    bytes[at..at + 8].copy_from_slice(&thread.flags.materialized().to_le_bytes());
    // The mask in force *before* the handler, which `sigreturn` restores.
    let at = UCONTEXT + SIGMASK;
    bytes[at..at + 8].copy_from_slice(&mask.to_le_bytes());

    // `siginfo`: the number, an errno of zero, the code, and the address.
    let info = SIGINFO;
    bytes[info..info + 4].copy_from_slice(&signal.to_le_bytes());
    bytes[info + 8..info + 12].copy_from_slice(&cause.code.to_le_bytes());
    // `si_addr` is the first member of the union, at offset sixteen for the
    // fault layouts — which are the only ones this kernel raises.
    bytes[info + 16..info + 24].copy_from_slice(&cause.address.to_le_bytes());
    bytes
}

/// Restores a thread from a frame, and answers the mask to put back.
///
/// Every general-purpose register, the program counter and the flags come
/// straight out of the block — which is what makes `siglongjmp` out of a
/// handler work without anything here knowing what `siglongjmp` is.
pub fn restore(thread: &mut Tcb, bytes: &[u8; FRAME]) -> u64 {
    let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"));
    let context = UCONTEXT + MCONTEXT;
    for (register, offset) in CONTEXT {
        thread.registers[register] = word(context + offset);
    }
    thread.rip = word(context + RIP);
    thread.flags.load(word(context + EFLAGS));
    word(UCONTEXT + SIGMASK)
}

/// Writes the alternate stack a handler was given into the frame, so that a
/// nested `sigaltstack(NULL, &old)` inside the handler answers truthfully.
pub fn record_altstack(bytes: &mut [u8; FRAME], stack: Altstack) {
    let at = UCONTEXT + ALTSTACK;
    bytes[at..at + 8].copy_from_slice(&stack.base.to_le_bytes());
    bytes[at + 8..at + 12].copy_from_slice(&stack.flags.to_le_bytes());
    bytes[at + 16..at + 24].copy_from_slice(&stack.size.to_le_bytes());
}

/// `stack_t`: where a handler runs when the signal asks for its own stack.
///
/// The whole reason `sigaltstack` exists is the case where the ordinary
/// stack cannot be used — a stack overflow, whose `SIGSEGV` would fault
/// again the moment a frame was pushed. That case only became possible here
/// when the address space gained guard pages.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Altstack {
    pub base: u64,
    pub size: u64,
    pub flags: i32,
}

impl Altstack {
    /// `SS_DISABLE`.
    pub const DISABLE: i32 = 2;
    /// `SS_ONSTACK`, which a handler running on it must see.
    pub const ON: i32 = 1;

    pub fn is_enabled(&self) -> bool {
        self.size != 0 && self.flags & Self::DISABLE == 0
    }

    pub fn to_bytes(self) -> [u8; 24] {
        let mut bytes = [0u8; 24];
        bytes[0..8].copy_from_slice(&self.base.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.flags.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.size.to_le_bytes());
        bytes
    }

    pub fn from_bytes(bytes: &[u8; 24]) -> Self {
        Self {
            base: u64::from_le_bytes(bytes[0..8].try_into().expect("eight bytes")),
            flags: i32::from_le_bytes(bytes[8..12].try_into().expect("four bytes")),
            size: u64::from_le_bytes(bytes[16..24].try_into().expect("eight bytes")),
        }
    }
}

/// `si_code` values, for the faults this kernel raises.
pub mod code {
    /// `SEGV_MAPERR`: the address is not mapped.
    pub const MAPERR: i32 = 1;
    /// `SEGV_ACCERR`: it is mapped, and not like that.
    pub const ACCERR: i32 = 2;
    /// `ILL_ILLOPN`: an opcode that is not one.
    pub const ILLOPN: i32 = 2;
    /// `FPE_INTDIV`: divide by zero, or a quotient that does not fit.
    pub const INTDIV: i32 = 1;
    /// `TRAP_BRKPT`: `int3`.
    pub const BRKPT: i32 = 1;
    /// `SI_TKILL`: sent by `tgkill`, rather than by the machine.
    pub const TKILL: i32 = -6;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registers a handler reads have to be the ones it interrupted, and
    /// `sigreturn` has to put them all back — every one, because a handler
    /// that clobbers a callee-saved register and returns is a program whose
    /// caller silently loses a value.
    #[test]
    fn a_frame_round_trips_every_register() {
        let mut thread = Tcb::new();
        for (index, register) in thread.registers.iter_mut().enumerate() {
            *register = 0x1000 + index as u64;
        }
        thread.rip = 0xdead_beef;
        thread.flags.set_carry(true);
        let before = thread.clone();

        let bytes = frame(&thread, 11, Cause::default(), 0x4321, 0x55);

        // Somebody else's registers entirely, as a handler would leave.
        let mut after = Tcb::new();
        for register in after.registers.iter_mut() {
            *register = 0xffff;
        }
        after.rip = 0;
        let mask = restore(&mut after, &bytes);

        assert_eq!(after.registers, before.registers);
        assert_eq!(after.rip, before.rip);
        assert!(after.flags.carry(), "the flags came back");
        assert_eq!(mask, 0x55, "and the mask the handler must restore");
    }

    /// The layout is Linux's, and a handler indexes it by fixed offsets.
    #[test]
    fn the_frame_puts_the_program_counter_where_a_handler_looks() {
        let mut thread = Tcb::new();
        thread.rip = 0x1234_5678_9abc_def0;
        thread.registers[4] = 0x7fff_0000;
        let bytes = frame(&thread, 11, Cause::default(), 0, 0);
        // `gregs[REG_RIP]` is offset 168 of the ucontext on x86-64, and
        // `REG_RSP` is 160.
        let at = |offset: usize| {
            u64::from_le_bytes(
                bytes[UCONTEXT + offset..UCONTEXT + offset + 8]
                    .try_into()
                    .expect("eight bytes"),
            )
        };
        assert_eq!(at(168), 0x1234_5678_9abc_def0, "REG_RIP");
        assert_eq!(at(160), 0x7fff_0000, "REG_RSP");
    }

    #[test]
    fn a_fault_carries_the_address_that_caused_it() {
        let thread = Tcb::new();
        let bytes = frame(
            &thread,
            11,
            Cause {
                code: code::MAPERR,
                address: 0xbadc0ffee,
            },
            0,
            0,
        );
        assert_eq!(
            i32::from_le_bytes(bytes[SIGINFO..SIGINFO + 4].try_into().unwrap()),
            11
        );
        assert_eq!(
            i32::from_le_bytes(bytes[SIGINFO + 8..SIGINFO + 12].try_into().unwrap()),
            code::MAPERR
        );
        assert_eq!(
            u64::from_le_bytes(bytes[SIGINFO + 16..SIGINFO + 24].try_into().unwrap()),
            0xbadc0ffee
        );
    }
}
