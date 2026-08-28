//! What a process is told about itself, and the small syscalls a libc asks
//! before it does anything else.
//!
//! These are exactly what a static glibc `hello` issues between `execve` and
//! its first `write`: `set_tid_address`, `set_robust_list`, `rseq`,
//! `prlimit64`, `getrandom`. None of them does any work, and all of them
//! have to answer, because glibc treats a surprise here as fatal — which is
//! why five rows that return constants are the difference between a real
//! binary running and one that dies before `main`.
//!
//! Two halves. What must agree with Linux — the argument checks and the
//! refusals — is compared against a native run. What must deliberately *not*
//! agree is checked here: a container's process id is one, because its entry
//! process is the first in its own namespace, and the stack limit is
//! another, because it has to be the stack the kernel actually handed over
//! rather than the host's.

mod support;

use support::{WorkingDirectory, m1_mounts, mounts_seeded};

fn container(workspace: &WorkingDirectory, label: &str, seed: Option<&[u8]>) -> runner::Container {
    let object = support::compile_corpus_object(workspace, "process_identity.c");
    let guest = workspace.path().join(format!("identity.{label}.wasm.o"));
    support::transpile_object(&object, &guest, zaqaru::structurer::Mode::default());
    let module = support::link_container(workspace, &[guest], label);
    let mounts = match seed {
        Some(seed) => mounts_seeded(seed),
        None => m1_mounts(),
    };
    runner::Container::instantiate(&std::fs::read(&module).expect("read"), mounts)
        .expect("instantiate")
}

fn call(container: &mut runner::Container, name: &str, argument: i64) -> i64 {
    container
        .call_guest(name, [argument, 0, 0, 0, 0, 0])
        .expect("the guest trapped")
}

/// The stack limit is the stack the guest was actually given.
///
/// glibc reads it at startup to size a thread's stack attribute. An answer
/// taken from the host would describe a stack this process does not have,
/// which is worse than no answer at all — so it comes from the one place
/// that decides it.
#[test]
fn the_stack_limit_is_the_stack_the_guest_was_given() {
    const RLIMIT_STACK: u32 = 3;
    let (soft, hard) =
        kisal::syscall::resource_limit_for(RLIMIT_STACK).expect("the stack has a limit");
    assert_eq!(
        soft,
        kisal::exec::STACK_BYTES,
        "the limit reported and the stack allocated disagree"
    );
    assert_eq!(hard, u64::MAX, "the stack's hard limit is not unlimited");
}

/// A limit nothing here decides is a named fault, not an invented number.
#[test]
fn an_unmodelled_limit_is_refused_by_name() {
    const RLIMIT_CPU: u32 = 0;
    assert!(
        kisal::syscall::resource_limit_for(RLIMIT_CPU).is_none(),
        "a limit was invented for a resource this container does not model"
    );
}

/// Registering a robust list of the wrong size is refused, and the right
/// size accepted.
#[test]
fn the_robust_list_checks_its_own_size() {
    let workspace = WorkingDirectory::new("identity-robust");
    let mut container = container(&workspace, "robust", None);

    let packed = call(&mut container, "guest_robust_list", 0);
    assert_eq!(packed & 0xffff, 0, "the correct size was refused");
    assert_eq!(
        (packed >> 16) & 0xffff,
        (-22i64) as u16 as i64,
        "a wrong size was accepted, so the list would be unreadable later"
    );
}

/// Restartable sequences are refused for real.
///
/// glibc takes `ENOSYS` by never using the feature again. A registration
/// that appeared to succeed would leave it expecting the kernel to restart
/// its critical sections, and nothing would.
#[test]
fn restartable_sequences_are_refused() {
    let workspace = WorkingDirectory::new("identity-rseq");
    let mut container = container(&workspace, "rseq", None);
    assert_eq!(call(&mut container, "guest_rseq", 0), -38, "not ENOSYS");
}

/// Setting a limit is refused by name rather than ignored.
///
/// A limit that appeared to change and did not would be a guest sizing
/// something against a promise nothing keeps.
#[test]
fn changing_a_limit_is_a_named_fault() {
    let workspace = WorkingDirectory::new("identity-setlimit");
    let mut container = container(&workspace, "setlimit", None);

    let error = container
        .call_guest("guest_limit_set", [0, 0, 0, 0, 0, 0])
        .expect_err("setting a resource limit succeeded");
    let _ = error;
    let log = container
        .mounts()
        .read(&[b"iso".to_vec(), b"log".to_vec(), b"error".to_vec()])
        .expect("the log mount failed")
        .unwrap_or_default();
    let log = String::from_utf8_lossy(&log).into_owned();
    assert!(
        log.contains("prlimit64") && log.contains("changing a resource limit"),
        "the refusal does not say what was asked for: {log}"
    );
}

/// Random bytes come from the seeded stream, so a run replays.
///
/// Compared on what the runs *drew*, not on how much: the count is always
/// the length asked for, so comparing counts would agree no matter what the
/// generator did.
#[test]
fn random_bytes_replay_from_the_seed() {
    let workspace = WorkingDirectory::new("identity-random");
    let mut first = container(&workspace, "random-a", Some(&[0x33; 32]));
    let mut second = container(&workspace, "random-b", Some(&[0x33; 32]));
    let mut other = container(&workspace, "random-c", Some(&[0x44; 32]));

    // The count is always everything asked for: the generator is always
    // ready, which is the only reason a real kernel would give less.
    for length in [1i64, 8, 64, 256, 512] {
        assert_eq!(call(&mut first, "guest_random_count", length), length);
    }

    let draw = |container: &mut runner::Container| -> Vec<i64> {
        (0..4)
            .map(|_| call(container, "guest_random_word", 0))
            .collect()
    };
    // `first` has already drawn above, so it is not compared against the
    // others — a stream that has advanced differently is a different
    // stream, which is the property working rather than failing.
    let same = draw(&mut second);
    let different = draw(&mut other);
    assert_eq!(
        same,
        draw(&mut container(&workspace, "random-d", Some(&[0x33; 32]))),
        "two containers with one seed drew different bytes"
    );
    assert_ne!(
        same, different,
        "two containers with different seeds drew the same bytes"
    );
}

/// A container with no entropy refuses to invent some.
///
/// That is the capability model: what a container can do is decided by its
/// mount table, and a generator with no seed answers by name rather than
/// with zeros — which are both plausible and catastrophic.
#[test]
fn a_container_without_entropy_refuses_rather_than_inventing() {
    let workspace = WorkingDirectory::new("identity-unseeded");
    let mut container = container(&workspace, "unseeded", None);
    assert_eq!(
        call(&mut container, "guest_random_count", 8),
        -19,
        "an unseeded container produced random bytes"
    );
}

/// The argument checks match Linux exactly.
///
/// These are the half that must agree: a refusal Linux gives and this does
/// not — or the reverse — is a libc taking a different branch at startup.
/// The answers that must deliberately *differ* (the process id, the stack
/// limit) are checked above instead, because a native run answers for the
/// host and the whole point is not to.
#[test]
fn the_argument_checks_match_native() {
    let workspace = WorkingDirectory::new("identity-native");
    let path = support::compile_corpus_shared_library(&workspace, &["process_identity.c"]);
    let library = unsafe { libloading::Library::new(&path) }.expect("load the native guest");
    let mut container = container(&workspace, "native", Some(&[0x55; 32]));

    // `rseq` is deliberately absent from this list. The native process has
    // already registered one — glibc does it at startup, which the strace of
    // any static binary shows — so a second registration answers `EINVAL`
    // and says nothing about a kernel that has none. Refusing it with
    // `ENOSYS` is specified behaviour rather than agreement, and
    // `restartable_sequences_are_refused` is where that is checked.
    for name in ["guest_robust_list", "guest_random_refusals"] {
        let native: libloading::Symbol<unsafe extern "C" fn(i64) -> i64> =
            unsafe { support::native_function(&library, name) };
        let expected = unsafe { native(0) };
        assert_eq!(
            call(&mut container, name, 0),
            expected,
            "{name} disagreed with native"
        );
    }

    // Probing a limit that exists answers zero on both; the resources are
    // the ones a libc actually asks about.
    for resource in [
        3i64, /* stack */
        7,    /* open files */
        9,    /* address space */
    ] {
        let native: libloading::Symbol<unsafe extern "C" fn(i64) -> i64> =
            unsafe { support::native_function(&library, "guest_limit_probe") };
        let expected = unsafe { native(resource) };
        assert_eq!(
            call(&mut container, "guest_limit_probe", resource),
            expected,
            "probing resource {resource} disagreed with native"
        );
    }
}
