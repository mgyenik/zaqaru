//! Sockets, which are rings with addressing.
//!
//! `docs/network-plan.md` is the plan and `container-plan.md`'s "Sockets and
//! epoll" is the authority for semantics. The sentence both rest on: **a
//! connected socket is two rings crossed.** What one endpoint writes into,
//! the other reads out of, in both directions at once — so everything a
//! stream does is already in [`crate::ring`], and what is here is the part
//! that is *not* a ring: which address a socket answers to, who is allowed
//! to connect to it, and the queue an `accept` pops from.
//!
//! # No packets, anywhere
//!
//! There is no TCP state machine, no IP layer and no checksums, because
//! nothing ever becomes a packet. A loopback connection has both ends in
//! the guest and is a kernel object; an edge connection has one end at the
//! host, which speaks real TCP on our behalf. gVisor built a userspace
//! netstack for this and the plan refuses one on the grounds that there
//! would be nothing for it to do.
//!
//! # The arena, and why it is shared
//!
//! One table behind an `Rc` on the kernel, cloned by `fork`, kept by
//! `execve` — the same arrangement, and for the same reason, as the ring
//! arena beside it. The port table lives *in* it, because a port table is
//! network-namespace state and a namespace spans processes; under a shared
//! arena that is a sentence rather than a subsystem.
//!
//! It is also where `container-plan.md`'s "multi-process honesty clause"
//! gets repealed. That clause priced sockets as the subsystem where fd
//! hoisting costs most — a prefork server's hot `accept` path running
//! through a host-side router. There is no router: workers inherit a
//! listener the way forked processes inherit a pipe, the accept queue is
//! arena state every process reaches through its own kernel, and a
//! worker's `accept4` is a queue pop. The analysis of *what* is shared
//! stands; the mechanism collapsed, exactly as it did for pipes.

use std::collections::VecDeque;

use crate::errno::Errno;
use crate::mount::Vnode;
use crate::ring::End;

/// The address families this kernel has.
pub mod family {
    pub const UNIX: i32 = 1;
    pub const INET: i32 = 2;
}

/// `SOCK_STREAM`, and the two flags the type argument carries beside it.
pub mod kind {
    pub const STREAM: i32 = 1;
    pub const DGRAM: i32 = 2;
    /// The type argument's low bits are the type; the high bits are flags,
    /// which is why `socket(AF_UNIX, SOCK_STREAM|SOCK_NONBLOCK, 0)` is one
    /// argument and not two.
    pub const MASK: i32 = 0xf;
    pub const NONBLOCK: i32 = 0o4000;
    pub const CLOEXEC: i32 = 0o2000000;
}

/// `setsockopt` levels and the options this kernel records.
pub mod option {
    pub const SOL_SOCKET: i32 = 1;
    pub const SOL_TCP: i32 = 6;

    pub const REUSEADDR: i32 = 2;
    pub const ERROR: i32 = 4;
    pub const SNDBUF: i32 = 7;
    pub const RCVBUF: i32 = 8;
    pub const KEEPALIVE: i32 = 9;
    pub const RCVTIMEO: i32 = 20;
    pub const SNDTIMEO: i32 = 21;
    pub const ACCEPTCONN: i32 = 30;
    pub const PROTOCOL: i32 = 38;
    pub const TYPE: i32 = 3;
    pub const DOMAIN: i32 = 39;

    pub const TCP_NODELAY: i32 = 1;
}

/// `shutdown`'s `how`.
pub mod how {
    pub const READ: i32 = 0;
    pub const WRITE: i32 = 1;
    pub const BOTH: i32 = 2;
}

/// Where a socket answers, in the form the arena keeps it.
///
/// Not the guest's `sockaddr`: that is parsed on the way in and rendered on
/// the way out, so that exactly one place knows the byte layout and
/// everything else compares values.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum Address {
    /// Bound to nothing yet.
    #[default]
    Unbound,
    /// `AF_INET`. The address is in host order here and byte-swapped at the
    /// boundary, because a comparison against `INADDR_ANY` written in
    /// network order is a comparison nobody can read.
    Inet { address: u32, port: u16 },
    /// `AF_UNIX`, by the vnode `bind` created — which is what makes two
    /// paths that name one file the same address, and what makes an unlinked
    /// socket unreachable by name while its connections live on.
    ///
    /// The path is kept beside it because `getsockname` answers with the
    /// path, not the inode.
    Unix { vnode: Vnode, path: Vec<u8> },
}

impl Address {
    pub fn family(&self) -> i32 {
        match self {
            Address::Unbound => 0,
            Address::Inet { .. } => family::INET,
            Address::Unix { .. } => family::UNIX,
        }
    }

    /// Whether a `connect` to `wanted` should reach a listener bound here.
    ///
    /// `0.0.0.0` is the wildcard: nginx binds it and a connection to any
    /// local address must find it. The trace in
    /// `demo/hello-django/baseline/n0-surface.txt` is why this is a rule and
    /// not an assumption — `bind(5, {AF_INET, 80, 0.0.0.0})` is what nginx
    /// actually calls.
    pub fn accepts(&self, wanted: &Address) -> bool {
        match (self, wanted) {
            (
                Address::Inet { address: bound, port: here },
                Address::Inet { port: there, .. },
            ) => here == there && (*bound == INADDR_ANY || Some(*bound) == wanted.inet_address()),
            (Address::Unix { vnode: here, .. }, Address::Unix { vnode: there, .. }) => {
                here == there
            }
            _ => false,
        }
    }

    fn inet_address(&self) -> Option<u32> {
        match self {
            Address::Inet { address, .. } => Some(*address),
            _ => None,
        }
    }

    pub fn port(&self) -> Option<u16> {
        match self {
            Address::Inet { port, .. } => Some(*port),
            _ => None,
        }
    }
}

/// `INADDR_ANY`, the wildcard bind.
pub const INADDR_ANY: u32 = 0;
/// `INADDR_LOOPBACK`, which is where every connection that stays inside the
/// container goes.
pub const INADDR_LOOPBACK: u32 = 0x7f00_0001;

/// Where Linux draws ephemeral ports from, and so does this.
///
/// A `connect` from an unbound socket needs a local address, because
/// `getsockname` is asked for one — gunicorn asks, in the traced baseline.
pub const EPHEMERAL_FIRST: u16 = 32768;
pub const EPHEMERAL_LAST: u16 = 60999;

/// What `listen` will accept as a backlog. Linux clamps to
/// `net.core.somaxconn`; the traced stack asks for 511, 2048 and 100.
pub const MAX_BACKLOG: usize = 4096;

/// One end of a connection: the two rings, and who is at each end of it.
#[derive(Clone, Debug)]
pub struct Endpoint {
    /// The ring this endpoint reads out of.
    pub receive: u32,
    /// The ring it writes into.
    pub transmit: u32,
    pub local: Address,
    pub peer: Address,
    /// Whether this endpoint has given each direction up. Separate from the
    /// ring's reference counts, which is the distinction pitfall 1 of the
    /// plan warns about: a `fork` raises one reference that stands for both
    /// rings, and the shutdown bits are *not* references.
    pub read_shut: bool,
    pub write_shut: bool,
}

/// What a socket is doing.
#[derive(Clone, Debug)]
pub enum State {
    /// `socket(2)` made it and nothing else has happened.
    Idle,
    /// `bind` succeeded. Not listening, not connected — which for a client
    /// socket is a normal place to be.
    Bound(Address),
    /// `listen` succeeded, and connections are queueing.
    Listening {
        address: Address,
        backlog: usize,
        /// Sockets that have connected and are waiting to be accepted.
        queue: VecDeque<u32>,
    },
    /// Connected, in either direction: a `connect` that succeeded, an
    /// `accept` that popped one, or half of a `socketpair`.
    Connected(Endpoint),
}

/// The options a socket records. Most of them are recorded and nothing else,
/// with a straight face: there is no TCP here to tune.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Recorded and without effect, and the reason is worth stating.
    /// `SO_REUSEADDR` exists on Linux to let a listener rebind a port still
    /// held by connections in `TIME_WAIT`. There is no `TIME_WAIT` here
    /// because there is no TCP, so a port is free the moment its listener
    /// closes and there is nothing for the option to relax.
    pub reuse_address: bool,
    /// Nagle's algorithm, which needs segments to coalesce.
    pub no_delay: bool,
    pub keep_alive: bool,
    pub receive_buffer: u32,
    pub send_buffer: u32,
    /// `SO_ERROR`: read once and cleared, which is how a non-blocking
    /// `connect` reports what happened. nginx reads it after *every*
    /// connect, completed or not — see the N0 baseline.
    pub error: i32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            reuse_address: false,
            no_delay: false,
            keep_alive: false,
            receive_buffer: crate::ring::CAPACITY as u32,
            send_buffer: crate::ring::CAPACITY as u32,
            error: 0,
        }
    }
}

/// One socket.
#[derive(Clone, Debug)]
pub struct Socket {
    pub family: i32,
    pub kind: i32,
    /// How many descriptors name it, across every process.
    pub references: u32,
    pub state: State,
    pub options: Options,
}

/// Every socket in the container. See the module comment for why it is one
/// table rather than one per process.
#[derive(Clone, Default, Debug)]
pub struct Sockets {
    entries: Vec<Option<Socket>>,
}

impl Sockets {
    /// A socket with nothing done to it, which is what `socket(2)` answers.
    pub fn create(&mut self, family: i32, kind: i32) -> u32 {
        let socket = Socket {
            family,
            kind,
            references: 1,
            state: State::Idle,
            options: Options::default(),
        };
        if let Some(free) = self.entries.iter().position(Option::is_none) {
            self.entries[free] = Some(socket);
            return free as u32;
        }
        self.entries.push(Some(socket));
        (self.entries.len() - 1) as u32
    }

    pub fn get(&self, id: u32) -> Option<&Socket> {
        self.entries.get(id as usize)?.as_ref()
    }

    pub fn get_mut(&mut self, id: u32) -> Option<&mut Socket> {
        self.entries.get_mut(id as usize)?.as_mut()
    }

    pub fn acquire(&mut self, id: u32) {
        if let Some(socket) = self.get_mut(id) {
            socket.references += 1;
        }
    }

    /// One fewer descriptor. Answers the rings to let go of when the last
    /// one goes, because the arena does not own the rings.
    pub fn release(&mut self, id: u32) -> Option<Endpoint> {
        let socket = self.get_mut(id)?;
        socket.references = socket.references.saturating_sub(1);
        if socket.references > 0 {
            return None;
        }
        let gone = self.entries[id as usize].take()?;
        match gone.state {
            State::Connected(endpoint) => Some(endpoint),
            // A listener with connections still queued: nobody will ever
            // accept them, and each one's peer must learn that. The caller
            // drains the queue.
            _ => None,
        }
    }

    /// A closing listener's unaccepted connections, which nobody will ever
    /// accept — so each one's peer sees the connection refused, in the only
    /// way a connected socket can say so: end of file and `EPIPE`.
    pub fn abandoned(&mut self, id: u32) -> Vec<u32> {
        match self.get_mut(id).map(|socket| &mut socket.state) {
            Some(State::Listening { queue, .. }) => queue.drain(..).collect(),
            _ => Vec::new(),
        }
    }

    /// The listener a connection to `wanted` should reach.
    ///
    /// A scan, and deliberately: this *is* the port table, a container holds
    /// a handful of listeners, and a scan over a `Vec` beats a map that has
    /// to be kept in step with the thing it indexes.
    pub fn listener_for(&self, wanted: &Address) -> Option<u32> {
        self.entries.iter().enumerate().find_map(|(id, held)| {
            let socket = held.as_ref()?;
            match &socket.state {
                State::Listening { address, .. } if address.accepts(wanted) => Some(id as u32),
                _ => None,
            }
        })
    }

    /// Whether anything already holds an address, which is what makes a
    /// second `bind` to it `EADDRINUSE`.
    pub fn bound(&self, wanted: &Address) -> bool {
        self.entries.iter().flatten().any(|socket| match &socket.state {
            State::Bound(address) | State::Listening { address, .. } => address == wanted,
            _ => false,
        })
    }

    /// A port nothing is using, for a `connect` from an unbound socket.
    pub fn ephemeral(&self) -> Option<u16> {
        (EPHEMERAL_FIRST..=EPHEMERAL_LAST).find(|port| {
            !self.bound(&Address::Inet {
                address: INADDR_LOOPBACK,
                port: *port,
            }) && !self.connected_from(*port)
        })
    }

    fn connected_from(&self, port: u16) -> bool {
        self.entries.iter().flatten().any(|socket| match &socket.state {
            State::Connected(endpoint) => endpoint.local.port() == Some(port),
            _ => false,
        })
    }

    /// Puts a connected socket on a listener's queue, or says the queue is
    /// full — which is a refused connection, not a wait.
    pub fn enqueue(&mut self, listener: u32, waiting: u32) -> Result<(), Errno> {
        match self.get_mut(listener).map(|socket| &mut socket.state) {
            Some(State::Listening { backlog, queue, .. }) => {
                if queue.len() >= *backlog {
                    return Err(Errno::ConnectionRefused);
                }
                queue.push_back(waiting);
                Ok(())
            }
            _ => Err(Errno::ConnectionRefused),
        }
    }

    pub fn dequeue(&mut self, listener: u32) -> Option<u32> {
        match self.get_mut(listener).map(|socket| &mut socket.state) {
            Some(State::Listening { queue, .. }) => queue.pop_front(),
            _ => None,
        }
    }

    pub fn queued(&self, listener: u32) -> usize {
        match self.get(listener).map(|socket| &socket.state) {
            Some(State::Listening { queue, .. }) => queue.len(),
            _ => 0,
        }
    }

    /// The endpoint behind a connected socket, for the transfer rows.
    pub fn endpoint(&self, id: u32) -> Option<&Endpoint> {
        match &self.get(id)?.state {
            State::Connected(endpoint) => Some(endpoint),
            _ => None,
        }
    }

    pub fn endpoint_mut(&mut self, id: u32) -> Option<&mut Endpoint> {
        match &mut self.get_mut(id)?.state {
            State::Connected(endpoint) => Some(endpoint),
            _ => None,
        }
    }
}

/// The table, as the kernel holds it. See [`crate::ring::Shared`].
pub type Shared = std::rc::Rc<std::cell::RefCell<Sockets>>;

/// Which ring a direction of a socket uses.
///
/// A socket reads from `receive` and writes into `transmit`, and it is the
/// *reader* of the first and the *writer* of the second — which is the whole
/// mapping from a socket onto the ring rules.
pub fn ring_of(endpoint: &Endpoint, direction: End) -> u32 {
    match direction {
        End::Read => endpoint.receive,
        End::Write => endpoint.transmit,
    }
}

impl<S: crate::abi::Store, M: crate::machine::Machine> crate::syscall::Kernel<'_, S, M> {
    /// What a socket answers a `poll` with.
    ///
    /// All of it is arena state — the rings' contents and reference counts,
    /// and the accept queue's length — which is what lets a scheduling
    /// decision ask it. Nothing here reads guest memory or the store, and
    /// that is the rule `poll.rs` exists to keep: readiness is asked while
    /// deciding *which* process to run, and the answer must not depend on
    /// which one is currently at the guest's addresses.
    pub(crate) fn socket_readiness(&self, id: u32) -> i16 {
        use crate::poll::event;
        let sockets = self.sockets.borrow();
        let Some(socket) = sockets.get(id) else {
            return event::NVAL;
        };
        match &socket.state {
            // A listener is readable when something is waiting to be
            // accepted, which is how every event loop finds out.
            State::Listening { .. } => match sockets.queued(id) {
                0 => 0,
                _ => event::IN | event::RDNORM,
            },
            // Nothing has happened to it yet. Linux reports a socket that
            // is neither connected nor listening as writable and hung up,
            // which is what a `poll` on an unconnected socket sees.
            State::Idle | State::Bound(_) => event::OUT | event::WRNORM | event::HUP,
            State::Connected(endpoint) => {
                let rings = self.rings.borrow();
                let mut bits = 0;
                let queued = rings.queued(endpoint.receive);
                if queued > 0 {
                    bits |= event::IN | event::RDNORM;
                }
                // The peer will send nothing more. `EPOLLRDHUP` is the
                // half-close a program watches for when it wants to keep
                // writing after the peer has stopped — nginx registers it,
                // in the traced baseline — and `POLLHUP` is only for when
                // *both* directions are done.
                let peer_writing = rings.writers(endpoint.receive) > 0;
                if !peer_writing && !endpoint.read_shut {
                    bits |= event::RDHUP;
                }
                let peer_reading = rings.readers(endpoint.transmit) > 0;
                if !endpoint.write_shut && (rings.room(endpoint.transmit) > 0 || !peer_reading) {
                    bits |= event::OUT | event::WRNORM;
                }
                // Nobody will ever read what this writes.
                if !peer_reading {
                    bits |= event::ERR;
                }
                if !peer_writing && !peer_reading {
                    bits |= event::HUP;
                }
                bits
            }
        }
    }

    /// `socket(2)`.
    ///
    /// Two families, and the refusal for anything else is by name rather
    /// than by silence: `AF_INET` reaches loopback and, when a port is
    /// mapped, the edge; `AF_UNIX` reaches the filesystem. `AF_INET6`,
    /// `AF_NETLINK` and `AF_PACKET` are each a different subsystem and each
    /// would be a lie to accept.
    pub(crate) fn make_socket(&mut self, arguments: crate::syscall::Arguments) -> crate::syscall::Outcome {
        use crate::syscall::Outcome;
        let family = arguments.get(0) as i32;
        let requested = arguments.get(1) as i32;
        let protocol = arguments.get(2) as i32;
        match self.open_socket(family, requested, protocol) {
            Ok(fd) => Outcome::Done(i64::from(fd)),
            Err(errno) => Outcome::Done(errno.as_result()),
        }
    }

    /// The half `socket` and `socketpair` share.
    fn open_socket(&mut self, family: i32, requested: i32, protocol: i32) -> Result<i32, Errno> {
        use crate::file::open_flags;
        if family != family::INET && family != family::UNIX {
            return Err(Errno::AddressFamily);
        }
        let kind = requested & kind::MASK;
        if kind != kind::STREAM {
            // Datagrams are a different object — a queue of messages rather
            // than a stream of bytes — and the traced stack never asks for
            // one. Refused by the errno Linux gives, not by a fault: a
            // program that probes for `SOCK_DGRAM` support gets an answer.
            return Err(Errno::ProtocolUnsupported);
        }
        // `IPPROTO_IP` (0) and `IPPROTO_TCP` (6) both mean TCP for a stream
        // socket, which is what nginx passes.
        if protocol != 0 && !(family == family::INET && protocol == 6) {
            return Err(Errno::ProtocolUnsupported);
        }
        let id = self.sockets.borrow_mut().create(family, kind);
        let flags = open_flags::READ_WRITE
            | match requested & kind::NONBLOCK != 0 {
                true => open_flags::NONBLOCK,
                false => 0,
            };
        self.files
            .open(
                crate::fd::Backing::Socket(id),
                flags,
                requested & kind::CLOEXEC != 0,
            )
            .inspect_err(|_| {
                self.sockets.borrow_mut().release(id);
            })
    }

    /// `socketpair(2)`: two connected endpoints and no address at all.
    ///
    /// The simplest connection there is, and the one gunicorn's master uses
    /// to tell its worker to shut down — by `sendmsg`, in the traced
    /// baseline. Two rings, crossed: what one writes the other reads.
    pub(crate) fn make_socketpair(&mut self, arguments: crate::syscall::Arguments) -> crate::syscall::Outcome {
        use crate::syscall::Outcome;
        let family = arguments.get(0) as i32;
        let requested = arguments.get(1) as i32;
        let protocol = arguments.get(2) as i32;
        let at = arguments.get(3) as u64;
        if family != family::UNIX {
            // Linux has no `AF_INET` socketpair, and saying so is better
            // than inventing one.
            return Outcome::Done(Errno::NotSupported.as_result());
        }
        if let Err(errno) = self.memory().check(at, 8) {
            return Outcome::Done(errno.as_result());
        }
        let first = match self.open_socket(family, requested, protocol) {
            Ok(fd) => fd,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        let second = match self.open_socket(family, requested, protocol) {
            Ok(fd) => fd,
            Err(errno) => {
                self.shut_socket(first);
                return Outcome::Done(errno.as_result());
            }
        };
        // One ring each way. Each ring starts with one reader and one
        // writer, which is exactly one endpoint on each side of it.
        let forward = self.rings.borrow_mut().create();
        let backward = self.rings.borrow_mut().create();
        let (Some(a), Some(b)) = (self.socket_of(first), self.socket_of(second)) else {
            return Outcome::Done(Errno::BadFile.as_result());
        };
        {
            let mut sockets = self.sockets.borrow_mut();
            if let Some(socket) = sockets.get_mut(a) {
                socket.state = State::Connected(Endpoint {
                    receive: backward,
                    transmit: forward,
                    local: Address::Unbound,
                    peer: Address::Unbound,
                    read_shut: false,
                    write_shut: false,
                });
            }
            if let Some(socket) = sockets.get_mut(b) {
                socket.state = State::Connected(Endpoint {
                    receive: forward,
                    transmit: backward,
                    local: Address::Unbound,
                    peer: Address::Unbound,
                    read_shut: false,
                    write_shut: false,
                });
            }
        }
        let mut bytes = [0u8; 8];
        bytes[0..4].copy_from_slice(&first.to_le_bytes());
        bytes[4..8].copy_from_slice(&second.to_le_bytes());
        // SAFETY: bounds-checked above, and nothing has run since.
        match unsafe { self.memory_mut().write(at, &bytes) } {
            Ok(()) => Outcome::Done(0),
            Err(errno) => {
                self.shut_socket(first);
                self.shut_socket(second);
                Outcome::Done(errno.as_result())
            }
        }
    }

    /// The ring a descriptor's socket moves bytes through in one direction.
    ///
    /// Three answers rather than two, because a direction that has been shut
    /// down is neither an error nor a ring: a `read` on one is a *completed
    /// read of nothing*, which is end of file, and end of file is not an
    /// errno. A `write` on one is `EPIPE` and a `SIGPIPE`. Collapsing those
    /// into a `Result` is how a half-closed socket comes to report a failure
    /// to a program that was told to expect a zero.
    pub(crate) fn socket_ring(&self, fd: i32, direction: End) -> Reach {
        let Ok(file) = self.files.description(fd) else {
            return Reach::Elsewhere;
        };
        let crate::fd::Backing::Socket(id) = file.backing else {
            return Reach::Elsewhere;
        };
        let sockets = self.sockets.borrow();
        let Some(endpoint) = sockets.endpoint(id) else {
            return Reach::Refused(Errno::NotConnected);
        };
        let shut = match direction {
            End::Read => endpoint.read_shut,
            End::Write => endpoint.write_shut,
        };
        match (shut, direction) {
            (true, End::Read) => Reach::Finished,
            (true, End::Write) => Reach::Refused(crate::ring::BROKEN),
            _ => Reach::Ring {
                ring: ring_of(endpoint, direction),
                flags: file.flags,
            },
        }
    }

    /// The socket a descriptor names, or `None`.
    pub(crate) fn socket_of(&self, fd: i32) -> Option<u32> {
        match self.files.description(fd).ok()?.backing {
            crate::fd::Backing::Socket(id) => Some(id),
            _ => None,
        }
    }

    /// Closes a descriptor during a row's own error unwind.
    fn shut_socket(&mut self, fd: i32) {
        let census = self.shared_census();
        let _ = self.files.close(fd);
        self.reconcile_shared(&census);
    }

    /// `shutdown(2)`: give up one direction, or both.
    ///
    /// A half-close, and `close` is only the both-directions case of it.
    /// Each direction is a reference on a ring, so giving one up is exactly
    /// what the last writer of a pipe closing already does: the peer drains
    /// and then reads zero, or its next write is `EPIPE`.
    pub(crate) fn shutdown(&mut self, arguments: crate::syscall::Arguments) -> crate::syscall::Outcome {
        use crate::syscall::Outcome;
        let Some(id) = self.socket_of(arguments.get(0) as i32) else {
            return Outcome::Done(match self.files.is_open(arguments.get(0) as i32) {
                true => Errno::NotSocket.as_result(),
                false => Errno::BadFile.as_result(),
            });
        };
        let direction = arguments.get(1) as i32;
        if !(how::READ..=how::BOTH).contains(&direction) {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let mut released: Vec<(u32, End)> = Vec::new();
        {
            let mut sockets = self.sockets.borrow_mut();
            let Some(endpoint) = sockets.endpoint_mut(id) else {
                // `shutdown` on a socket that was never connected is
                // `ENOTCONN`, which is how a program learns the difference
                // between "closed" and "never opened".
                return Outcome::Done(Errno::NotConnected.as_result());
            };
            if direction != how::WRITE && !endpoint.read_shut {
                endpoint.read_shut = true;
                released.push((endpoint.receive, End::Read));
            }
            if direction != how::READ && !endpoint.write_shut {
                endpoint.write_shut = true;
                released.push((endpoint.transmit, End::Write));
            }
        }
        let mut rings = self.rings.borrow_mut();
        for (ring, end) in released {
            rings.release(ring, end);
        }
        Outcome::Done(0)
    }
}

/// What a descriptor reaches when a transfer asks for its socket's ring.
pub enum Reach {
    /// Not a socket at all — the caller's other backings apply.
    Elsewhere,
    /// Move bytes through this ring, with the description's flags.
    Ring { ring: u32, flags: i32 },
    /// The direction is shut. A completed transfer of nothing.
    Finished,
    Refused(Errno),
}
