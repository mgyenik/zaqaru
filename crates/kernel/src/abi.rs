//! The store: the kernel's one face towards the host.
//!
//! Everything the kernel cannot answer from its own memory — time, entropy,
//! external I/O, the console, diagnostics — is a path under `/iso` read or
//! written through a [`Store`], and adding a capability later is adding a
//! mount, never adding an import. The kernel sees only the trait; the
//! module's real store, the two host imports it is lowered onto, and the
//! arena the host writes into live in the `guest` crate, and the native
//! tests supply in-memory doubles.

/// What a store call answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StoreOutcome {
    /// `ok(some(bytes))` for a read, `ok(_)` for a write.
    Present,
    /// `ok(none)`: the path exists as an address but holds nothing.
    Absent,
    /// `err(message)`. The message is a diagnostic, never an errno — errno
    /// is decided by the syscall row that provoked the call, because POSIX
    /// lives in the kernel and nowhere else.
    Failed,
}

/// The store, as the kernel sees it: paths in, bytes out, nothing else.
///
/// A trait rather than two free functions so the whole kernel above it is
/// testable natively against an in-memory double — which is where every
/// piece of kernel logic gets falsified first, in milliseconds, before
/// emulation is involved at all.
pub trait Store {
    fn read(&mut self, path: &[&[u8]], into: &mut Vec<u8>) -> StoreOutcome;
    fn write(&mut self, path: &[&[u8]], data: &[u8]) -> StoreOutcome;

    /// The diagnostic from the most recent failure, appended to `into`.
    ///
    /// A store error is a string, and it is the only thing on this boundary
    /// that says *why*. The errno the guest sees is decided by the syscall
    /// row and cannot carry it, so without this the reason is simply lost.
    /// Valid only until the end of the current syscall, which is the arena's
    /// lifetime.
    fn last_error(&self, into: &mut Vec<u8>) {
        let _ = into;
    }

    /// A syscall is beginning. Whatever the store handed the kernel for the
    /// previous one is dead.
    ///
    /// The one store that needs to hear this is the module's real one, whose
    /// host writes returned bytes into an arena that lives for exactly one
    /// syscall. An in-memory double has nothing to reset.
    fn begin_syscall(&mut self) {}
}

/// A store several processes reach.
///
/// A container is a process *tree* and the host boundary is the container's:
/// a child's `write` to the console has to arrive on the same console its
/// parent writes to. The kernel is replicated per process — which is what
/// makes inheritance across a fork correct by construction — and the store
/// is the one thing that must not be.
///
/// Interior mutability rather than a borrow because the processes outlive
/// each other in no particular order, and a lifetime saying otherwise would
/// be saying something untrue.
pub struct Shared<S>(std::rc::Rc<std::cell::RefCell<S>>);

impl<S> Clone for Shared<S> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<S> Shared<S> {
    pub fn new(store: S) -> Self {
        Self(std::rc::Rc::new(std::cell::RefCell::new(store)))
    }

    /// The store itself, for a host reading back what the container wrote.
    pub fn borrow(&self) -> std::cell::Ref<'_, S> {
        self.0.borrow()
    }

    /// The same, to change what the host will answer next.
    ///
    /// A host's answers are not fixed at boot — a clock moves, a network
    /// delivers, and a shutdown is asked for while the container is
    /// running. This is how an embedder that holds a store changes one.
    pub fn borrow_mut(&self) -> std::cell::RefMut<'_, S> {
        self.0.borrow_mut()
    }
}

impl<S: Store> Store for Shared<S> {
    fn read(&mut self, path: &[&[u8]], into: &mut Vec<u8>) -> StoreOutcome {
        self.0.borrow_mut().read(path, into)
    }

    fn write(&mut self, path: &[&[u8]], data: &[u8]) -> StoreOutcome {
        self.0.borrow_mut().write(path, data)
    }

    fn last_error(&self, into: &mut Vec<u8>) {
        self.0.borrow().last_error(into);
    }

    fn begin_syscall(&mut self) {
        self.0.borrow_mut().begin_syscall();
    }
}
