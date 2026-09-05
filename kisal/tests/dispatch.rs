//! The kernel's logic, falsified natively in milliseconds.
//!
//! This is the innermost tier of the testing discipline: anything that can be
//! decided without emulation is decided here, so that the emulated tiers are
//! spent on what only they can check. Every row kisal grows gets its routing
//! and its errno tested at this level first.

use kisal::machine::{GuestBuffer, GuestBytes};
use kisal::abi::{Store, StoreOutcome};
use kisal::errno::Errno;
use kisal::machine::{Machine, Registers};
use kisal::paths;
use kisal::syscall::{Arguments, Fault, Kernel, Outcome, number};

/// An in-memory stand-in for the host: records every write, answers reads
/// from what it recorded.
#[derive(Default)]
struct Recording {
    written: Vec<(Vec<Vec<u8>>, Vec<u8>)>,
    /// Paths the store refuses, so the failure path has a way to be reached.
    refuse: Vec<Vec<Vec<u8>>>,
}

impl Recording {
    fn key(path: &[&[u8]]) -> Vec<Vec<u8>> {
        path.iter().map(|segment| segment.to_vec()).collect()
    }

    fn contents(&self, path: &[&[u8]]) -> Vec<u8> {
        let key = Self::key(path);
        self.written
            .iter()
            .filter(|(written, _)| *written == key)
            .flat_map(|(_, bytes)| bytes.iter().copied())
            .collect()
    }
}

impl Store for Recording {
    fn last_error(&self, into: &mut Vec<u8>) {
        into.extend_from_slice(b"the sink refused it");
    }

    fn read(&mut self, path: &[&[u8]], into: &mut Vec<u8>) -> StoreOutcome {
        let bytes = self.contents(path);
        if bytes.is_empty() {
            return StoreOutcome::Absent;
        }
        into.extend_from_slice(&bytes);
        StoreOutcome::Present
    }

    fn write(&mut self, path: &[&[u8]], data: &[u8]) -> StoreOutcome {
        if self.refuse.contains(&Self::key(path)) {
            return StoreOutcome::Failed;
        }
        self.written.push((Self::key(path), data.to_vec()));
        StoreOutcome::Present
    }
}

/// An image with nothing in it, for the rows that have nothing to do with
/// the filesystem. Leaked because the kernel borrows it for its lifetime and
/// a test's kernel lives as long as the test.
fn empty_image() -> kisal::image::Image<'static> {
    let baked: &'static baker::Image = Box::leak(Box::new(baker::bake_empty()));
    kisal::image::Image::parse(&baked.index, &baked.blob).expect("parse an empty image")
}

/// A `write(2)`'s arguments, with the buffer named by the address of a real
/// slice — which is exactly what a guest address is inside the module, so the
/// code under test here is the code that ships.
/// The bytes have to live at a *guest* address, so the caller keeps the
/// buffer alive and this only names it.
fn write_of(descriptor: i64, buffer: &GuestBuffer) -> Arguments {
    Arguments::new([
        descriptor,
        buffer.address(),
        buffer.len() as i64,
        0,
        0,
        0,
    ])
}

#[test]
fn stdout_and_stderr_reach_their_own_console_paths() {
    let mut kernel = Kernel::new(Recording::default(), Registers::default(), empty_image());
    assert_eq!(
        kernel.dispatch(number::WRITE, write_of(1, &GuestBuffer::of(b"out"))),
        Outcome::Done(3)
    );
    assert_eq!(
        kernel.dispatch(number::WRITE, write_of(2, &GuestBuffer::of(b"err"))),
        Outcome::Done(3)
    );
    assert_eq!(kernel.store.contents(paths::CONSOLE_STDOUT), b"out");
    assert_eq!(kernel.store.contents(paths::CONSOLE_STDERR), b"err");
}

#[test]
fn a_descriptor_with_no_backend_is_ebadf() {
    let mut kernel = Kernel::new(Recording::default(), Registers::default(), empty_image());
    assert_eq!(
        kernel.dispatch(number::WRITE, write_of(3, &GuestBuffer::of(b"nowhere"))),
        Outcome::Done(Errno::BadFile.as_result())
    );
    assert!(kernel.store.written.is_empty());
}

/// A zero-length write succeeds without touching memory, which is what lets
/// it name a null buffer — and callers do.
#[test]
fn a_zero_length_write_never_reads_the_buffer() {
    let mut kernel = Kernel::new(Recording::default(), Registers::default(), empty_image());
    let arguments = Arguments::new([1, 0, 0, 0, 0, 0]);
    assert_eq!(kernel.dispatch(number::WRITE, arguments), Outcome::Done(0));
}

#[test]
fn a_negative_length_is_einval() {
    let mut kernel = Kernel::new(Recording::default(), Registers::default(), empty_image());
    let arguments = Arguments::new([1, 0, -1, 0, 0, 0]);
    assert_eq!(
        kernel.dispatch(number::WRITE, arguments),
        Outcome::Done(Errno::Invalid.as_result())
    );
}

/// A store that fails is an `EIO`, not a fault: the store's error string is a
/// diagnostic, and deciding what it means to the guest is the syscall row's
/// job. POSIX lives in the kernel, including here.
#[test]
fn a_failing_store_becomes_an_errno() {
    let mut store = Recording::default();
    store.refuse.push(Recording::key(paths::CONSOLE_STDOUT));
    let mut kernel = Kernel::new(store, Registers::default(), empty_image());
    assert_eq!(
        kernel.dispatch(number::WRITE, write_of(1, &GuestBuffer::of(b"lost"))),
        Outcome::Done(Errno::Io.as_result())
    );

    // The errno cannot carry a reason, so the store's own account of the
    // failure has to reach the kernel log or it does not exist anywhere.
    let logged = String::from_utf8(kernel.store.contents(paths::LOG_ERROR)).expect("utf-8");
    assert_eq!(
        logged,
        "kisal: the store at /iso/console/stdout failed: the sink refused it"
    );
}

/// A failing *log* store is not reported through itself. One failed write is
/// a diagnostic; a loop is a hang.
#[test]
fn a_failing_log_store_is_not_reported_through_itself() {
    let mut store = Recording::default();
    store.refuse.push(Recording::key(paths::CONSOLE_STDOUT));
    store.refuse.push(Recording::key(paths::LOG_ERROR));
    let mut kernel = Kernel::new(store, Registers::default(), empty_image());
    assert_eq!(
        kernel.dispatch(number::WRITE, write_of(1, &GuestBuffer::of(b"lost"))),
        Outcome::Done(Errno::Io.as_result())
    );
    assert!(kernel.store.contents(paths::LOG_ERROR).is_empty());
}

/// The named half of the loud-error policy.
///
/// This needs a syscall the table *names* and does not implement, so its
/// choice has to move as the grind proceeds — `getpid` was it until `getpid`
/// was implemented, then `clone3` until threads arrived. Every number the
/// table *names* now has a row, so this uses one it does not name, which is
/// the case that will outlive every particular choice: `setuid`, which a
/// container with one user has no business implementing.
///
/// The premise is asserted, so the day it gains a row this fails and says so
/// rather than passing quietly.
#[test]
fn an_unimplemented_syscall_faults_and_names_itself() {
    /// `init_module(2)`: loading a kernel module.
    ///
    /// Chosen because it will *stay* unimplemented. This test needs a
    /// number the kernel does not know, and the previous choice — `setuid`
    /// — stopped being one the day nginx's traced privilege drop put it on
    /// the worklist. A container has no kernel to load a module into, so
    /// this one is safe from the same fate.
    const UNIMPLEMENTED: i64 = 175;
    let mut kernel = Kernel::new(Recording::default(), Registers::default(), empty_image());
    let outcome = kernel.dispatch(UNIMPLEMENTED, Arguments::new([0; 6]));
    let Outcome::Fault(fault) = outcome else {
        panic!("an unimplemented syscall produced {outcome:?} instead of a fault");
    };
    assert_eq!(
        fault,
        Fault {
            number: UNIMPLEMENTED,
            name: None,
            arguments: [0; 6],
            detail: None,
        }
    );
    let mut message = String::new();
    fault.message(&mut message);
    assert_eq!(
        message,
        "kisal: unimplemented syscall 175 with (0, 0, 0, 0, 0, 0)"
    );
}

/// A number outside the table prints as a number rather than as a guess. The
/// loud error's whole value is that it is accurate.
#[test]
fn an_unknown_number_is_reported_as_a_number() {
    let mut kernel = Kernel::new(Recording::default(), Registers::default(), empty_image());
    let outcome = kernel.dispatch(9999, Arguments::new([0; 6]));
    let Outcome::Fault(fault) = outcome else {
        panic!("expected a fault, got {outcome:?}");
    };
    assert_eq!(fault.name, None);
    let mut message = String::new();
    fault.message(&mut message);
    assert_eq!(
        message,
        "kisal: unimplemented syscall 9999 with (0, 0, 0, 0, 0, 0)"
    );
}

/// Every errno the kernel can return lands in the band a libc recognises.
/// A value outside it would be read by the guest as a successful result.
#[test]
fn every_errno_lands_in_the_band_libc_recognises() {
    for errno in [
        Errno::Perm,
        Errno::NoEntry,
        Errno::Io,
        Errno::BadFile,
        Errno::NoMemory,
        Errno::Access,
        Errno::Fault,
        Errno::Busy,
        Errno::Exists,
        Errno::NotDir,
        Errno::IsDir,
        Errno::Invalid,
        Errno::NoTty,
        Errno::Pipe,
        Errno::NameTooLong,
        Errno::NoSys,
        Errno::Loop,
    ] {
        let result = errno.as_result();
        assert!(
            (-4095..0).contains(&result),
            "{errno:?} encodes as {result}, outside the -4095..-1 band"
        );
    }
}

// ---- arch_prctl: the thread pointer ----------------------------------------

const ARCH_SET_GS: i64 = 0x1001;
const ARCH_SET_FS: i64 = 0x1002;
const ARCH_GET_FS: i64 = 0x1003;
const ARCH_GET_GS: i64 = 0x1004;

#[test]
fn setting_the_fs_base_moves_the_thread_pointer() {
    let mut kernel = Kernel::new(Recording::default(), Registers::default(), empty_image());
    assert_eq!(
        kernel.dispatch(
            number::ARCH_PRCTL,
            Arguments::new([ARCH_SET_FS, 0x1234_5678, 0, 0, 0, 0])
        ),
        Outcome::Done(0)
    );
    assert_eq!(kernel.machine.segment_base(), 0x1234_5678);
}

/// `ARCH_GET_FS` writes the base to the address it is given — eight bytes,
/// little-endian, which is what every libc reads back.
#[test]
fn getting_the_fs_base_writes_it_where_asked() {
    let mut kernel = Kernel::new(Recording::default(), Registers::default(), empty_image());
    kernel.machine.set_segment_base(0x0011_2233_4455_6677);

    let destination = GuestBytes::<8>::new();
    let address = destination.address();
    assert_eq!(
        kernel.dispatch(
            number::ARCH_PRCTL,
            Arguments::new([ARCH_GET_FS, address, 0, 0, 0, 0])
        ),
        Outcome::Done(0)
    );
    assert_eq!(u64::from_le_bytes(*destination), 0x0011_2233_4455_6677);
}

#[test]
fn getting_the_fs_base_into_nothing_is_efault() {
    let mut kernel = Kernel::new(Recording::default(), Registers::default(), empty_image());
    assert_eq!(
        kernel.dispatch(
            number::ARCH_PRCTL,
            Arguments::new([ARCH_GET_FS, 0, 0, 0, 0, 0])
        ),
        Outcome::Done(Errno::Fault.as_result())
    );
}

/// `%gs` is a loud error here as well as in the translator, and the fault
/// names the sub-function rather than the syscall — `arch_prctl` mostly
/// works, and a worklist entry saying otherwise would send someone to the
/// wrong place.
#[test]
fn the_gs_base_is_a_named_fault_not_a_plausible_answer() {
    for request in [ARCH_SET_GS, ARCH_GET_GS] {
        let mut kernel = Kernel::new(Recording::default(), Registers::default(), empty_image());
        let outcome = kernel.dispatch(number::ARCH_PRCTL, Arguments::new([request, 0, 0, 0, 0, 0]));
        let Outcome::Fault(fault) = outcome else {
            panic!("`%gs` produced {outcome:?} instead of a fault");
        };
        assert_eq!(fault.number, number::ARCH_PRCTL);
        let mut message = String::new();
        fault.message(&mut message);
        assert!(
            message.contains("arch_prctl") && message.contains("%gs"),
            "the fault says {message:?}"
        );
    }
}

/// A sub-function Linux does not have either is `EINVAL`, which is what
/// Linux answers — a deliberate row rather than a fallthrough.
#[test]
fn an_unknown_arch_prctl_request_is_einval() {
    let mut kernel = Kernel::new(Recording::default(), Registers::default(), empty_image());
    assert_eq!(
        kernel.dispatch(number::ARCH_PRCTL, Arguments::new([0x9999, 0, 0, 0, 0, 0])),
        Outcome::Done(Errno::Invalid.as_result())
    );
}

/// A sub-function Linux *does* have and kisal does not implement is a named
/// fault. `EINVAL` there would be a plausible wrong answer — the guest would
/// conclude the kernel is too old and carry on.
#[test]
fn a_real_but_unimplemented_arch_prctl_request_is_a_named_fault() {
    for (request, expected) in [(0x1011i64, "CPUID"), (0x1012, "CPUID")] {
        let mut kernel = Kernel::new(Recording::default(), Registers::default(), empty_image());
        let outcome = kernel.dispatch(number::ARCH_PRCTL, Arguments::new([request, 0, 0, 0, 0, 0]));
        let Outcome::Fault(fault) = outcome else {
            panic!("request {request:#x} produced {outcome:?} instead of a fault");
        };
        let mut message = String::new();
        fault.message(&mut message);
        assert!(message.contains(expected), "the fault says {message:?}");
    }
}

// ---- guest memory: the bounds check POSIX requires -------------------------

use kisal::memory::GuestReader;
use targum::space::{PAGE_SIZE, Protection, Space};

/// An address space of `limit` bytes with everything in it reachable — the
/// ahead-of-time world's arrangement, which is what these cases are about.
/// What they check is the arithmetic that decides whether a range is inside
/// the space at all, so they say the limit explicitly rather than taking
/// whatever a particular module's memory happens to be.
fn flat(limit: u64) -> Space {
    let mut space = Space::new(limit);
    // From the second page, as the kernel's own flattening does: page zero
    // is never mapped, which is what makes a null pointer `EFAULT`.
    space.protect(PAGE_SIZE, limit - PAGE_SIZE, Protection::ALL);
    space
}

/// The check is pure arithmetic over (address, length, limit), so it is
/// tested against explicit limits rather than against whatever a particular
/// module's memory happens to be.
#[test]
fn a_range_inside_the_guest_is_accepted() {
    let space = flat(0x1_0000);
    let memory = GuestReader::new(&space);
    assert_eq!(memory.check(0x1100, 0x10), Ok(()));
    assert_eq!(
        memory.check(0xfff0, 0x10),
        Ok(()),
        "ending exactly at the limit"
    );
}

#[test]
fn a_range_past_the_end_is_efault_not_a_trap() {
    let space = flat(0x1_0000);
    let memory = GuestReader::new(&space);
    assert_eq!(memory.check(0xfff8, 0x10), Err(Errno::Fault));
    assert_eq!(memory.check(0x1_0000, 1), Err(Errno::Fault));
}

/// The bug this exists to prevent: a length whose top half would be discarded
/// by the cast to `usize` inside the module. Validated at full width, it is
/// simply out of range.
#[test]
fn a_length_that_would_truncate_is_refused() {
    let space = flat(0x1_0000);
    let memory = GuestReader::new(&space);
    assert_eq!(memory.check(0x1000, 0x1_0000_0000), Err(Errno::Fault));
    assert_eq!(memory.check(0x1000, 0x1_0000_0005), Err(Errno::Fault));
}

/// An address whose top half would be discarded must not slip past a check
/// written against the low half.
#[test]
fn an_address_that_would_truncate_is_refused() {
    let space = flat(0x1_0000);
    let memory = GuestReader::new(&space);
    assert_eq!(memory.check(0x1_0000_0000, 8), Err(Errno::Fault));
    assert_eq!(
        memory.check(0x1_0000_0000, 0),
        Ok(()),
        "but a zero-length access touches nothing, wherever it points"
    );
}

/// `slice::from_raw_parts` is undefined for a range that wraps the address
/// space. A guest can ask for one, so the check has to refuse it rather than
/// overflow into a small end address.
///
/// The limit is an ordinary one: what makes this case dangerous is the
/// *addition* overflowing, which happens whatever the limit is, and a limit
/// of `u64::MAX` would only ask the page table for a bitmap covering an
/// address space the machine does not have.
#[test]
fn a_range_that_wraps_the_address_space_is_refused() {
    let space = flat(0x1_0000);
    let memory = GuestReader::new(&space);
    assert_eq!(memory.check(u64::MAX - 4, 16), Err(Errno::Fault));
    assert_eq!(memory.check(u64::MAX, 1), Err(Errno::Fault));
}

/// Linux answers `EFAULT` for a null buffer with a real length, and accepts
/// one with no length.
#[test]
fn a_null_buffer_is_efault_only_when_it_would_be_read() {
    let space = flat(0x1_0000);
    let memory = GuestReader::new(&space);
    assert_eq!(memory.check(0, 1), Err(Errno::Fault));
    assert_eq!(memory.check(0, 0), Ok(()));
}

/// The syscall rows answer with the errno rather than dereferencing, which is
/// the difference between one failed call and a dead instance.
#[test]
fn a_write_outside_the_guest_is_efault() {
    let mut kernel = Kernel::new(
        Recording::default(),
        Registers {
            memory_limit: 0x1_0000,
            ..Registers::default()
        },
        empty_image(),
    );
    let arguments = Arguments::new([1, 0xf000, 0x1_0000_0000, 0, 0, 0]);
    assert_eq!(
        kernel.dispatch(number::WRITE, arguments),
        Outcome::Done(Errno::Fault.as_result())
    );
    assert!(kernel.store.written.is_empty(), "it wrote anyway");
}

#[test]
fn an_arch_prctl_get_outside_the_guest_is_efault() {
    let mut kernel = Kernel::new(
        Recording::default(),
        Registers {
            memory_limit: 0x1_0000,
            ..Registers::default()
        },
        empty_image(),
    );
    assert_eq!(
        kernel.dispatch(
            number::ARCH_PRCTL,
            Arguments::new([ARCH_GET_FS, 0x1_0000_0000, 0, 0, 0, 0])
        ),
        Outcome::Done(Errno::Fault.as_result())
    );
}

// ---- the canonical-ABI return areas ----------------------------------------
//
// The layouts are pinned against literals written from the specification, not
// against the constants the code itself uses. Two sides that derive their
// expectations from one definition cannot detect that the definition is
// wrong; the host writes these offsets independently, so this is where the
// two are made to agree with something outside both.

use kisal::abi::{ReadResult, Slice, WriteResult};

#[test]
fn a_list_lowers_to_a_pointer_and_a_length() {
    assert_eq!(core::mem::size_of::<Slice>(), 8);
    assert_eq!(core::mem::align_of::<Slice>(), 4);
}

/// `result<option<list<u8>>, string>`: a one-byte discriminant padded to the
/// payload's four-byte alignment, then a twelve-byte union.
#[test]
fn the_read_return_area_is_sixteen_bytes() {
    assert_eq!(core::mem::size_of::<ReadResult>(), 16);
    assert_eq!(core::mem::align_of::<ReadResult>(), 4);
    assert_eq!(core::mem::offset_of!(ReadResult, discriminant), 0);
    assert_eq!(core::mem::offset_of!(ReadResult, arm), 4);
}

/// `result<list<list<u8>>, string>`: the same discriminant, then an
/// eight-byte union — both arms are a `(pointer, length)`.
#[test]
fn the_write_return_area_is_twelve_bytes() {
    assert_eq!(core::mem::size_of::<WriteResult>(), 12);
    assert_eq!(core::mem::align_of::<WriteResult>(), 4);
    assert_eq!(core::mem::offset_of!(WriteResult, discriminant), 0);
    assert_eq!(core::mem::offset_of!(WriteResult, payload), 4);
}

/// The two arms overlap, and reading the wrong one must not be possible by
/// accident: on `err` the message starts where `ok`'s inner discriminant is.
/// The canonical ABI numbers a variant's cases in declaration order, and
/// `option` is `none | some`. Asserted against the literals rather than the
/// constants used to build the value, because the two sides of this boundary
/// agreeing with each other is not the same as either agreeing with the spec.
#[test]
fn the_option_discriminant_follows_declaration_order() {
    assert_eq!(kisal::abi::OPTION_NONE, 0);
    assert_eq!(kisal::abi::OPTION_SOME, 1);
}

#[test]
fn the_two_arms_of_a_read_result_do_not_leak_into_each_other() {
    let ok_some = ReadResult {
        discriminant: 0,
        arm: [kisal::abi::OPTION_SOME, 0x1000, 5],
    };
    assert!(!ok_some.is_error());
    assert_eq!(ok_some.error(), None);
    let value = ok_some.value().expect("ok(some) has a value");
    assert_eq!((value.pointer, value.length), (0x1000, 5));

    let ok_none = ReadResult {
        discriminant: 0,
        arm: [kisal::abi::OPTION_NONE, 0, 0],
    };
    assert!(!ok_none.is_error());
    assert!(ok_none.value().is_none());

    // The message's (pointer, length) sits at [4..12], one word earlier than
    // the bytes would — which is exactly the mistake the union shape exists
    // to prevent.
    let failed = ReadResult {
        discriminant: 1,
        arm: [0x2000, 11, 0],
    };
    assert!(failed.is_error());
    assert!(failed.value().is_none(), "an error is not a value");
    let message = failed.error().expect("err carries a message");
    assert_eq!((message.pointer, message.length), (0x2000, 11));
}

/// A store that answers the clock paths, so the time the guest reads is a
/// time the test chose.
struct Clock {
    realtime: Vec<u8>,
    monotonic: Vec<u8>,
}

impl Store for Clock {
    fn read(&mut self, path: &[&[u8]], into: &mut Vec<u8>) -> StoreOutcome {
        let bytes = if path == paths::TIME_REALTIME {
            &self.realtime
        } else if path == paths::TIME_MONOTONIC {
            &self.monotonic
        } else {
            return StoreOutcome::Absent;
        };
        if bytes.is_empty() {
            return StoreOutcome::Absent;
        }
        into.extend_from_slice(bytes);
        StoreOutcome::Present
    }

    fn write(&mut self, _path: &[&[u8]], _data: &[u8]) -> StoreOutcome {
        StoreOutcome::Present
    }
}

/// The `timespec` a `clock_gettime` left behind.
fn timespec(image: &[u8; 16]) -> (i64, i64) {
    (
        i64::from_le_bytes(image[..8].try_into().expect("eight bytes")),
        i64::from_le_bytes(image[8..].try_into().expect("eight bytes")),
    )
}

fn clock_of(clock: i64, destination: &mut [u8; 16]) -> Arguments {
    Arguments::new([clock, destination.as_mut_ptr() as usize as i64, 0, 0, 0, 0])
}

/// `clock_gettime` splits the host's nanoseconds into a `timespec`, and the
/// two clocks are two clocks.
///
/// A native process never issues this syscall — glibc reads the vDSO — so
/// nothing about it can be checked by comparing against a `strace`. What can
/// be checked is that the arithmetic is right and that the caller reaches
/// the clock it asked for, which is what this does: the two paths hold
/// deliberately different times, and every clock id lands on one of them.
#[test]
fn clock_gettime_divides_the_hosts_nanoseconds() {
    let store = Clock {
        realtime: b"1756400000123456789".to_vec(),
        monotonic: b"42000000042\n".to_vec(),
    };
    let mut kernel = Kernel::new(store, Registers::default(), empty_image());

    let mut image = GuestBytes::<16>::new();
    assert_eq!(
        kernel.dispatch(number::CLOCK_GETTIME, clock_of(0, &mut image)),
        Outcome::Done(0)
    );
    assert_eq!(timespec(&image), (1_756_400_000, 123_456_789));

    // The trailing newline a host writing a number into a file is entitled
    // to leave behind.
    assert_eq!(
        kernel.dispatch(number::CLOCK_GETTIME, clock_of(1, &mut image)),
        Outcome::Done(0)
    );
    assert_eq!(timespec(&image), (42, 42));

    // Every id Linux numbers, landing on the clock it names. `MONOTONIC_RAW`,
    // the coarse pair and `BOOTTIME` are the same two clocks under other
    // promises, and answering them from the wrong one would be a bug no
    // caller could see until it went backwards.
    for (clock, expected) in [
        (0i64, 1_756_400_000i64),
        (5, 1_756_400_000),
        (1, 42),
        (4, 42),
        (6, 42),
        (7, 42),
    ] {
        assert_eq!(
            kernel.dispatch(number::CLOCK_GETTIME, clock_of(clock, &mut image)),
            Outcome::Done(0),
            "clock {clock} was refused"
        );
        assert_eq!(
            timespec(&image).0,
            expected,
            "clock {clock} read the wrong one"
        );
    }

    // Per-process and per-thread CPU time, which this kernel does not
    // account for.
    for clock in [2i64, 3, 11, -1] {
        assert_eq!(
            kernel.dispatch(number::CLOCK_GETTIME, clock_of(clock, &mut image)),
            Outcome::Done(Errno::Invalid.as_result()),
            "clock {clock} was answered"
        );
    }
}

/// A time before 1970 is a negative count of nanoseconds, and `timespec`'s
/// remainder is still positive: the seconds field rounds towards negative
/// infinity rather than towards zero.
///
/// Division that truncates gets this wrong by a whole second and only for
/// dates nobody tests with, which is why it is written down here.
#[test]
fn clock_gettime_floors_a_time_before_the_epoch() {
    let store = Clock {
        realtime: b"-1500000000".to_vec(),
        monotonic: b"0".to_vec(),
    };
    let mut kernel = Kernel::new(store, Registers::default(), empty_image());

    let mut image = GuestBytes::<16>::new();
    assert_eq!(
        kernel.dispatch(number::CLOCK_GETTIME, clock_of(0, &mut image)),
        Outcome::Done(0)
    );
    // −1.5 seconds is −2 seconds plus half of one, not −1 seconds minus half.
    assert_eq!(timespec(&image), (-2, 500_000_000));

    assert_eq!(
        kernel.dispatch(number::CLOCK_GETTIME, clock_of(1, &mut image)),
        Outcome::Done(0)
    );
    assert_eq!(timespec(&image), (0, 0));
}

/// A container whose host mounted no clock has none, and asking what time it
/// is fails rather than being told the epoch.
///
/// The same capability decision as entropy, for a stronger reason: a
/// plausible wrong time is a certificate that verifies, an expiry that has
/// not passed, and a log that says something untrue.
#[test]
fn clock_gettime_without_a_mount_is_refused_rather_than_invented() {
    let mut kernel = Kernel::new(Recording::default(), Registers::default(), empty_image());
    let mut image = GuestBytes::<16>::new();
    *image = [0xa5u8; 16];
    for clock in [0i64, 1] {
        assert_eq!(
            kernel.dispatch(number::CLOCK_GETTIME, clock_of(clock, &mut image)),
            Outcome::Done(Errno::Invalid.as_result())
        );
    }
    assert_eq!(*image, [0xa5u8; 16], "the destination was written anyway");
}

/// A clock that answers something other than a number is a broken host, not
/// a time.
#[test]
fn clock_gettime_refuses_what_it_cannot_parse() {
    for answer in [
        &b""[..],
        b"   ",
        b"twelve",
        b"12.5",
        b"1e9",
        b"0x10",
        b"12 34",
        b"-",
        // Past what a `timespec`'s seconds field can hold, which is a host
        // answering in some other unit.
        b"999999999999999999999",
    ] {
        let store = Clock {
            realtime: answer.to_vec(),
            monotonic: answer.to_vec(),
        };
        let mut kernel = Kernel::new(store, Registers::default(), empty_image());
        let mut image = GuestBytes::<16>::new();
        assert_eq!(
            kernel.dispatch(number::CLOCK_GETTIME, clock_of(0, &mut image)),
            Outcome::Done(Errno::Invalid.as_result()),
            "{:?} was accepted as a time",
            String::from_utf8_lossy(answer)
        );
    }
}

/// A signal mask is read back, and the two signals that cannot be blocked
/// are not.
#[test]
fn the_signal_mask_is_kept_and_read_back() {
    let mut kernel = Kernel::new(Recording::default(), Registers::default(), empty_image());
    let mut set = GuestBytes::<8>::new();
    let mut old = GuestBytes::<8>::new();
    let mask = |how: i64, set: &mut [u8; 8], old: &mut [u8; 8]| {
        Arguments::new([
            how,
            set.as_mut_ptr() as usize as i64,
            old.as_mut_ptr() as usize as i64,
            8,
            0,
            0,
        ])
    };

    // Block SIGABRT (6) and SIGKILL (9); only the first can be blocked.
    *set = (signal_bit(6) | signal_bit(9)).to_le_bytes();
    assert_eq!(
        kernel.dispatch(number::RT_SIGPROCMASK, mask(0, &mut set, &mut old)),
        Outcome::Done(0)
    );
    assert_eq!(u64::from_le_bytes(*old), 0, "the mask started non-empty");

    *set = 0u64.to_le_bytes();
    assert_eq!(
        kernel.dispatch(number::RT_SIGPROCMASK, mask(2, &mut set, &mut old)),
        Outcome::Done(0)
    );
    assert_eq!(
        u64::from_le_bytes(*old),
        signal_bit(6),
        "SIGKILL was blockable, or SIGABRT was not kept"
    );

    // A size other than eight is a caller built against another kernel.
    let mut wrong = Arguments::new([0, set.as_mut_ptr() as usize as i64, 0, 16, 0, 0]);
    wrong.values[3] = 16;
    assert_eq!(
        kernel.dispatch(number::RT_SIGPROCMASK, wrong),
        Outcome::Done(Errno::Invalid.as_result())
    );
}

/// A fatal signal sent to this process ends it; a blocked one does not.
///
/// This is `abort`: it unblocks `SIGABRT` and raises it at itself, precisely
/// so nothing can stop it. The status is what `wait` reports for a process
/// killed by a signal, which a shell prints as 128 plus the number.
#[test]
fn a_fatal_signal_to_this_process_ends_it() {
    let mut kernel = Kernel::new(Recording::default(), Registers::default(), empty_image());
    let raise = |signal: i64| Arguments::new([1, 1, signal, 0, 0, 0]);

    assert_eq!(
        kernel.dispatch(number::TGKILL, raise(6)),
        Outcome::Exit(134)
    );

    // Blocked, so it stays pending and nothing delivers it.
    let mut set = GuestBytes::<8>::new();
    *set = signal_bit(6).to_le_bytes();
    let mut old = GuestBytes::<8>::new();
    assert_eq!(
        kernel.dispatch(
            number::RT_SIGPROCMASK,
            Arguments::new([
                0,
                set.as_mut_ptr() as usize as i64,
                old.as_mut_ptr() as usize as i64,
                8,
                0,
                0
            ])
        ),
        Outcome::Done(0)
    );
    assert_eq!(kernel.dispatch(number::TGKILL, raise(6)), Outcome::Done(0));

    // A signal whose default action is to be ignored never ends anything.
    assert_eq!(kernel.dispatch(number::TGKILL, raise(28)), Outcome::Done(0));

    // A thread that does not exist, and a signal number that is not one.
    assert_eq!(
        kernel.dispatch(number::TGKILL, Arguments::new([1, 2, 6, 0, 0, 0])),
        Outcome::Done(Errno::NoProcess.as_result())
    );
    assert_eq!(
        kernel.dispatch(number::TGKILL, raise(0)),
        Outcome::Done(Errno::Invalid.as_result())
    );
    assert_eq!(
        kernel.dispatch(number::TGKILL, raise(65)),
        Outcome::Done(Errno::Invalid.as_result())
    );
}

fn signal_bit(signal: i64) -> u64 {
    1u64 << (signal - 1)
}

/// `statfs` says what filesystem this is and how much room is on it, and
/// both are branched on: `ls` reads the type to decide whether `d_type` can
/// be trusted, and anything that writes reads the free count first.
///
/// The type is `OVERLAYFS_SUPER_MAGIC` because a container's root *is* a
/// read-only image with a writable layer over it. Saying `ext4` would be a
/// claim that unlinking a file in the lower layer frees space, and it does
/// not.
#[test]
fn statfs_describes_the_overlay_and_the_room_left() {
    /// `OVERLAYFS_SUPER_MAGIC`.
    const OVERLAY: u64 = 0x794c_7630;
    let mut kernel = Kernel::new(Recording::default(), Registers::default(), empty_image());
    let buffer = GuestBytes::<120>::new();
    let path = GuestBuffer::of(b"/\0");
    assert_eq!(
        kernel.dispatch(
            number::STATFS,
            Arguments::new([path.address(), buffer.address(), 0, 0, 0, 0])
        ),
        Outcome::Done(0)
    );
    let field = |at: usize| u64::from_le_bytes(buffer[at..at + 8].try_into().expect("eight bytes"));
    assert_eq!(field(0), OVERLAY, "f_type");
    assert_eq!(field(8), 4096, "f_bsize");
    assert_eq!(field(64), 255, "f_namelen");
    assert_eq!(field(72), 4096, "f_frsize");
    // Free blocks, and the same number available to a caller with no
    // reservation — there is no reserve here for one to be exempt from.
    assert_eq!(field(24), field(32), "f_bfree and f_bavail");
    assert!(field(16) >= field(24), "more blocks than free ones");
    // `ST_VALID`, without which a caller ignores `f_flags` entirely.
    assert_eq!(field(80) & 0x20, 0x20, "ST_VALID in f_flags");
}

/// A path that is not there is `ENOENT`, and a descriptor that is not open
/// is `EBADF` — the two things `statfs` and `fstatfs` can tell apart, and
/// the whole reason they are separate calls here rather than one.
#[test]
fn statfs_refuses_what_is_not_there() {
    let mut kernel = Kernel::new(Recording::default(), Registers::default(), empty_image());
    let buffer = GuestBytes::<120>::new();
    let path = GuestBuffer::of(b"/nowhere\0");
    assert_eq!(
        kernel.dispatch(
            number::STATFS,
            Arguments::new([path.address(), buffer.address(), 0, 0, 0, 0])
        ),
        Outcome::Done(Errno::NoEntry.as_result())
    );
    assert_eq!(
        kernel.dispatch(
            number::FSTATFS,
            Arguments::new([9, buffer.address(), 0, 0, 0, 0])
        ),
        Outcome::Done(Errno::BadFile.as_result())
    );
}

/// `fadvise64` does nothing, and the checks are the whole of what a program
/// can observe about it: a call that cheerfully accepted a bad descriptor or
/// an unknown advice would hide a bug in whatever asked.
#[test]
fn fadvise_checks_what_it_cannot_act_on() {
    let mut kernel = Kernel::new(Recording::default(), Registers::default(), empty_image());
    let advise = |fd: i64, advice: i64| Arguments::new([fd, 0, 0, advice, 0, 0]);
    // Standard output is open, and `POSIX_FADV_SEQUENTIAL` is advice this
    // kernel knows and has nothing to do with.
    assert_eq!(kernel.dispatch(number::FADVISE64, advise(1, 2)), Outcome::Done(0));
    assert_eq!(
        kernel.dispatch(number::FADVISE64, advise(9, 2)),
        Outcome::Done(Errno::BadFile.as_result())
    );
    assert_eq!(
        kernel.dispatch(number::FADVISE64, advise(1, 6)),
        Outcome::Done(Errno::Invalid.as_result())
    );
    // And a pipe has no position to advise about.
    let ends = GuestBytes::<8>::new();
    assert_eq!(
        kernel.dispatch(number::PIPE, Arguments::new([ends.address(), 0, 0, 0, 0, 0])),
        Outcome::Done(0)
    );
    let reader = i32::from_le_bytes(ends[0..4].try_into().expect("four bytes"));
    assert_eq!(
        kernel.dispatch(number::FADVISE64, advise(i64::from(reader), 2)),
        Outcome::Done(Errno::NotSeekable.as_result())
    );
}

/// `PR_CAPBSET_READ`: every capability up to `CAP_LAST_CAP` is in the set
/// and everything past it is `EINVAL`, which is the whole of what a program
/// can check. Measured against this machine's kernel, where `cap_last_cap`
/// is 40 and 41 is refused.
#[test]
fn the_capability_bounding_set_is_whole() {
    /// `PR_CAPBSET_READ`.
    const CAPBSET_READ: i64 = 23;
    let mut kernel = Kernel::new(Recording::default(), Registers::default(), empty_image());
    let ask = |capability: i64| Arguments::new([CAPBSET_READ, capability, 0, 0, 0, 0]);
    for capability in [0, 21, 32, 40] {
        assert_eq!(
            kernel.dispatch(number::PRCTL, ask(capability)),
            Outcome::Done(1),
            "capability {capability} should be in the bounding set"
        );
    }
    for capability in [41, 63, -1] {
        assert_eq!(
            kernel.dispatch(number::PRCTL, ask(capability)),
            Outcome::Done(Errno::Invalid.as_result()),
            "capability {capability} is not a capability"
        );
    }
}
