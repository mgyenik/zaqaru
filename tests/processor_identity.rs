//! What the container says its processor is.
//!
//! This is the one instruction whose answer must deliberately *not* match
//! the host, so it has a test of its own rather than a place in the
//! differential. A libc ships several implementations of `memcpy` and picks
//! between them from what `cpuid` reports. Reporting the host's processor
//! would do two bad things: make a container's behaviour depend on the
//! machine it happened to run on, and select code paths written in
//! instructions the translator does not cover.
//!
//! So one fixed processor is reported on every host, and these are the
//! fields that decide those choices.

mod support;

use support::{WorkingDirectory, m1_mounts};

/// Feature bits in leaf 1's `edx` and `ecx` that a libc branches on.
mod feature {
    pub const SSE: u32 = 1 << 25;
    pub const SSE2: u32 = 1 << 26;
    pub const SSE3: u32 = 1 << 0;
    pub const SSSE3: u32 = 1 << 9;
    pub const SSE4_1: u32 = 1 << 19;
    pub const SSE4_2: u32 = 1 << 20;
    pub const POPCNT: u32 = 1 << 23;
    pub const OSXSAVE: u32 = 1 << 27;
    pub const AVX: u32 = 1 << 28;
}

fn container(workspace: &WorkingDirectory) -> runner::Container {
    let object = support::compile_corpus_object(workspace, "processor_id.s");
    let guest = workspace.path().join("processor_id.wasm.o");
    support::transpile_object(&object, &guest, zaqaru::structurer::Mode::default());
    let module = support::link_container(workspace, &[guest], "cpuid");
    runner::Container::instantiate(
        &std::fs::read(&module).expect("read the container"),
        m1_mounts(),
    )
    .expect("instantiate")
}

fn field(container: &mut runner::Container, name: &str, leaf: u32) -> u32 {
    container
        .call_guest(name, [leaf as i64, 0, 0, 0, 0, 0])
        .expect("the guest trapped") as u32
}

#[test]
fn the_reported_processor_has_sse2_and_nothing_later() {
    let workspace = WorkingDirectory::new("cpuid");
    let mut container = container(&workspace);

    let edx = field(&mut container, "cpuid_edx", 1);
    assert_ne!(edx & feature::SSE, 0, "SSE is not reported");
    assert_ne!(edx & feature::SSE2, 0, "SSE2 is not reported");

    // Everything after SSE2 is absent, and each one of these selects a code
    // path built from instructions this translation does not have.
    let ecx = field(&mut container, "cpuid_ecx", 1);
    for (name, bit) in [
        ("SSSE3", feature::SSSE3),
        ("SSE4.1", feature::SSE4_1),
        ("SSE4.2", feature::SSE4_2),
        ("POPCNT", feature::POPCNT),
        ("OSXSAVE", feature::OSXSAVE),
        ("AVX", feature::AVX),
    ] {
        assert_eq!(
            ecx & bit,
            0,
            "{name} is reported, so a libc will select a path for it"
        );
    }
    // SSE3 is the one thing in `ecx` that is reported, which is what makes
    // this a Core 2 rather than something older still.
    assert_ne!(ecx & feature::SSE3, 0);
}

/// The highest leaf understood stops before the one that describes AVX2.
///
/// This is not decoration. A libc asks leaf 0 how far it may go, and only
/// queries leaf 7 — where AVX2 and AVX-512 live — if the answer reaches it.
#[test]
fn the_leaf_that_describes_avx_is_out_of_range() {
    let workspace = WorkingDirectory::new("cpuid-leaves");
    let mut container = container(&workspace);

    let highest = field(&mut container, "cpuid_eax", 0);
    assert!(
        highest < 7,
        "leaf 7 is in range at {highest}, so a libc will ask it about AVX2"
    );

    // And a leaf out of range answers zero in all four, which is what a
    // processor does for a leaf it does not implement.
    for name in ["cpuid_eax", "cpuid_ebx", "cpuid_ecx", "cpuid_edx"] {
        assert_eq!(field(&mut container, name, 7), 0, "{name} for leaf 7");
        assert_eq!(
            field(&mut container, name, 0x4242),
            0,
            "{name} for a nonsense leaf"
        );
    }
}

/// The vendor string, which is read as three registers and printed as one
/// twelve-character name.
#[test]
fn the_vendor_string_reads_back_as_one_name() {
    let workspace = WorkingDirectory::new("cpuid-vendor");
    let mut container = container(&workspace);

    let mut name = Vec::new();
    for register in ["cpuid_ebx", "cpuid_edx", "cpuid_ecx"] {
        name.extend_from_slice(&field(&mut container, register, 0).to_le_bytes());
    }
    assert_eq!(
        String::from_utf8(name).expect("an unreadable vendor string"),
        "GenuineIntel"
    );
}

/// The extended control register has to agree with `cpuid`: it says which
/// processor states the operating system has enabled, and a libc reads it
/// before using an AVX path.
#[test]
fn the_enabled_processor_state_stops_at_sse() {
    let workspace = WorkingDirectory::new("cpuid-xcr0");
    let mut container = container(&workspace);

    let enabled = field(&mut container, "extended_control", 0);
    assert_eq!(enabled & 0x3, 0x3, "x87 and SSE state are not enabled");
    assert_eq!(enabled & 0x4, 0, "the AVX state is reported as live");
}

/// And the same answer on every host, which is the whole point.
#[test]
fn the_answer_does_not_depend_on_the_machine() {
    let workspace = WorkingDirectory::new("cpuid-native");
    let mut container = container(&workspace);

    let reported = field(&mut container, "cpuid_eax", 1);
    let native: u32;
    // SAFETY: `cpuid` with leaf 1, which every x86-64 processor implements.
    unsafe {
        std::arch::asm!(
            "push rbx",
            "cpuid",
            "pop rbx",
            inout("eax") 1u32 => native,
            out("ecx") _,
            out("edx") _,
        );
    }
    assert_ne!(
        reported, native,
        "the container reported this host's own processor signature, which \
         means the answer is not curated at all — or this host really is a \
         Core 2, in which case pick a different signature for the test"
    );
}
