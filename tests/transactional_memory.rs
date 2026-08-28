//! Hardware transactional memory, which this machine does not have.
//!
//! `xbegin` starts a transaction and names where to jump if it aborts. The
//! architecture is explicit that a transaction may abort spuriously, for no
//! reason at all, and that software must never rely on one committing —
//! which is why every user of `xbegin` carries a non-transactional fallback.
//! So a transaction that always aborts is a conformant implementation rather
//! than a stand-in for one, and taking the fallback is taking a path the
//! program is already required to have.
//!
//! This has a test file of its own rather than a place in the differential
//! for the same reason `cpuid` does: there is no native answer to compare
//! against. On a host without TSX `xbegin` is an invalid opcode, and on one
//! with it the transaction may genuinely commit — either way the result
//! would depend on the machine underneath, which is what a container exists
//! to prevent.

mod support;

use support::{WorkingDirectory, m1_mounts};

fn container(workspace: &WorkingDirectory) -> runner::Container {
    let object = support::compile_corpus_object(workspace, "transaction.s");
    let guest = workspace.path().join("transaction.wasm.o");
    support::transpile_object(&object, &guest, zaqaru::structurer::Mode::default());
    let module = support::link_container(workspace, &[guest], "transaction");
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

/// Every transaction aborts, and the status says not to try again.
///
/// The retry bit is the one that matters: glibc's lock elision loops while
/// the status says a retry might succeed, so a status carrying that bit
/// spins forever instead of taking the lock.
#[test]
fn a_transaction_aborts_and_does_not_ask_to_be_retried() {
    let workspace = WorkingDirectory::new("tsx");
    let mut container = container(&workspace);

    let status = call(&mut container, "transaction_status");
    assert_ne!(status, 1000, "a transaction committed");
    assert_eq!(
        status & 0b10,
        0,
        "the abort status asks to be retried, which spins an elision loop \
         forever instead of taking the lock"
    );
    assert_eq!(status, 0, "the abort status is not a plain `do not retry`");
}

/// The elision loop gives up after one attempt and falls back.
///
/// This is the shape `__lll_lock_elision` has, and it is what the status
/// above is *for*: one try, no retry, take the real lock.
#[test]
fn a_lock_elision_gives_up_after_one_attempt() {
    let workspace = WorkingDirectory::new("tsx-elision");
    let mut container = container(&workspace);

    let attempts = call(&mut container, "elision_attempts");
    assert_eq!(
        attempts, 1,
        "the elision loop made {attempts} attempts; one is the only answer \
         that both tries and falls back"
    );
}

/// And nothing is ever inside a transaction.
#[test]
fn no_transaction_is_ever_active() {
    let workspace = WorkingDirectory::new("tsx-active");
    let mut container = container(&workspace);
    assert_eq!(call(&mut container, "inside_transaction"), 0);
}
