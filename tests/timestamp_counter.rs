//! The timestamp counter, which counts reads rather than cycles.
//!
//! `rdtsc` answers from machine state rather than from the world, and this
//! is where that is checked. It has a file of its own for the reason `cpuid`
//! does: a native run answers for the host, and the whole point is not to.
//!
//! Two commitments the design already makes decide the shape. Replay — two
//! runs from one seed must produce identical output, and a host counter read
//! straight through would differ every run. And time as a *resource* —
//! clocks reach the guest through `/iso/time` as syscalls, which is why the
//! auxiliary vector omits `AT_SYSINFO_EHDR`. `rdtsc` is an instruction and
//! bypasses that whether or not we would like it to, so it answers from
//! state.
//!
//! See `zaqaru::machine::TIMESTAMP_STEP` for why the step is large and odd.

mod support;

use support::{WorkingDirectory, m1_mounts};
use zaqaru::machine::TIMESTAMP_STEP;

fn container(workspace: &WorkingDirectory, label: &str) -> runner::Container {
    let object = support::compile_corpus_object(workspace, "timestamp.s");
    let guest = workspace.path().join(format!("timestamp.{label}.wasm.o"));
    support::transpile_object(&object, &guest, zaqaru::structurer::Mode::default());
    let module = support::link_container(workspace, &[guest], label);
    runner::Container::instantiate(
        &std::fs::read(&module).expect("read the container"),
        m1_mounts(),
    )
    .expect("instantiate")
}

fn call(container: &mut runner::Container, name: &str) -> i64 {
    container
        .call_guest(name, [0, 0, 0, 0, 0, 0])
        .expect("the guest trapped")
}

/// It advances, and by the step it says it does.
///
/// Advancing at all is what makes a spin loop terminate; advancing by a
/// known amount is what makes the run reproducible.
#[test]
fn the_counter_advances_by_its_step() {
    let workspace = WorkingDirectory::new("tsc");
    let mut container = container(&workspace, "step");

    assert_eq!(
        call(&mut container, "timestamp_step"),
        TIMESTAMP_STEP,
        "two reads did not differ by one step"
    );

    let first = call(&mut container, "timestamp");
    let second = call(&mut container, "timestamp");
    assert!(
        second > first,
        "the counter went backwards or stood still: {first} then {second}"
    );
}

/// The two halves are the counter's halves, not two copies of one.
///
/// A sixty-four bit counter delivered as `%edx:%eax` is easy to get wrong in
/// a way that looks fine until the low half wraps — which, at a step near a
/// billion, it does after about four seconds of reads.
#[test]
fn the_counter_is_delivered_as_two_halves() {
    let workspace = WorkingDirectory::new("tsc-halves");
    let mut container = container(&workspace, "halves");

    // Read until the counter passes a billion times four, so the high half
    // is nonzero and a low-half copy would show.
    let mut whole = 0i64;
    for _ in 0..8 {
        whole = call(&mut container, "timestamp");
    }
    assert!(
        whole as u64 > u32::MAX as u64,
        "the counter never reached the high half: {whole:#x}"
    );

    let high = call(&mut container, "timestamp_high") as u64;
    let whole_after = call(&mut container, "timestamp") as u64;
    assert_eq!(
        high,
        (whole_after - TIMESTAMP_STEP as u64) >> 32,
        "the high half is not the counter's top thirty-two bits"
    );
}

/// The low bits vary between reads.
///
/// glibc's adaptive mutex takes `tsc & (backoff - 1)` as jitter for its
/// exponential backoff. An even step would hand it the same value on every
/// read, which is why the step is odd.
#[test]
fn the_low_bits_differ_between_reads() {
    let workspace = WorkingDirectory::new("tsc-jitter");
    let mut container = container(&workspace, "jitter");

    let packed = call(&mut container, "timestamp_jitter");
    let (first, second) = (packed & 0xf, (packed >> 4) & 0xf);
    assert_ne!(
        first, second,
        "two consecutive reads gave the same low bits, so a backoff jitter \
         taking them would not vary"
    );
}

/// Two containers from the same image read the same sequence.
///
/// This is the property the whole design rests on: a counter that answered
/// from the host would differ here, and every run of the guest would too.
#[test]
fn two_runs_read_the_same_counter() {
    let workspace = WorkingDirectory::new("tsc-replay");
    let mut first = container(&workspace, "replay-a");
    let mut second = container(&workspace, "replay-b");

    let one: Vec<i64> = (0..5).map(|_| call(&mut first, "timestamp")).collect();
    let two: Vec<i64> = (0..5).map(|_| call(&mut second, "timestamp")).collect();
    assert_eq!(one, two, "two runs disagreed about the counter");
}

/// And `rdtscp` reports the one processor there is.
#[test]
fn the_counter_reports_one_processor() {
    let workspace = WorkingDirectory::new("tsc-processor");
    let mut container = container(&workspace, "processor");
    assert_eq!(call(&mut container, "timestamp_processor"), 0);
}
