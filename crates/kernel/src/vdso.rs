//! The vDSO: the clock the guest reads without asking.
//!
//! Linux maps a small shared object into every process holding fast
//! implementations of the calls that only *read* kernel state. glibc finds
//! it through `AT_SYSINFO_EHDR` in the auxiliary vector, resolves the
//! symbols out of its dynamic table, and calls them directly — which is why
//! a native `strace` of a program that reads the clock ten thousand times
//! shows no clock syscalls at all. Without one, glibc takes the syscall path
//! it keeps for exactly that case, and that was the single structural
//! divergence left between a native run of the demo stack and an
//! interpreted one: 6,455 of 6,621 extra calls, one cause.
//!
//! # How it can work here at all
//!
//! A real vDSO reads a page the kernel keeps fresh and interpolates between
//! updates with the processor's cycle counter. Both halves look impossible
//! here — this kernel's clock lives on the far side of the host boundary,
//! and not crossing it is the entire point.
//!
//! They are not. `rdtsc` already answers from the retired-instruction
//! counter (`cpu::exec::Cpu::timestamp`), so it plays the part of the
//! cycle counter exactly: monotone, cheap, and a pure function of execution.
//! The kernel samples the host clock at the points it *already* crosses the
//! boundary — a process finishing its slice, and the container going idle —
//! and publishes a timebase against that counter. Between samples the guest
//! computes the time itself.
//!
//! What that buys is better than what a real vDSO gives: between kernel
//! samples the time a guest reads is a pure function of how far it has
//! executed. The nondeterminism still enters exactly where it always did,
//! through the store, at points that are themselves a function of
//! execution — so a recorded run still replays.
//!
//! `the engine`'s comment on `rdtsc` used to say that nothing may calibrate
//! against it. That was right while the only thing that could was the
//! *guest*, which would have been inventing a relationship between a
//! counter and a clock it has no business knowing. The kernel holds both,
//! and calibrating is what it is for.
//!
//! # The rate is measured, not assumed
//!
//! Two consecutive samples give elapsed nanoseconds and elapsed ticks, and
//! their ratio is the rate. It therefore tracks whatever the machine
//! actually does — a slow host, a fast one, an accelerator that retires
//! instructions at a different rate — without anything being told.

use crate::errno::Errno;

/// The compiled image, built by `build.rs` from `the kernel/vdso/vdso.c`.
pub const IMAGE: &[u8] = include_bytes!(env!("KISAL_VDSO"));

/// What the kernel publishes for the vDSO to read.
///
/// **Must match `struct timebase` in `the kernel/vdso/vdso.c` byte for byte**,
/// and a test below pins it. Two definitions of one layout is exactly the
/// kind of agreement that rots silently: the C side would read a field that
/// had moved and answer a time from the wrong half of the struct, and
/// nothing would fail — the clock would simply be wrong.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Timebase {
    /// Odd while the kernel is writing. A reader that sees the same even
    /// value before and after knows nothing moved underneath it — Linux's
    /// seqlock, needed here for the same reason: a guest thread is preempted
    /// at instruction boundaries and the kernel refreshes at scheduling
    /// points, so a thread can be stopped in the middle of a read.
    pub sequence: u32,
    /// Zero until the kernel has a clock to publish. A vDSO reading zero
    /// answers `-ENOSYS`, which sends glibc to the syscall — the fallback,
    /// and the right answer for a container with no `/iso/time` mounted.
    pub usable: u32,
    pub base_realtime: u64,
    pub base_monotonic: u64,
    pub base_tsc: u64,
    /// Nanoseconds per tick as `(delta * multiplier) >> shift`.
    pub multiplier: u64,
    pub shift: u32,
    pub padding: u32,
}

/// How the fixed-point rate is represented.
///
/// Large, because the rate is *small*: `rdtsc` advances about a billion per
/// retired instruction and an instruction takes tens of nanoseconds, so
/// nanoseconds-per-tick is around 2×10⁻⁸. A shift of 32 would quantise that
/// to about half a percent; 48 leaves the multiplier in the millions, where
/// a unit of error is parts per million. The product needs 128 bits, which
/// is why the vDSO's arithmetic is a `mul` and a `shrd` rather than a
/// multiply.
pub const SHIFT: u32 = 48;

/// The rate to use before two samples exist to measure one from.
///
/// Fifty million instructions a second is what the engine measures on this
/// machine, and it only has to be close: it is replaced by a measured rate
/// at the second sample, which is microseconds later.
const ASSUMED_MIPS: u64 = 50;

impl Timebase {
    /// The multiplier for a measured rate of `nanoseconds` over `ticks`.
    ///
    /// Answers `None` for a sample too short to divide by, which is not an
    /// error: it means keep the rate you had.
    pub fn rate(nanoseconds: u64, ticks: u64) -> Option<u64> {
        if ticks == 0 || nanoseconds == 0 {
            return None;
        }
        // `(nanoseconds << SHIFT) / ticks`, in 128 bits because the
        // numerator does not fit in 64.
        Some((((nanoseconds as u128) << SHIFT) / ticks as u128) as u64)
    }

    /// The rate to start from, from the constants above.
    pub fn assumed_rate() -> u64 {
        let per_instruction = cpu::exec::TIMESTAMP_STEP;
        Self::rate(1_000_000_000, ASSUMED_MIPS * 1_000_000 * per_instruction)
            .unwrap_or(1)
    }

    /// What the vDSO would compute from this timebase at `tsc`.
    ///
    /// Here so that the kernel can answer its own `clock_gettime` the same
    /// way — a guest that calls the vDSO and a guest that calls the syscall
    /// must not see two different clocks, and the way to guarantee that is
    /// one arithmetic rather than two.
    pub fn at(&self, tsc: u64, monotonic: bool) -> u64 {
        let base = match monotonic {
            true => self.base_monotonic,
            false => self.base_realtime,
        };
        let since = tsc.wrapping_sub(self.base_tsc);
        base.saturating_add(((since as u128 * self.multiplier as u128) >> SHIFT) as u64)
    }
}

/// Where the vDSO and its timebase live in the guest's address space.
///
/// The timebase is the page *immediately below* the image, because the
/// vDSO finds it by subtracting a page from its own base — which needs no
/// relocation, and therefore no loader. A vDSO is mapped, not loaded.
#[derive(Clone, Copy, Debug)]
pub struct Placement {
    pub timebase: u64,
    pub image: u64,
    pub length: u64,
}

impl<S: crate::abi::Store, M: crate::machine::Machine> crate::syscall::Kernel<'_, S, M> {
    /// Maps the vDSO and its timebase, and answers where they went.
    ///
    /// Called at `exec`, after the program and its interpreter are placed
    /// and before the stack is built — because the stack carries the
    /// auxiliary vector, and `AT_SYSINFO_EHDR` is how anything finds this.
    pub(crate) fn map_vdso(&mut self) -> Result<Placement, Errno> {
        let page = crate::space::PAGE;
        let length = (IMAGE.len() as u64).next_multiple_of(page);
        let span = page + length;
        let request = crate::space::Request {
            hint: 0,
            length: span,
            prot: crate::space::prot::READ,
            flags: 0,
            backing: crate::space::Backing::Anonymous,
        };
        let placed = {
            let Self {
                space,
                machine,
                pages,
                enforcement,
                ..
            } = self;
            let enforcement = *enforcement;
            space
                .map(&request, &mut |to| {
                    crate::syscall::grow_memory(machine, pages, enforcement, to)
                })
                .map_err(|_| Errno::NoMemory)?
        };
        let timebase = placed.0;
        let image = timebase + page;
        // The table learns the mapping before anything is written into it:
        // the sync is where the pages become this process's, and a page
        // that was somebody else's is zeroed as it does — so bytes placed
        // ahead of it would be placed into the wrong process and then
        // erased.
        self.sync_pages(timebase, timebase + span);
        // The bytes next, while the whole span is still one plain readable
        // mapping — `place` is the kernel writing its own memory and does
        // not consult protections, but the *tree* is about to say this is
        // not writable and the two must never disagree about anything.
        self.pages
            .place_fill(timebase, span, 0)
            .map_err(|_| Errno::Fault)?;
        self.pages.place(image, IMAGE).map_err(|_| Errno::Fault)?;
        // Then the image becomes executable — through the tree, with the
        // table synced from it, because a protection set on one and not the
        // other is the drift `verify_pages` exists to catch. Read and
        // execute whatever the image's own header says: the linker gives it
        // one RWX segment, and a guest has no business writing its own
        // clock.
        self.space
            .protect(
                image,
                length,
                crate::space::prot::READ | crate::space::prot::EXEC,
            )
            .map_err(|_| Errno::NoMemory)?;
        self.sync_pages(timebase, timebase + span);
        self.vdso = Some(Placement {
            timebase,
            image,
            length,
        });
        // Published straight away, so that a program which reads the clock
        // before the first scheduling point gets an answer rather than a
        // fallback.
        self.refresh_timebase();
        Ok(Placement {
            timebase,
            image,
            length,
        })
    }

    /// Samples the host clock and publishes a new timebase.
    ///
    /// At the points the container already crosses the boundary — a process
    /// finishing its slice, and going idle — so this costs no crossing that
    /// was not happening anyway.
    ///
    /// **Monotonic time may not go backwards**, and a fresh sample can
    /// easily be behind what the last one's rate had already extrapolated
    /// to. So the base is moved rather than the answer: the published
    /// monotonic base is never less than what the previous timebase would
    /// have said at this instant. Real kernels do the same thing when they
    /// slew a clock, and for the same reason.
    pub(crate) fn refresh_timebase(&mut self) {
        let Some(placed) = self.vdso else {
            return;
        };
        let Some(tsc) = self.machine.tcb().map(|thread| thread.timestamp()) else {
            return;
        };
        let (Some(realtime), Some(monotonic)) = (self.realtime(), self.monotonic()) else {
            // No clock mounted. The page stays unusable and the vDSO sends
            // every caller to the syscall, which answers `EINVAL` — the same
            // refusal by name a container with no `/iso/time` has always
            // got.
            return;
        };
        let previous = self.timebase;
        let multiplier = match previous.usable != 0 {
            // Measured over the interval just ended, so the rate tracks
            // whatever the machine actually does.
            true => Timebase::rate(
                monotonic.saturating_sub(previous.base_monotonic),
                tsc.wrapping_sub(previous.base_tsc),
            )
            .unwrap_or(previous.multiplier),
            false => Timebase::assumed_rate(),
        };
        let extrapolated = match previous.usable != 0 {
            true => previous.at(tsc, true),
            false => 0,
        };
        let published = Timebase {
            sequence: previous.sequence,
            usable: 1,
            base_realtime: realtime,
            base_monotonic: monotonic.max(extrapolated),
            base_tsc: tsc,
            multiplier,
            shift: SHIFT,
            padding: 0,
        };
        self.timebase = published;
        self.publish(placed.timebase, published);
    }

    /// Writes a timebase into the guest's page, under the seqlock.
    fn publish(&mut self, at: u64, mut published: Timebase) {
        // Odd while writing, and even after — which is the whole protocol,
        // and the reason the sequence is written twice on its own rather
        // than as part of the struct.
        published.sequence = published.sequence.wrapping_add(1);
        let opening = published.sequence;
        let _ = self.pages.place(at, &opening.to_le_bytes());
        let _ = self.pages.place(at, &encode(&published));
        published.sequence = published.sequence.wrapping_add(1);
        self.timebase.sequence = published.sequence;
        let _ = self.pages.place(at, &published.sequence.to_le_bytes());
    }

    /// The wall clock in nanoseconds, or `None` when none is mounted.
    pub(crate) fn realtime(&mut self) -> Option<u64> {
        let mut bytes = Vec::new();
        if self.store.read(crate::paths::TIME_REALTIME, &mut bytes)
            != crate::abi::StoreOutcome::Present
        {
            return None;
        }
        u64::try_from(crate::syscall::parse_nanoseconds(&bytes)?).ok()
    }
}

/// A timebase as the vDSO reads it.
fn encode(held: &Timebase) -> [u8; SIZE] {
    let mut bytes = [0u8; SIZE];
    bytes[0..4].copy_from_slice(&held.sequence.to_le_bytes());
    bytes[4..8].copy_from_slice(&held.usable.to_le_bytes());
    bytes[8..16].copy_from_slice(&held.base_realtime.to_le_bytes());
    bytes[16..24].copy_from_slice(&held.base_monotonic.to_le_bytes());
    bytes[24..32].copy_from_slice(&held.base_tsc.to_le_bytes());
    bytes[32..40].copy_from_slice(&held.multiplier.to_le_bytes());
    bytes[40..44].copy_from_slice(&held.shift.to_le_bytes());
    bytes
}

/// The struct's size on the wire, which is the C side's.
pub const SIZE: usize = 48;

const _: () = assert!(
    core::mem::size_of::<Timebase>() == SIZE,
    "the timebase's Rust layout must match the C one in `the kernel/vdso/vdso.c`"
);
