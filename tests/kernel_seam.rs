//! M1: every seam this project adds, exercised once by one `write(2)`.
//!
//! The path under test is the whole vertical: a translated `syscall`, the
//! generated seam, kisal's dispatcher, an `ll-store` mount, the host. Nothing
//! else is built, which is the point — a failure anywhere in it can name its
//! layer instead of being a mystery in a system with a hundred moving parts.
//!
//! The oracle is not a golden file. The M1 guest is hand-written assembly
//! using a raw `syscall`, so the same `.s` runs natively on Linux; what the
//! transpiled run did is compared against what the kernel did with the same
//! instructions.

mod support;

use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use support::{
    ALL_MODES, WorkingDirectory, compile_corpus_object, compile_foreign_wasm_object,
    link_container, link_container_with_image, m1_mounts, print_wasm, seam_object,
    transpile_object, transpile_object_resumable, try_link_wasm, validate_wasm,
};

/// The bytes `guest_write` sends, and how many of them.
const MESSAGE: &[u8] = b"hello, courtyard\n";

/// Every way the guest can be built: both control-flow translations, with
/// checkpoint-resume on and off.
///
/// Resume is not incidental here. A translated `syscall` reserves a
/// return-address slot exactly as a `call` does, so with `--resume` on every
/// syscall site becomes a resume site — the property the whole scheduler
/// design rests on. Running the seam under both settings is what keeps that
/// claim honest before anything depends on it.
fn variants() -> Vec<(String, bool, zaqaru::structurer::Mode)> {
    let mut variants = Vec::new();
    for mode in ALL_MODES {
        for resume in [false, true] {
            variants.push((format!("{mode:?}/resume={resume}"), resume, mode));
        }
    }
    variants
}

fn build_container(
    workspace: &WorkingDirectory,
    source: &str,
    label: &str,
    resume: bool,
    mode: zaqaru::structurer::Mode,
) -> PathBuf {
    let native = compile_corpus_object(workspace, source);
    let guest = workspace
        .path()
        .join(format!("{source}.{label}.wasm.o").replace('/', "."));
    if resume {
        transpile_object_resumable(&native, &guest, mode);
    } else {
        transpile_object(&native, &guest, mode);
    }
    link_container(workspace, &[guest], &label.replace('/', "."))
}

fn instantiate(module: &PathBuf) -> runner::Container {
    let bytes = std::fs::read(module).expect("read the linked container");
    validate_wasm(&bytes);
    runner::Container::instantiate(&bytes, m1_mounts())
        .expect("the runner could not instantiate the container")
}

/// What the same `.s` does when the real kernel runs it: the oracle.
fn native_write(length: usize) -> (i64, Vec<u8>) {
    let workspace = WorkingDirectory::new("m1-native");
    let library = support::compile_corpus_shared_library(&workspace, &["syscall_write.s"]);
    let native = unsafe { libloading::Library::new(&library) }.expect("load the native guest");
    let guest_write: libloading::Symbol<unsafe extern "C" fn(i64, i64) -> i64> =
        unsafe { support::native_function(&native, "guest_write") };

    let target = workspace.path().join("written");
    let file = std::fs::File::create(&target).expect("create the oracle's destination");
    let returned = unsafe { guest_write(file.as_raw_fd() as i64, length as i64) };
    drop(file);
    let delivered = std::fs::read(&target).expect("read back what the oracle wrote");
    (returned, delivered)
}

#[test]
fn write_reaches_the_console_through_every_seam() {
    let (native_returned, native_delivered) = native_write(MESSAGE.len());
    assert_eq!(
        native_delivered, MESSAGE,
        "the oracle itself is wrong, which makes every comparison below meaningless"
    );

    let workspace = WorkingDirectory::new("m1-write");
    for (label, resume, mode) in variants() {
        let module = build_container(&workspace, "syscall_write.s", &label, resume, mode);
        let mut container = instantiate(&module);

        // Descriptor 1 on this side, a file on the oracle's: each caller
        // names its own destination, and what is compared is what the
        // syscall did with it.
        let returned = container
            .call_guest("guest_write", [1, MESSAGE.len() as i64, 0, 0, 0, 0])
            .unwrap_or_else(|error| panic!("[{label}] the guest trapped: {error:?}"));

        let delivered = container
            .mounts()
            .read(&path(&[b"iso", b"console", b"stdout"]))
            .unwrap_or_else(|error| panic!("[{label}] the console mount failed: {error}"))
            .unwrap_or_default();

        assert_eq!(
            returned, native_returned,
            "[{label}] the syscall returned {returned}, the kernel returns {native_returned}"
        );
        assert_eq!(
            delivered,
            native_delivered,
            "[{label}] delivered {:?}, the kernel delivers {:?}",
            String::from_utf8_lossy(&delivered),
            String::from_utf8_lossy(&native_delivered)
        );
    }
}

/// A short write, so that the length reaching the kernel is the guest's
/// argument rather than a constant that happens to be right.
#[test]
fn the_length_argument_survives_the_seam() {
    let (native_returned, native_delivered) = native_write(5);

    let workspace = WorkingDirectory::new("m1-partial");
    let module = build_container(
        &workspace,
        "syscall_write.s",
        "partial",
        false,
        zaqaru::structurer::Mode::Structured,
    );
    let mut container = instantiate(&module);
    let returned = container
        .call_guest("guest_write", [1, 5, 0, 0, 0, 0])
        .expect("the guest trapped");
    let delivered = container
        .mounts()
        .read(&path(&[b"iso", b"console", b"stdout"]))
        .expect("the console mount failed")
        .unwrap_or_default();

    assert_eq!(returned, native_returned);
    assert_eq!(delivered, native_delivered);
    assert_eq!(delivered, b"hello");
}

/// The other descriptor goes to the other mount path, which is the whole of
/// M1's resolution step — and the first row of the fd table M3 replaces it
/// with.
#[test]
fn descriptors_route_to_their_own_paths() {
    let workspace = WorkingDirectory::new("m1-stderr");
    let module = build_container(
        &workspace,
        "syscall_write.s",
        "stderr",
        false,
        zaqaru::structurer::Mode::Structured,
    );
    let mut container = instantiate(&module);
    container
        .call_guest("guest_write", [2, MESSAGE.len() as i64, 0, 0, 0, 0])
        .expect("the guest trapped");

    assert_eq!(
        container
            .mounts()
            .read(&path(&[b"iso", b"console", b"stderr"]))
            .expect("mount failed")
            .unwrap_or_default(),
        MESSAGE
    );
    assert!(
        container
            .mounts()
            .read(&path(&[b"iso", b"console", b"stdout"]))
            .expect("mount failed")
            .unwrap_or_default()
            .is_empty(),
        "stderr's bytes reached stdout"
    );
}

/// A descriptor with no backend is `EBADF`, not a fault: an fd that is not
/// open is an ordinary POSIX error, and the loud-error policy is about
/// syscalls kisal has not implemented, never about calls that legitimately
/// fail.
#[test]
fn an_unbacked_descriptor_is_an_ordinary_error() {
    let workspace = WorkingDirectory::new("m1-ebadf");
    let module = build_container(
        &workspace,
        "syscall_write.s",
        "ebadf",
        false,
        zaqaru::structurer::Mode::Structured,
    );
    let mut container = instantiate(&module);
    let returned = container
        .call_guest("guest_write", [7, MESSAGE.len() as i64, 0, 0, 0, 0])
        .expect("an EBADF must return, not trap");
    assert_eq!(returned, -9, "EBADF is -9 in `rax`");
}

/// The loud-error policy, tested rather than merely stated: an unimplemented
/// syscall names itself in the kernel log and stops the run.
#[test]
fn an_unimplemented_syscall_names_itself() {
    let workspace = WorkingDirectory::new("m1-unimplemented");
    let module = build_container(
        &workspace,
        "syscall_unimplemented.s",
        "unimplemented",
        false,
        zaqaru::structurer::Mode::Structured,
    );
    let mut container = instantiate(&module);
    let outcome = container.call_guest("guest_getpid", [0; 6]);
    assert!(
        outcome.is_err(),
        "an unimplemented syscall returned {outcome:?} instead of stopping the run"
    );

    let logged = container
        .mounts()
        .read(&path(&[b"iso", b"log", b"error"]))
        .expect("the log mount failed")
        .unwrap_or_default();
    let logged = String::from_utf8_lossy(&logged).into_owned();
    // The exact message, not a substring: `"390".contains("39")` is true, so a
    // substring check cannot tell a correct number from a wrong one.
    assert_eq!(
        logged,
        "kisal: unimplemented syscall getpid (39) with (0, 0, 0, 0, 0, 0)"
    );
}

/// All six syscall arguments arrive, in order, in the registers Linux uses.
///
/// The fourth is `%r10` rather than the `%rcx` a C call would use, because
/// the `syscall` instruction overwrites `%rcx` before the kernel sees it.
/// Nothing else in the suite passes more than three arguments, so without
/// this the seam could transpose the last three — or read `%rcx` for the
/// fourth — and every test would still pass.
#[test]
fn all_six_syscall_arguments_reach_the_kernel_in_order() {
    let workspace = WorkingDirectory::new("m1-sixargs");
    let module = build_container(
        &workspace,
        "syscall_unimplemented.s",
        "sixargs",
        false,
        zaqaru::structurer::Mode::Structured,
    );
    let mut container = instantiate(&module);
    let outcome = container.call_guest("guest_six_arguments", [0; 6]);
    assert!(
        outcome.is_err(),
        "an unimplemented syscall must stop the run"
    );

    let logged = container
        .mounts()
        .read(&path(&[b"iso", b"log", b"error"]))
        .expect("the log mount failed")
        .unwrap_or_default();
    // `%rcx` holds -1 at the syscall and must not appear: reading it for the
    // fourth argument is the mistake this exists to catch.
    assert_eq!(
        String::from_utf8_lossy(&logged),
        "kisal: unimplemented syscall getpid (39) with (11, 22, 33, 44, 55, 66)"
    );
}

/// The seam's value is that a disagreement about the kernel's shape is a
/// *link* error. Asserting the linker's complaint is the only way to know
/// that is still true.
#[test]
fn a_mistyped_kernel_dispatch_fails_at_link() {
    let workspace = WorkingDirectory::new("m1-mistyped");
    let seam = workspace.write("seam.wasm.o", seam_object());
    let impostor = compile_foreign_wasm_object(
        &workspace,
        "impostor",
        "long long kisal_syscall(long long number) { return number; }\n",
    );
    // `--fatal-warnings` is the flag the container recipe links with, and it
    // is what turns the mismatch into a refusal: `wasm-ld` on its own warns
    // and links. Testing the link without it would test a path nothing uses,
    // and would pass on the warning text alone.
    let outcome = try_link_wasm(
        &[seam, impostor],
        &workspace.path().join("mistyped.wasm"),
        &["--fatal-warnings"],
    );
    assert!(
        !outcome.succeeded,
        "wasm-ld accepted a kernel whose `kisal_syscall` takes one argument \
         instead of seven:\n{}",
        outcome.report()
    );
    assert!(
        outcome.mentions("signature mismatch") && outcome.mentions("kisal_syscall"),
        "the linker's complaint does not name a signature mismatch on the \
         seam:\n{}",
        outcome.report()
    );
}

/// The control for the test above: the same link, with a kernel of the right
/// shape, succeeds. Without it, "the link failed" proves nothing about *why*.
#[test]
fn a_correctly_typed_kernel_links() {
    let workspace = WorkingDirectory::new("m1-welltyped");
    let seam = workspace.write("seam.wasm.o", seam_object());
    let honest = compile_foreign_wasm_object(
        &workspace,
        "honest",
        "long long kisal_syscall(long long n, long long a, long long b, long long c,\n\
                                 long long d, long long e, long long f) {\n\
             return n + a + b + c + d + e + f;\n\
         }\n",
    );
    let outcome = try_link_wasm(
        &[seam, honest],
        &workspace.path().join("welltyped.wasm"),
        &["--fatal-warnings"],
    );
    assert!(
        outcome.succeeded,
        "a correctly-typed kernel was refused, so the test above proves \
         nothing about signatures:\n{}",
        outcome.report()
    );
}

/// The scheduler's machinery, linked five milestones before its first real
/// throw. What is proved here is toolchain-shaped, and stated narrowly on
/// purpose: the tag survives `wasm-ld` with its relocations intact, the
/// engine accepts the standardized `try_table` without a flag, the catch
/// reports the yield, and the register globals are what the throwing code
/// left.
///
/// What it does *not* prove: the thrower is the immediate callee of the
/// frame holding the catch, so no unwind crosses an intervening frame here,
/// and the only code between the load and the save is the `throw` itself —
/// which nothing could disturb. Unwinding a real chain of transpiled frames
/// is M7's first genuine block, and belongs to the milestone that creates
/// one rather than to a claim made in advance.
#[test]
fn the_yield_tag_survives_the_link_and_the_engine() {
    let workspace = WorkingDirectory::new("m1-yield");
    let module = build_container(
        &workspace,
        "syscall_write.s",
        "yield",
        false,
        zaqaru::structurer::Mode::Structured,
    );

    let text = print_wasm(&std::fs::read(&module).expect("read the container"));
    assert!(
        text.contains("(tag "),
        "the tag section did not survive the link"
    );
    assert!(
        text.contains("try_table"),
        "the standardized catch did not survive the link"
    );

    let mut container = instantiate(&module);

    // Put a value in every register, then run the throw under the catch. The
    // thrower is reached through the indirect table, so a frame with no
    // handler of its own sits between the throw and the catch.
    let image = container
        .allocate_transfer(zaqaru::seam::machine_image::SIZE, 8)
        .expect("allocate");
    let pattern = machine_pattern();
    container
        .write_memory(image, &pattern)
        .expect("write the image");
    container
        .call::<u32, ()>("x86_load_machine", image)
        .expect("load the machine image");

    let slot: i32 = container.call("x86_yield_slot", ()).expect("read the slot");
    let outcome: i32 = container
        .call("x86_run_thread", slot)
        .expect("the throw escaped its catch");
    assert_eq!(outcome, 1, "`x86_run_thread` did not report a yield");

    let after = container
        .allocate_transfer(zaqaru::seam::machine_image::SIZE, 8)
        .expect("allocate");
    container
        .call::<u32, ()>("x86_save_machine", after)
        .expect("save the machine image");
    let saved = container
        .read_memory(after, pattern.len())
        .expect("read the saved image");
    assert_eq!(
        saved, pattern,
        "the unwind disturbed the register file it is supposed to leave alone"
    );
}

/// `x86_run_thread` reports the other outcome too: a continuation that runs
/// off the end of its chain rather than yielding.
///
/// The continuation is installed by the host into a slot of its own, which is
/// exactly how a thread is started from outside the module — and the reason
/// the container link exports its indirect function table.
#[test]
fn a_continuation_that_returns_reports_no_yield() {
    let workspace = WorkingDirectory::new("m1-noyield");
    let module = build_container(
        &workspace,
        "syscall_write.s",
        "noyield",
        false,
        zaqaru::structurer::Mode::Structured,
    );
    let mut container = instantiate(&module);
    let slot = container
        .install_continuation()
        .expect("install a continuation into the function table");
    let outcome: i32 = container
        .call("x86_run_thread", slot)
        .expect("the continuation trapped");
    assert_eq!(
        outcome, 0,
        "`x86_run_thread` reported a yield for a continuation that returned"
    );
}

/// The register file survives a round trip through linear memory, cell for
/// cell. A cell either helper forgets shows up as a mismatch, because the
/// pattern makes every cell distinguishable from every other and from zero.
/// The layout itself, against literals written from the model rather than
/// from the constants the generator uses.
///
/// The round-trip test below cannot do this job: it builds its expected image
/// out of the same constants `build_machine_image` walks, so a cell moved to
/// overlap another is reproduced identically on both sides and stays
/// invisible. Seventeen eight-byte registers, thirty-two eight-byte XMM
/// halves, six four-byte flags, each region starting where the previous one
/// ends.
#[test]
fn the_machine_image_layout_is_what_the_model_says() {
    use zaqaru::seam::machine_image;
    assert_eq!(machine_image::REGISTER_OFFSET, 0);
    assert_eq!(machine_image::SEGMENT_BASE_OFFSET, 16 * 8);
    assert_eq!(machine_image::VECTOR_OFFSET, 17 * 8);
    assert_eq!(machine_image::FLAG_OFFSET, 17 * 8 + 32 * 8);
    assert_eq!(machine_image::SIZE, 17 * 8 + 32 * 8 + 6 * 4);
    assert_eq!(machine_image::SIZE, 416);
    assert_eq!(
        machine_image::SEGMENT_BASE_OFFSET % 8,
        0,
        "every eight-byte cell must be eight-byte aligned"
    );
}

/// The one number the kernel and the generated seam both have to know.
///
/// The kernel cannot depend on the generator — it is the thing that runs
/// inside the module — so the sentinel that means "this thread is leaving"
/// is written on both sides. Everywhere else a disagreement between the two
/// is a link error, because it is a signature; here it would be silent, and
/// the failure would be a syscall result the seam mistook for an exit. So it
/// is a test instead.
#[test]
fn the_leave_sentinel_agrees_across_the_seam() {
    assert_eq!(
        zaqaru::seam::LEAVE,
        kisal::LEAVE,
        "the seam would throw on a value the kernel never sends, or miss the \
         one it does"
    );
}

#[test]
fn the_machine_image_round_trips() {
    let workspace = WorkingDirectory::new("m1-image");
    let module = build_container(
        &workspace,
        "syscall_write.s",
        "image",
        false,
        zaqaru::structurer::Mode::Structured,
    );
    let mut container = instantiate(&module);

    let pattern = machine_pattern();
    let source = container
        .allocate_transfer(pattern.len() as u32, 8)
        .expect("allocate");
    let sink = container
        .allocate_transfer(pattern.len() as u32, 8)
        .expect("allocate");
    container.write_memory(source, &pattern).expect("write");
    container
        .call::<u32, ()>("x86_load_machine", source)
        .expect("load");
    container
        .call::<u32, ()>("x86_save_machine", sink)
        .expect("save");
    assert_eq!(
        container.read_memory(sink, pattern.len()).expect("read"),
        pattern
    );
}

/// The save helper reads the *right* globals, not merely a symmetric set:
/// after a `write(2)`, the image holds what the syscall left in `rax` and
/// what the guest put in the argument registers.
#[test]
fn a_saved_image_holds_what_the_guest_left_behind() {
    let workspace = WorkingDirectory::new("m1-observed");
    let module = build_container(
        &workspace,
        "syscall_write.s",
        "observed",
        false,
        zaqaru::structurer::Mode::Structured,
    );
    let mut container = instantiate(&module);
    container
        .call_guest("guest_write", [1, MESSAGE.len() as i64, 0, 0, 0, 0])
        .expect("the guest trapped");

    let image = container
        .allocate_transfer(zaqaru::seam::machine_image::SIZE, 8)
        .expect("allocate");
    container
        .call::<u32, ()>("x86_save_machine", image)
        .expect("save");
    let saved = container
        .read_memory(image, zaqaru::seam::machine_image::SIZE as usize)
        .expect("read");

    assert_eq!(
        register(&saved, 0),
        MESSAGE.len() as i64,
        "`rax` does not hold the write's result"
    );
    assert_eq!(register(&saved, 7), 1, "`rdi` does not hold the descriptor");
    assert_eq!(
        register(&saved, 2),
        MESSAGE.len() as i64,
        "`rdx` does not hold the length"
    );
    // The hardware destroys both, and the seam makes that deterministic.
    assert_eq!(register(&saved, 1), 0, "`rcx` was not clobbered");
    assert_eq!(register(&saved, 11), 0, "`r11` was not clobbered");
}

/// A guest that issues no syscall must not acquire the seam. The import is
/// declared from an instruction scan, and a scan that fires on the wrong
/// instruction would tie every object to a kernel it does not need.
#[test]
fn an_object_without_a_syscall_does_not_import_the_seam() {
    let workspace = WorkingDirectory::new("m1-noseam");
    let native = compile_corpus_object(&workspace, "add.c");
    let guest = workspace.path().join("add.wasm.o");
    transpile_object(&native, &guest, zaqaru::structurer::Mode::Structured);
    let text = print_wasm(&std::fs::read(&guest).expect("read the object"));
    assert!(
        !text.contains("x86_syscall"),
        "an object with no `syscall` imported the kernel seam:\n{text}"
    );
}

fn path(segments: &[&[u8]]) -> Vec<Vec<u8>> {
    segments.iter().map(|segment| segment.to_vec()).collect()
}

fn register(image: &[u8], number: usize) -> i64 {
    let offset = zaqaru::seam::machine_image::REGISTER_OFFSET as usize + number * 8;
    i64::from_le_bytes(image[offset..offset + 8].try_into().expect("eight bytes"))
}

/// A distinct value in every cell of the machine image, so that a helper
/// skipping one is a mismatch rather than a coincidence.
///
/// Flags are four bytes and the rest are eight, so the pattern is written
/// through the same layout the helpers walk rather than as a flat fill.
fn machine_pattern() -> Vec<u8> {
    use zaqaru::seam::machine_image;
    let mut image = vec![0u8; machine_image::SIZE as usize];
    assert_eq!(
        zaqaru::machine::REGISTER_NAMES.len(),
        16,
        "the pattern below enumerates the register file by hand; a cell it \
         does not reach stays zero, and a helper that drops that cell would \
         round-trip anyway"
    );
    assert_eq!(zaqaru::machine::VECTOR_REGISTER_COUNT, 16);
    assert_eq!(zaqaru::machine::Flag::ALL.len(), 6);
    for number in 0..16usize {
        let value = 0x0101_0000_0000_0000u64 | (number as u64 + 1);
        let offset = machine_image::REGISTER_OFFSET as usize + number * 8;
        image[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    {
        let offset = machine_image::SEGMENT_BASE_OFFSET as usize;
        image[offset..offset + 8].copy_from_slice(&0x0404_0000_0000_0001u64.to_le_bytes());
    }
    for number in 0..16usize {
        for half in 0..2usize {
            let value = 0x0202_0000_0000_0000u64 | ((number * 2 + half) as u64 + 1);
            let offset = machine_image::VECTOR_OFFSET as usize + number * 16 + half * 8;
            image[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
    }
    for index in 0..5usize {
        let value = 0x0300_0000u32 | (index as u32 + 1);
        let offset = machine_image::FLAG_OFFSET as usize + index * 4;
        image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    image
}

/// The 128 bytes below `%rsp` survive a syscall, as they do on Linux.
///
/// A `syscall` is not a `call`: the SysV ABI lets a callee destroy the red
/// zone, and the kernel does not. Compilers rely on the difference — gcc at
/// `-O2` keeps a leaf function's locals there across an inline `syscall`
/// without ever moving `%rsp` — so anything the seam spends below the guest's
/// stack pointer is silent corruption of live data.
#[test]
fn the_red_zone_survives_a_syscall() {
    let workspace = WorkingDirectory::new("m1-redzone");
    let library = support::compile_corpus_shared_library(&workspace, &["syscall_red_zone.s"]);
    let native = unsafe { libloading::Library::new(&library) }.expect("load the native guest");
    let guest_red_zone: libloading::Symbol<unsafe extern "C" fn(i64, i64) -> i64> =
        unsafe { support::native_function(&native, "guest_red_zone") };
    let target = workspace.path().join("written");
    let file = std::fs::File::create(&target).expect("create the oracle's destination");
    let native_survivors = unsafe { guest_red_zone(file.as_raw_fd() as i64, 5) };
    drop(file);
    assert_eq!(
        native_survivors, 16,
        "the real kernel clobbered the red zone, so this oracle is not one"
    );

    for (label, resume, mode) in variants() {
        let module = build_container(&workspace, "syscall_red_zone.s", &label, resume, mode);
        let mut container = instantiate(&module);
        let survivors = container
            .call_guest("guest_red_zone", [1, 5, 0, 0, 0, 0])
            .unwrap_or_else(|error| panic!("[{label}] the guest trapped: {error:?}"));
        assert_eq!(
            survivors, native_survivors,
            "[{label}] {} of 16 red-zone quadwords survived the syscall; the \
             kernel preserves all 16",
            survivors
        );
    }
}

/// A count whose top half would be lost casting to `usize` inside the module
/// is refused, not truncated.
///
/// Measured before the bounds check existed: this returned 4294967296 —
/// claiming four gigabytes written — and delivered nothing. Any libc retry
/// loop (`while (n < total) n += write(...)`) believes that number.
#[test]
fn a_count_that_would_truncate_is_refused_not_truncated() {
    let workspace = WorkingDirectory::new("m1-truncate");
    let module = build_container(
        &workspace,
        "syscall_write.s",
        "truncate",
        false,
        zaqaru::structurer::Mode::Structured,
    );
    let mut container = instantiate(&module);
    let returned = container
        .call_guest("guest_write", [1, 0x1_0000_0005, 0, 0, 0, 0])
        .expect("an out-of-range count must return, not kill the instance");
    assert_eq!(returned, -14, "EFAULT is -14 in `rax`");
    assert!(
        container
            .mounts()
            .read(&path(&[b"iso", b"console", b"stdout"]))
            .expect("mount failed")
            .unwrap_or_default()
            .is_empty(),
        "the refused write delivered bytes anyway"
    );
}

/// An address past the end of linear memory fails one syscall, where an
/// unchecked dereference would trap and take every thread in the instance
/// with it.
#[test]
fn an_address_past_linear_memory_fails_the_call_not_the_instance() {
    let workspace = WorkingDirectory::new("m1-oob");
    let module = build_container(
        &workspace,
        "syscall_write.s",
        "oob",
        false,
        zaqaru::structurer::Mode::Structured,
    );
    let mut container = instantiate(&module);
    let returned = container
        .call_guest("guest_write", [1, 0xffff_ff00, 0, 0, 0, 0])
        .expect("an out-of-range buffer must return, not trap");
    assert_eq!(returned, -14);

    // The instance is still usable, which is the whole point.
    let returned = container
        .call_guest("guest_write", [1, MESSAGE.len() as i64, 0, 0, 0, 0])
        .expect("the instance died with the failed syscall");
    assert_eq!(returned, MESSAGE.len() as i64);
}

// ---- M2: the `%fs` base ----------------------------------------------------

/// Thread-local storage, differentially, across every way the guest can be
/// built — including promotion off, because `x86_fs_base` is a new cell in
/// the machine model and a cell that behaves differently in a local than in
/// its global is a promotion bug.
///
/// The guest is self-restoring: the native oracle runs in this process, so
/// leaving `%fs` pointing at a scratch buffer would destroy the harness's own
/// thread-local storage.
#[test]
fn the_segment_base_matches_native_in_every_build() {
    const VALUE: i64 = 0x1234_5678;

    let workspace = WorkingDirectory::new("m2-fs");
    let library = support::compile_corpus_shared_library(&workspace, &["segment_base.s"]);
    let native_library = unsafe { libloading::Library::new(&library) }.expect("load native");
    let guest: libloading::Symbol<unsafe extern "C" fn(i64) -> i64> =
        unsafe { support::native_function(&native_library, "guest_segment_base") };
    let native = unsafe { guest(VALUE) };
    assert_eq!(
        native,
        VALUE + 1,
        "the oracle itself is wrong (-1 = the kernel disagreed about the base, \
         -2 = `lea` picked up the segment)"
    );

    for mode in ALL_MODES {
        for promote in [true, false] {
            let options = support::TranspileOptions {
                mode,
                promote,
                resume: false,
            };
            let label = options.label().replace('/', ".");
            let native_object = compile_corpus_object(&workspace, "segment_base.s");
            let object = workspace
                .path()
                .join(format!("segment_base.{label}.wasm.o"));
            support::transpile_object_configured(&native_object, &object, options);
            let module = link_container(&workspace, &[object], &label);
            let mut container = instantiate(&module);

            let returned = container
                .call_guest("guest_segment_base", [VALUE, 0, 0, 0, 0, 0])
                .unwrap_or_else(|error| panic!("[{label}] the guest trapped: {error:?}"));
            assert_eq!(
                returned, native,
                "[{label}] returned {returned}, native returns {native}"
            );

            // The guest's own return value already proves the round trip: it
            // is the value read back through `%fs:16` after being written
            // through `%fs:8` and incremented, and it is -1 if `arch_prctl`
            // disagreed about the base or -2 if `lea` picked the segment up.
        }
    }
}

/// `%gs` is a named translation error, not an approximation — including on
/// the relaxed global-offset-table path, which returns a symbol's address
/// without ever building an effective address and so is the one place a
/// prefix could slip through unexamined.
#[test]
fn a_gs_prefix_is_a_named_translation_error() {
    let workspace = WorkingDirectory::new("m2-gs");
    for (name, source) in [
        (
            "load",
            "\t.text\n\t.globl gs_load\n\t.type gs_load, @function\ngs_load:\n\
             \tmovq %gs:8, %rax\n\tret\n\t.size gs_load, .-gs_load\n",
        ),
        (
            "table",
            "\t.text\n\t.globl gs_table\n\t.type gs_table, @function\ngs_table:\n\
             \tmovq %gs:elsewhere@GOTPCREL(%rip), %rax\n\tret\n\
             \t.size gs_table, .-gs_table\n",
        ),
    ] {
        let assembly = workspace.write(&format!("gs_{name}.s"), source);
        let object = workspace.path().join(format!("gs_{name}.o"));
        let outcome = support::run_tool_capturing(
            "gcc",
            &[
                "-c",
                &assembly.to_string_lossy(),
                "-o",
                &object.to_string_lossy(),
            ],
        );
        assert!(
            outcome.succeeded,
            "assembling the {name} case: {}",
            outcome.report()
        );

        let error = support::try_transpile_object(&object, zaqaru::structurer::Mode::Structured)
            .expect_err("a `%gs` operand must be refused, not approximated");
        let rendered = format!("{error:?}");
        assert!(
            rendered.contains("GS"),
            "the {name} case's error does not name the prefix: {rendered}"
        );
    }
}

/// The flags a guest set before a syscall reach the globals, where the kernel
/// snapshots a blocking thread's register file from.
///
/// Flags are exempt from the flush at an ordinary call because the SysV ABI
/// makes them call-clobbered. A `syscall` has no such exemption, and the
/// difference is invisible until a context switch restores the wrong ones.
#[test]
fn flags_set_before_a_syscall_are_visible_to_the_kernel() {
    use zaqaru::seam::machine_image;

    let workspace = WorkingDirectory::new("m1-flags");
    let module = build_container(
        &workspace,
        "syscall_write.s",
        "flags",
        false,
        zaqaru::structurer::Mode::Structured,
    );
    let mut container = instantiate(&module);
    container
        .call_guest(
            "guest_flags_before_syscall",
            [1, MESSAGE.len() as i64, 0, 0, 0, 0],
        )
        .expect("the guest trapped");

    let image = container
        .allocate_transfer(machine_image::SIZE, 8)
        .expect("allocate");
    container
        .call::<u32, ()>("x86_save_machine", image)
        .expect("save");
    let saved = container
        .read_memory(image, machine_image::SIZE as usize)
        .expect("read");
    let flag = |index: usize| {
        let offset = machine_image::FLAG_OFFSET as usize + index * 4;
        i32::from_le_bytes(saved[offset..offset + 4].try_into().expect("four bytes"))
    };

    // `Flag::ALL` order: zero, sign, carry, overflow, parity.
    assert_eq!(flag(0), 0, "zero flag");
    assert_eq!(flag(1), 1, "sign flag — `0 - 1` is negative");
    assert_eq!(flag(2), 1, "carry flag — `0 - 1` borrows");
    assert_eq!(flag(3), 0, "overflow flag");
}

/// A container with nowhere to send a kernel complaint is refused at boot.
///
/// Without the mount the loud-error policy is silently mute: the write fails,
/// the kernel aborts, and `panic = abort` inside a wasm module has no stderr
/// to say why. The whole doctrine rests on a channel, so its absence is a
/// configuration error rather than a surprise at fault time.
#[test]
fn a_container_without_a_kernel_log_is_refused_at_boot() {
    let workspace = WorkingDirectory::new("m1-nolog");
    let module = build_container(
        &workspace,
        "syscall_write.s",
        "nolog",
        false,
        zaqaru::structurer::Mode::Structured,
    );
    let bytes = std::fs::read(&module).expect("read the container");

    let mut mounts = runner::store::MountTable::new();
    mounts.mount(&[b"iso", b"console"], Box::new(runner::store::Sink::new()));
    let error = runner::Container::instantiate(&bytes, mounts)
        .err()
        .expect("a container with no kernel log must be refused");
    assert!(
        format!("{error:?}").contains("/iso/log"),
        "the refusal does not name the missing mount: {error:?}"
    );
}

/// The transfer arena's lifetime is one syscall, and this pins it.
///
/// It is not a limitation to work around — it is what stops the `ll-store`
/// boundary leaking, since the host allocates through it on every read and
/// nothing is ever freed back across. What it *is* is a trap for a caller who
/// assumes otherwise: a path placed here once came back as `"iso"`, the first
/// segment of the console mount's result path, and every path the guest built
/// from it was wrong from that point on with nothing failing.
///
/// Nothing in the runtime places data here and expects it to last. This test
/// exists so that the day something tries, the behaviour is written down
/// rather than discovered.
#[test]
fn the_transfer_arena_lasts_one_syscall() {
    let workspace = WorkingDirectory::new("m1-arena");
    let module = build_container(
        &workspace,
        "syscall_write.s",
        "arena",
        false,
        zaqaru::structurer::Mode::Structured,
    );
    let mut container = instantiate(&module);

    let placed = container.allocate_transfer(16, 8).expect("allocate");
    container
        .write_memory(placed, b"before-a-syscall")
        .expect("write");
    assert_eq!(
        container.read_memory(placed, 16).expect("read"),
        b"before-a-syscall",
        "the bytes are there until a syscall happens"
    );

    container
        .call_guest("guest_write", [1, MESSAGE.len() as i64, 0, 0, 0, 0])
        .expect("the guest trapped");

    assert_ne!(
        container.read_memory(placed, 16).expect("read"),
        b"before-a-syscall",
        "the arena outlived a syscall, so its documented lifetime is wrong \
         and every caller reasoning about it is reasoning about the wrong \
         thing"
    );
}

/// The stack pointer is balanced across a syscall even in a function whose
/// own instructions never write it.
///
/// The reservations a `syscall` and a call site make are the *translation's*,
/// not the guest's: `iced` reports `syscall` as writing `rcx` and `r11` and a
/// tail `jmp` as writing nothing, so a leaf that reads `%rsp` and leaves by
/// tail jump used to have its promoted `%rsp` updated by the reservation and
/// never published. The guest's stack pointer came back shifted by 136 bytes,
/// permanently, with the resume-ID slot written 136 bytes from where the
/// driver looks for it — and nothing said so.
#[test]
fn the_stack_pointer_is_balanced_in_a_leaf_that_tail_jumps() {
    let workspace = WorkingDirectory::new("m1-leaf");
    for (label, resume, mode) in variants() {
        let module = build_container(&workspace, "syscall_leaf.s", &label, resume, mode);
        let mut container = instantiate(&module);
        let drift = container
            .call_guest("guest_leaf_tail", [1, 5, 0, 0, 0, 0])
            .unwrap_or_else(|error| panic!("[{label}] the guest trapped: {error:?}"));
        assert_eq!(
            drift, 0,
            "[{label}] `%rsp` moved {drift} bytes across a syscall in a \
             function that never writes it"
        );
        assert_eq!(
            container
                .mounts()
                .read(&path(&[b"iso", b"console", b"stdout"]))
                .expect("mount")
                .unwrap_or_default(),
            b"leaf\n",
            "[{label}] the syscall itself did not happen"
        );
    }
}

/// Standard input, all the way down: guest `read(0, …)` → kisal → the
/// `ll-store` import → the runner's mount table → back through the return
/// area into guest memory.
///
/// This is the only path in the system that crosses the store boundary
/// *downward*. Every filesystem row answers out of the baked image and every
/// console write goes the other way, so without a guest that reads, the
/// runner's `ll_read` closure — its return-area decoding, its `place` into
/// guest memory, and all three of its `some`/`none`/error arms — is code no
/// test executes. It was, until this.
#[test]
fn standard_input_crosses_the_store_boundary() {
    let workspace = WorkingDirectory::new("m3-console");
    let image = baker::object::empty().expect("an empty image");
    for (label, resume, mode) in variants() {
        let native = support::compile_corpus_object_with(
            &workspace,
            "console.c",
            support::Compiler::Gcc,
            support::CodeModel::PositionIndependent,
            "-O1",
        );
        let guest = workspace
            .path()
            .join(format!("console.{label}.wasm.o").replace('/', "."));
        let options = support::TranspileOptions {
            mode,
            promote: true,
            resume,
        };
        support::transpile_object_configured(&native, &guest, options);
        let module =
            link_container_with_image(&workspace, &[guest], &image, &label.replace('/', "."));
        let bytes = std::fs::read(&module).expect("read the container");
        validate_wasm(&bytes);

        let mut mounts = m1_mounts();
        mounts
            .write(
                &path(&[b"iso", b"console", b"stdin"]),
                b"typed by a human\n",
            )
            .expect("seed standard input");
        let mut container = runner::Container::instantiate(&bytes, mounts).expect("instantiate");
        container
            .call_guest("guest_console", [1, 0, 0, 0, 0, 0])
            .unwrap_or_else(|error| panic!("[{label}] the guest trapped: {error:?}"));

        let written = container
            .mounts()
            .read(&path(&[b"iso", b"console", b"stdout"]))
            .expect("mount")
            .unwrap_or_default();
        assert!(
            written.len() > 32,
            "[{label}] the guest wrote {} bytes, so it did not run",
            written.len()
        );
        // The bytes came back in two reads, split where the guest asked for
        // eight and then for the rest.
        assert_eq!(&written[..17], b"typed by a human\n", "[{label}]");
        let report = &written[17..17 + 32];
        let word = |index: usize| {
            i64::from_le_bytes(report[index * 8..index * 8 + 8].try_into().expect("8"))
        };
        assert_eq!(word(0), 8, "[{label}] the first read asked for eight");
        assert_eq!(word(1), 9, "[{label}] and the second took the rest");
        assert_eq!(word(2), 0, "[{label}] then end of input");
        assert_eq!(word(3), -29, "[{label}] a console stream is not seekable");
    }
}

/// The other arm of the same import: a store that has nothing at the path.
/// `ok(none)` has to arrive as end-of-input rather than as an error or as
/// whatever bytes happened to be in the return area.
#[test]
fn an_absent_standard_input_is_end_of_input() {
    let workspace = WorkingDirectory::new("m3-console-absent");
    let image = baker::object::empty().expect("an empty image");
    let native = support::compile_corpus_object_with(
        &workspace,
        "console.c",
        support::Compiler::Gcc,
        support::CodeModel::PositionIndependent,
        "-O1",
    );
    let guest = workspace.path().join("console.absent.wasm.o");
    support::transpile_object_configured(
        &native,
        &guest,
        support::TranspileOptions::new(zaqaru::structurer::Mode::Structured),
    );
    let module = link_container_with_image(&workspace, &[guest], &image, "absent");
    let bytes = std::fs::read(&module).expect("read the container");
    // Nothing is ever written to `/iso/console/stdin`, so the mount answers
    // `none` — the mount exists, and the value does not.
    let mut container = runner::Container::instantiate(&bytes, m1_mounts()).expect("instantiate");
    container
        .call_guest("guest_console", [1, 0, 0, 0, 0, 0])
        .expect("the guest trapped");
    let written = container
        .mounts()
        .read(&path(&[b"iso", b"console", b"stdout"]))
        .expect("mount")
        .unwrap_or_default();
    assert_eq!(written.len(), 32, "only the report, and no input bytes");
    let word =
        |index: usize| i64::from_le_bytes(written[index * 8..index * 8 + 8].try_into().expect("8"));
    assert_eq!([word(0), word(1), word(2)], [0, 0, 0]);
}

/// The guard below the kernel stack is real: a kernel that overruns its
/// stack traps, rather than writing into whatever the linker placed below it.
///
/// The kernel's stack is a fixed region in a system with no faults, so this
/// check is the only thing between an overrun and silent corruption. The
/// overrun is produced the way a real one would happen — a kernel frame
/// bigger than the region — by linking the seam against a stand-in
/// `kisal_syscall` that asks for 66 KiB and writes into it. The control is
/// the same link with a modest frame, which must *not* trap: without it,
/// "it trapped" would not distinguish the guard from any other reason.
#[test]
fn a_kernel_that_overruns_its_stack_traps() {
    const GREEDY: &str = "\
long long kisal_syscall(long long n, long long a, long long b, long long c,\n\
                        long long d, long long e, long long f) {\n\
    volatile char frame[66 * 1024];\n\
    for (int i = 0; i < 2048; i++) { frame[i] = (char)i; }\n\
    return frame[0] + n + a + b + c + d + e + f;\n\
}\n";
    const MODEST: &str = "\
long long kisal_syscall(long long n, long long a, long long b, long long c,\n\
                        long long d, long long e, long long f) {\n\
    volatile char frame[256];\n\
    for (int i = 0; i < 256; i++) { frame[i] = (char)i; }\n\
    return frame[0] + n + a + b + c + d + e + f;\n\
}\n";

    let workspace = WorkingDirectory::new("m1-overrun");
    for (label, source, overruns) in [("modest", MODEST, false), ("greedy", GREEDY, true)] {
        let seam = workspace.write(&format!("seam.{label}.wasm.o"), seam_object());
        let kernel = support::compile_foreign_wasm_object(&workspace, label, source);
        let linked = workspace.path().join(format!("{label}.wasm"));
        let outcome = support::try_link_wasm(
            &[seam, kernel],
            &linked,
            &["--fatal-warnings", "--export=x86_syscall"],
        );
        assert!(
            outcome.succeeded,
            "[{label}] link failed:\n{}",
            outcome.report()
        );

        // Instantiated directly rather than through `runner::Container`:
        // this module is the seam and a stand-in kernel, with none of the
        // exports a real container carries.
        let engine = wasmtime::Engine::default();
        let module = wasmtime::Module::from_file(&engine, &linked).expect("module");
        let mut store = wasmtime::Store::new(&engine, ());
        let instance = wasmtime::Instance::new(&mut store, &module, &[]).expect("instantiate");
        let entry = instance
            .get_typed_func::<(), ()>(&mut store, "x86_syscall")
            .expect("x86_syscall");
        let result = entry.call(&mut store, ());
        if overruns {
            let error = result.expect_err("a kernel that wrote below its stack was not noticed");
            let text = format!("{error:?}");
            assert!(
                text.contains("unreachable"),
                "[{label}] the trap does not name what happened: {text}"
            );
        } else {
            result.unwrap_or_else(|error| {
                panic!("[{label}] an ordinary kernel frame tripped the guard: {error:?}")
            });
        }
    }
}

/// The sentinel is planted before the kernel runs, and it is planted in the
/// container the runner actually boots — not only in the seam's own object.
///
/// A check against a value nothing writes would pass forever, so this looks
/// for the pattern in the live instance's memory after a real syscall.
///
/// The kernel's stack is a fixed region in a system with no faults, so the
/// only thing standing between an overrun and silent corruption of whatever
/// the linker placed below it is this check. A claim like that is worth
/// exactly as much as a demonstration that it fires, so the sentinel is
/// found in the container's own memory and clobbered from the host — which
/// is what an overrun would do, without needing a kernel deep enough to do
/// it.
#[test]
fn the_kernel_stack_guard_is_planted_in_a_real_container() {
    let workspace = WorkingDirectory::new("m1-guard");
    let module = build_container(
        &workspace,
        "syscall_write.s",
        "guard",
        false,
        zaqaru::structurer::Mode::Structured,
    );
    let mut container = instantiate(&module);
    // One syscall, so the guard is planted and the run is known good.
    container
        .call_guest("guest_write", [1, 5, 0, 0, 0, 0])
        .expect("the first call trapped");

    // Find the sentinel the seam plants. It is a distinctive eight bytes and
    // nothing else in the module writes it.
    let sentinel: [u8; 8] = 0x4b49_5341_4c47_5244i64.to_le_bytes();
    let size = container.memory_size().expect("memory size");
    let memory = container
        .read_memory(0, size)
        .expect("read the container's memory");
    let planted = memory
        .windows(8)
        .filter(|window| *window == sentinel)
        .count();
    assert_eq!(
        planted,
        4096 / 256,
        "the guard is not filled, so the check compares against nothing"
    );

    // Clobbering it from outside is *not* a trap, and that is the design
    // rather than a gap: the sentinel is planted on the way into every
    // syscall, so the next entry repairs it. The bracket detects an overrun
    // that happens while the kernel is running, which is the only moment one
    // can happen — asserting a trap here would be asserting the opposite of
    // what the code does.
    container
        .write_memory(at_of(&memory, &sentinel) as u32, &[0; 8])
        .expect("clobber the guard");
    container
        .call_guest("guest_write", [1, 5, 0, 0, 0, 0])
        .expect("the repaired guard must not trap");
}

fn at_of(memory: &[u8], pattern: &[u8; 8]) -> usize {
    memory
        .windows(8)
        .position(|window| window == pattern)
        .expect("the guard sentinel is not in memory, so nothing planted it")
}

/// The synthetic `/dev`, inside a container.
///
/// The mount is built by the kernel at boot, in the module's own memory, and
/// attached over the directory the image provides. Natively that is a `Vec`
/// and a leak; inside the module it is the guest's allocator, its linear
/// memory, and a 32-bit `usize`. Everything about `/dev` is asserted
/// natively as well — this is what says the two agree, which is the only
/// thing a native test of a wasm kernel cannot say.
#[test]
fn the_synthetic_devices_work_inside_a_container() {
    let workspace = WorkingDirectory::new("m4-devices");
    // An image with the mount points a base image ships, and nothing else.
    let root = workspace.path().join("tree");
    std::fs::create_dir_all(root.join("dev")).expect("mkdir");
    std::fs::create_dir_all(root.join("proc")).expect("mkdir");
    let image = baker::object::emit(&baker::bake_directory(&root).expect("bake"))
        .expect("emit the image object");

    let native = support::compile_corpus_object_with(
        &workspace,
        "devices.c",
        support::Compiler::Gcc,
        support::CodeModel::PositionIndependent,
        "-O1",
    );
    let guest = workspace.path().join("devices.wasm.o");
    support::transpile_object_configured(
        &native,
        &guest,
        support::TranspileOptions::new(zaqaru::structurer::Mode::Structured),
    );
    let module = link_container_with_image(&workspace, &[guest], &image, "devices");
    let bytes = std::fs::read(&module).expect("read the container");
    validate_wasm(&bytes);

    let report = |seed: &[u8]| -> Vec<[i64; 4]> {
        let mut container = runner::Container::instantiate(&bytes, support::mounts_seeded(seed))
            .expect("instantiate");
        container
            .call_guest("guest_devices", [1, 0, 0, 0, 0, 0])
            .expect("the guest trapped");
        let written = container
            .mounts()
            .read(&path(&[b"iso", b"console", b"stdout"]))
            .expect("mount")
            .unwrap_or_default();
        assert_eq!(written.len() % 32, 0, "the report is whole records");
        written
            .chunks_exact(32)
            .map(|chunk| {
                let word = |index: usize| {
                    i64::from_le_bytes(chunk[index * 8..index * 8 + 8].try_into().expect("eight"))
                };
                [word(0), word(1), word(2), word(3)]
            })
            .collect()
    };

    let records = report(&[7u8; 32]);
    let find = |tag: i64| {
        *records
            .iter()
            .find(|record| record[0] == tag)
            .unwrap_or_else(|| panic!("no record with tag {tag} in {records:?}"))
    };

    // The nodes, with the mode and device number Linux gives them.
    for (tag, mode, rdev) in [
        (100, 0o020666, 0x103),
        (101, 0o020666, 0x105),
        (102, 0o020666, 0x107),
        (103, 0o020666, 0x109),
    ] {
        let record = find(tag);
        assert_eq!(record[1], 0, "stat of node {tag} failed");
        assert_eq!(record[2] & 0xffff, mode, "node {tag} mode");
        assert_eq!(record[3], rdev, "node {tag} rdev");
    }

    assert_eq!(find(200)[1], 1, "/dev/null opened for writing");
    assert_eq!(find(201)[1], 0, "reading /dev/null is end of file");
    assert_eq!(find(202)[1], 16, "writing to /dev/null accepts everything");

    let zeros = find(300);
    assert_eq!(zeros[1], 128, "a read past the kernel's chunk is one read");
    assert_eq!(zeros[2], 0, "and every byte of it is zero");
    assert_eq!(find(301)[1], 0, "a device has no seek position");
    assert_eq!(find(302)[1], 8, "and reads carry on after one");

    assert_eq!(find(400)[1], 8, "/dev/full reads zeros");
    assert_eq!(find(401)[1], -28, "and refuses every write with ENOSPC");

    let random = find(500);
    assert_eq!(random[1], 1, "urandom's first read is not zeros");
    assert_eq!(random[2], 1, "nor its second");
    assert_eq!(random[3], 1, "and the stream advances");
    assert_eq!(find(600)[1], -25, "stdio is not a terminal");

    // The bytes are the seed's, expanded: the same seed replays and a
    // different one does not. This is the record-and-replay property, at
    // the smallest scale it can be tested at.
    let same = report(&[7u8; 32]);
    let different = report(&[8u8; 32]);
    let bytes_of = |records: &[[i64; 4]]| {
        *records
            .iter()
            .find(|record| record[0] == 501)
            .expect("the raw bytes")
    };
    assert_eq!(bytes_of(&records), bytes_of(&same), "the same seed replays");
    assert_ne!(
        bytes_of(&records),
        bytes_of(&different),
        "a different seed does not"
    );

    // And a container the host gave no entropy has none: the read is
    // refused rather than answered with zeros.
    let mut blind = runner::Container::instantiate(&bytes, m1_mounts()).expect("instantiate");
    blind
        .call_guest("guest_devices", [1, 0, 0, 0, 0, 0])
        .expect("the guest trapped");
    let written = blind
        .mounts()
        .read(&path(&[b"iso", b"console", b"stdout"]))
        .expect("mount")
        .unwrap_or_default();
    let unseeded: Vec<[i64; 4]> = written
        .chunks_exact(32)
        .map(|chunk| {
            let word = |index: usize| {
                i64::from_le_bytes(chunk[index * 8..index * 8 + 8].try_into().expect("eight"))
            };
            [word(0), word(1), word(2), word(3)]
        })
        .collect();
    let raw = unseeded
        .iter()
        .find(|record| record[0] == 501)
        .expect("the raw bytes");
    assert_eq!([raw[1], raw[2]], [0, 0], "no entropy, and no bytes");
}

/// `/proc/self/maps`, read from inside a container through the ordinary
/// file rows.
///
/// The file is a rendering of the VMA tree made when it is read. Everything
/// about it is asserted natively too — this is what says the rendering
/// survives the target it ships to, where an address is a real offset into
/// linear memory and `usize` is thirty-two bits wide. glibc's
/// `pthread_getattr_np` reads this file to find a thread's stack bounds,
/// which is why the format is what it is.
#[test]
fn a_guest_reads_proc_self_maps_inside_a_container() {
    let workspace = WorkingDirectory::new("m5-maps");
    let root = workspace.path().join("tree");
    std::fs::create_dir_all(root.join("proc")).expect("mkdir");
    std::fs::create_dir_all(root.join("dev")).expect("mkdir");
    let image = baker::object::emit(&baker::bake_directory(&root).expect("bake"))
        .expect("emit the image object");

    let native = support::compile_corpus_object_with(
        &workspace,
        "maps.c",
        support::Compiler::Gcc,
        support::CodeModel::PositionIndependent,
        "-O1",
    );
    let guest = workspace.path().join("maps.wasm.o");
    support::transpile_object_configured(
        &native,
        &guest,
        support::TranspileOptions::new(zaqaru::structurer::Mode::Structured),
    );
    let module = link_container_with_image(&workspace, &[guest], &image, "maps");
    let bytes = std::fs::read(&module).expect("read the container");
    validate_wasm(&bytes);
    let mut container = runner::Container::instantiate(&bytes, m1_mounts()).expect("instantiate");
    container
        .call_guest("guest_maps", [1, 0, 0, 0, 0, 0])
        .expect("the guest trapped");

    let report = container
        .mounts()
        .read(&path(&[b"iso", b"console", b"stdout"]))
        .expect("mount")
        .unwrap_or_default();
    assert!(report.len() > 32, "the guest wrote nothing");
    let word = |index: usize| {
        i64::from_le_bytes(report[index * 8..index * 8 + 8].try_into().expect("eight"))
    };
    let mapped = word(0);
    let length = word(1);
    let read = word(2);
    assert!(mapped > 0, "the guest's mmap failed with {mapped}");
    assert!(read > 0, "reading /proc/self/maps failed with {read}");
    let text =
        String::from_utf8(report[32..32 + read as usize].to_vec()).expect("the file is text");

    // Every line is `start-end perms offset dev:dev inode path`, in address
    // order — the shape `pthread_getattr_np`'s parser expects.
    let mut previous = 0u64;
    let mut covering = None;
    for line in text.lines() {
        let (range, rest) = line.split_once(' ').unwrap_or_else(|| panic!("{line:?}"));
        let (start, end) = range.split_once('-').unwrap_or_else(|| panic!("{line:?}"));
        let start = u64::from_str_radix(start, 16).unwrap_or_else(|_| panic!("{line:?}"));
        let end = u64::from_str_radix(end, 16).unwrap_or_else(|_| panic!("{line:?}"));
        assert!(start >= previous, "out of address order: {text}");
        assert!(end > start, "an empty mapping: {line:?}");
        previous = start;
        let perms = &rest[..4];
        assert!(
            perms.chars().count() == 4 && perms.ends_with('p'),
            "malformed permissions in {line:?}"
        );
        if start <= mapped as u64 && (mapped as u64) < end {
            covering = Some((start, end, perms.to_string()));
        }
    }

    let (start, end, perms) =
        covering.unwrap_or_else(|| panic!("no line covers {mapped:#x}:\n{text}"));
    assert_eq!(
        start, mapped as u64,
        "the mapping starts where it was given"
    );
    assert_eq!(
        end - start,
        length as u64,
        "and is as long as it was asked for"
    );
    assert_eq!(perms, "rw-p", "with the protection it was asked for");
}
