//! Sockets, which are rings with addressing.
//!
//! The sentence everything here rests on: **a connected socket is two rings
//! crossed.** What one endpoint writes into,
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
//! It is also where the early worry about multi-process sockets went away.
//! That worry priced sockets as the subsystem where fd
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

/// Whether an address is one of this container's own — `127.0.0.0/8`, the
/// whole of the network a namespace with only `lo` in it has.
pub fn loopback(address: u32) -> bool {
    address >> 24 == 127
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
    /// connect, completed or not — see the traced baseline.
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
    /// The host's number for this connection or listener, when it is one the
    /// host terminates.
    ///
    /// `None` for everything that stays inside the container, which is most
    /// of it: a loopback connection has both ends in the guest and never
    /// reaches the store. This is what tells the pump which sockets have a
    /// far side it has to move bytes to.
    pub edge: Option<u32>,
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
            edge: None,
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

    /// The host-side connection number an edge socket stands for, if it is
    /// one. Asked before a release, because a retired socket is gone and
    /// the host still has to be told about it.
    pub fn edge_of(&self, id: u32) -> Option<u32> {
        self.get(id)?.edge
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

    /// Every listening socket, by identifier.
    pub fn listeners(&self) -> Vec<u32> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(id, held)| {
                matches!(held.as_ref()?.state, State::Listening { .. }).then_some(id as u32)
            })
            .collect()
    }

    /// Every connected socket the host terminates, with its host number.
    pub fn edges(&self) -> Vec<(u32, u32)> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(id, held)| {
                let socket = held.as_ref()?;
                let edge = socket.edge?;
                matches!(socket.state, State::Connected(_)).then_some((id as u32, edge))
            })
            .collect()
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
                // The peer will send nothing more. `EPOLLRDHUP` is the
                // half-close a program watches for when it wants to keep
                // writing after the peer has stopped — nginx registers it,
                // in the traced baseline — and `POLLHUP` is only for when
                // *both* directions are done.
                let peer_writing = rings.writers(endpoint.receive) > 0;
                // Readable when there are bytes, and readable when there
                // never will be: end of file is something a `read` returns
                // without waiting, which is what `POLLIN` asks. Linux
                // reports a `FIN` as `POLLIN | POLLRDHUP`, and a program
                // that asked only for `POLLIN` — CPython's `recv` under a
                // socket timeout, which is how gunicorn waits for nginx to
                // close its side — must not sleep through it.
                if queued > 0 || !peer_writing || endpoint.read_shut {
                    bits |= event::IN | event::RDNORM;
                }
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
attach_endpoint(&mut self.rings.borrow_mut(), backward, forward);
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
attach_endpoint(&mut self.rings.borrow_mut(), forward, backward);
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
        // The write half of an edge connection, if this call gives it up.
        // A half-close has to reach the host for the same reason a full one
        // does: the peer is waiting for an end of file, and nothing inside
        // the module can send it one.
        let mut half_closed: Option<(u32, u32)> = None;
        {
            let mut sockets = self.sockets.borrow_mut();
            let edge = sockets.edge_of(id);
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
                if let Some(conn) = edge {
                    half_closed = Some((conn, endpoint.transmit));
                }
            }
        }
        // Before the ring goes, because the flush inside reads it — the
        // same ordering the close path needs, and for the same reason.
        if let Some((conn, ring)) = half_closed {
            self.end_edge(conn, ring, b"shutdown");
        }
        let mut rings = self.rings.borrow_mut();
        for (ring, end) in released {
            rings.release(ring, end);
        }
        Outcome::Done(0)
    }
}

/// Records that a freshly connected endpoint names two rings.
///
/// Paired with [`detach_endpoint`], and both exist because the rings are
/// named by the endpoint for longer than the endpoint holds an end of them
/// open — see `Ring::attached` in `ring.rs`.
fn attach_endpoint(rings: &mut crate::ring::Rings, receive: u32, transmit: u32) {
    rings.attach(receive);
    rings.attach(transmit);
}

/// The endpoint a released socket had, if any, no longer names its rings.
fn detach_endpoint(rings: &mut crate::ring::Rings, retired: Option<Endpoint>) {
    if let Some(endpoint) = retired {
        rings.detach(endpoint.receive);
        rings.detach(endpoint.transmit);
    }
}

/// `/iso/net/conn/{j}/{leaf}`, built once so that the pump and the close
/// path cannot disagree about where a connection lives.
fn edge_path(conn: u32, leaf: &[u8]) -> Vec<Vec<u8>> {
    vec![
        b"iso".to_vec(),
        b"net".to_vec(),
        b"conn".to_vec(),
        conn.to_string().into_bytes(),
        leaf.to_vec(),
    ]
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

/// A `sockaddr` as the guest wrote it, before it means anything.
///
/// Separate from [`Address`] because `bind` and `connect` interpret the same
/// bytes differently: an `AF_UNIX` `bind` *creates* the node its path names
/// and a `connect` *resolves* one, so the parse cannot resolve on its own
/// without deciding which call it is in.
#[derive(Clone, Debug)]
pub enum Requested {
    Inet { address: u32, port: u16 },
    Unix { path: Vec<u8> },
}

/// `struct sockaddr_in`.
const SOCKADDR_IN: u64 = 16;
/// `struct sockaddr_un`: a family and 108 bytes of path.
const SOCKADDR_UN: u64 = 110;
const UNIX_PATH: usize = 108;

impl<S: crate::abi::Store, M: crate::machine::Machine> crate::syscall::Kernel<'_, S, M> {
    /// Reads a `sockaddr` out of guest memory.
    ///
    /// The one place the byte layout is known, so that everything else
    /// compares values. Ports and addresses are network order on the wire
    /// and host order in the arena, and the swap happens here — a
    /// comparison against `INADDR_ANY` written in network order is a
    /// comparison nobody can read.
    fn address_at(&self, at: u64, length: u64) -> Result<Requested, Errno> {
        if length < 2 {
            return Err(Errno::Invalid);
        }
        let mut family = [0u8; 2];
        self.pages.read(at, &mut family).map_err(|_| Errno::Fault)?;
        match i32::from(u16::from_le_bytes(family)) {
            family::INET => {
                if length < SOCKADDR_IN {
                    return Err(Errno::Invalid);
                }
                let mut bytes = [0u8; SOCKADDR_IN as usize];
                self.pages.read(at, &mut bytes).map_err(|_| Errno::Fault)?;
                Ok(Requested::Inet {
                    port: u16::from_be_bytes(bytes[2..4].try_into().expect("two bytes")),
                    address: u32::from_be_bytes(bytes[4..8].try_into().expect("four bytes")),
                })
            }
            family::UNIX => {
                let span = length.min(SOCKADDR_UN);
                let mut bytes = vec![0u8; span as usize];
                self.pages.read(at, &mut bytes).map_err(|_| Errno::Fault)?;
                let path = &bytes[2..];
                if path.first() == Some(&0) {
                    // The abstract namespace: a name with no filesystem
                    // behind it, which this kernel has no table for.
                    // Refused by name rather than treated as an empty path,
                    // which would silently make every abstract socket the
                    // same one.
                    return Err(Errno::AddressUnavailable);
                }
                let end = path.iter().position(|byte| *byte == 0).unwrap_or(path.len());
                if end == 0 {
                    return Err(Errno::Invalid);
                }
                Ok(Requested::Unix {
                    path: path[..end].to_vec(),
                })
            }
            _ => Err(Errno::AddressFamily),
        }
    }

    /// Writes an [`Address`] back where `getsockname` and friends were told
    /// to put it, and updates the length the caller passed in and out.
    ///
    /// The out-length is the address's *full* size even when the buffer was
    /// smaller — Linux truncates the bytes and reports what it would have
    /// needed, which is how a caller learns to ask again.
    pub(crate) fn write_address(&mut self, at: u64, length_at: u64, address: &Address) -> Result<(), Errno> {
        if at == 0 || length_at == 0 {
            return Ok(());
        }
        let mut room = [0u8; 4];
        self.pages
            .read(length_at, &mut room)
            .map_err(|_| Errno::Fault)?;
        let room = u32::from_le_bytes(room) as u64;
        let rendered: Vec<u8> = match address {
            Address::Unbound => {
                let mut bytes = vec![0u8; SOCKADDR_IN as usize];
                bytes[0..2].copy_from_slice(&(family::UNIX as u16).to_le_bytes());
                bytes.truncate(2);
                bytes
            }
            Address::Inet { address, port } => {
                let mut bytes = vec![0u8; SOCKADDR_IN as usize];
                bytes[0..2].copy_from_slice(&(family::INET as u16).to_le_bytes());
                bytes[2..4].copy_from_slice(&port.to_be_bytes());
                bytes[4..8].copy_from_slice(&address.to_be_bytes());
                bytes
            }
            Address::Unix { path, .. } => {
                let kept = path.len().min(UNIX_PATH - 1);
                // Two for the family, the path, and the terminator Linux
                // counts in the length it reports.
                let mut bytes = vec![0u8; 2 + kept + 1];
                bytes[0..2].copy_from_slice(&(family::UNIX as u16).to_le_bytes());
                bytes[2..2 + kept].copy_from_slice(&path[..kept]);
                bytes
            }
        };
        let written = (rendered.len() as u64).min(room);
        if written > 0 {
            self.pages
                .write(at, &rendered[..written as usize])
                .map_err(|_| Errno::Fault)?;
        }
        self.pages
            .write(length_at, &(rendered.len() as u32).to_le_bytes())
            .map_err(|_| Errno::Fault)?;
        Ok(())
    }

    /// `bind(2)`.
    pub(crate) fn bind(&mut self, arguments: crate::syscall::Arguments) -> crate::syscall::Outcome {
        use crate::syscall::Outcome;
        let fd = arguments.get(0) as i32;
        let Some(id) = self.socket_of(fd) else {
            return Outcome::Done(self.not_a_socket(fd));
        };
        let requested = match self.address_at(arguments.get(1) as u64, arguments.get(2) as u64) {
            Ok(requested) => requested,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        // Bound once. Linux answers `EINVAL` for a second `bind`, which is
        // how a program learns it is looking at the wrong socket rather
        // than at a busy address.
        if !matches!(self.sockets.borrow().get(id).map(|s| &s.state), Some(State::Idle)) {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let address = match requested {
            Requested::Inet { address, port } => {
                // Loopback and the wildcard are the two this container can
                // answer to. An address belonging to some other machine is
                // `EADDRNOTAVAIL`, which is what Linux says and what stops a
                // program believing it bound something it did not.
                if address != INADDR_ANY && address != INADDR_LOOPBACK {
                    return Outcome::Done(Errno::AddressUnavailable.as_result());
                }
                let port = match port {
                    // Port zero means "pick one", which a server that binds
                    // before it knows its port relies on.
                    0 => match self.sockets.borrow().ephemeral() {
                        Some(port) => port,
                        None => return Outcome::Done(Errno::AddressInUse.as_result()),
                    },
                    port => port,
                };
                Address::Inet { address, port }
            }
            Requested::Unix { path } => {
                // The node is created here, which is also the check: a name
                // already taken is `EADDRINUSE`, and `create` says `EEXIST`.
                let vnode = match self.create_socket_node(arguments.get(1) as i64 + 2, 0o755) {
                    Ok(vnode) => vnode,
                    Err(Errno::Exists) => {
                        return Outcome::Done(Errno::AddressInUse.as_result())
                    }
                    Err(errno) => return Outcome::Done(errno.as_result()),
                };
                Address::Unix { vnode, path }
            }
        };
        if self.sockets.borrow().bound(&address) {
            return Outcome::Done(Errno::AddressInUse.as_result());
        }
        if let Some(socket) = self.sockets.borrow_mut().get_mut(id) {
            socket.state = State::Bound(address);
        }
        Outcome::Done(0)
    }

    /// `listen(2)`.
    pub(crate) fn listen(&mut self, arguments: crate::syscall::Arguments) -> crate::syscall::Outcome {
        use crate::syscall::Outcome;
        let fd = arguments.get(0) as i32;
        let Some(id) = self.socket_of(fd) else {
            return Outcome::Done(self.not_a_socket(fd));
        };
        // Linux clamps rather than refuses, which is why the traced stack's
        // 2048 works on a machine whose `somaxconn` is smaller.
        let backlog = (arguments.get(1).max(0) as usize).min(MAX_BACKLOG).max(1);
        let mut sockets = self.sockets.borrow_mut();
        let Some(socket) = sockets.get_mut(id) else {
            return Outcome::Done(Errno::BadFile.as_result());
        };
        let address = match &socket.state {
            State::Bound(address) => address.clone(),
            // Listening twice only changes the backlog, which nginx does
            // on reload.
            State::Listening { .. } => {
                if let State::Listening { backlog: held, .. } = &mut socket.state {
                    *held = backlog;
                }
                return Outcome::Done(0);
            }
            // An unbound `listen` on Linux binds an ephemeral port first.
            State::Idle => Address::Inet {
                address: INADDR_LOOPBACK,
                port: 0,
            },
            State::Connected(_) => return Outcome::Done(Errno::AlreadyConnected.as_result()),
        };
        let address = match address {
            Address::Inet { address, port: 0 } => match sockets.ephemeral() {
                Some(port) => Address::Inet { address, port },
                None => return Outcome::Done(Errno::AddressInUse.as_result()),
            },
            address => address,
        };
        let Some(socket) = sockets.get_mut(id) else {
            return Outcome::Done(Errno::BadFile.as_result());
        };
        let port = address.port();
        socket.state = State::Listening {
            address,
            backlog,
            queue: std::collections::VecDeque::new(),
        };
        drop(sockets);
        // The host is told, so that a mapped port can start accepting. An
        // `AF_UNIX` listener has no port and never crosses.
        if let Some(port) = port {
            self.register_listener(port);
        }
        Outcome::Done(0)
    }

    /// `connect(2)`, which on loopback completes in one step.
    ///
    /// There is no handshake: the listener is a queue in the same arena, so
    /// connecting is making two rings and pushing the far end onto it. Which
    /// is why `EINPROGRESS` does not arise here — the plan's pitfall 4, and
    /// the reason it insists the asymmetry with the edge be stated.
    ///
    /// `SO_ERROR` *does* arise here, though, because nginx reads it after
    /// every connect whether or not one was needed. See the traced baseline.
    pub(crate) fn connect(&mut self, arguments: crate::syscall::Arguments) -> crate::syscall::Outcome {
        use crate::syscall::Outcome;
        let fd = arguments.get(0) as i32;
        let Some(id) = self.socket_of(fd) else {
            return Outcome::Done(self.not_a_socket(fd));
        };
        let requested = match self.address_at(arguments.get(1) as u64, arguments.get(2) as u64) {
            Ok(requested) => requested,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        match self.sockets.borrow().get(id).map(|socket| &socket.state) {
            Some(State::Connected(_)) => {
                return Outcome::Done(Errno::AlreadyConnected.as_result())
            }
            Some(State::Listening { .. }) => {
                return Outcome::Done(Errno::AlreadyConnected.as_result())
            }
            None => return Outcome::Done(Errno::BadFile.as_result()),
            _ => {}
        }
        let wanted = match requested {
            Requested::Inet { address, port } => {
                // A connection to any local address is a connection to this
                // container; there is nowhere else for it to go.
                let address = match address {
                    INADDR_ANY => INADDR_LOOPBACK,
                    address => address,
                };
                // And there is nowhere else at all. This container is a
                // namespace holding only `lo`: it is told of no interface,
                // so nothing outside `127.0.0.0/8` has a route, and the
                // answer is that there is no network rather than that the
                // far side refused. The distinction is what a client acts
                // on — "refused" means retry, "unreachable" means stop —
                // and `-p` publishes a port *inward*, which is a listener
                // the host reaches, never a route the guest can take out.
                if !loopback(address) {
                    return Outcome::Done(Errno::NetworkUnreachable.as_result());
                }
                Address::Inet { address, port }
            }
            Requested::Unix { path } => {
                // Resolved rather than created: a path that is not there is
                // `ENOENT`, which is exactly what glibc's `nscd` probe
                // reads before falling back to `/etc/passwd`.
                let root = self.vfs.root();
                let vnode = match self.vfs.resolve(root, &path, crate::vfs::Lookup::FOLLOW) {
                    Ok(vnode) => vnode,
                    Err(errno) => return Outcome::Done(errno.as_result()),
                };
                Address::Unix { vnode, path }
            }
        };
        let Some(listener) = self.sockets.borrow().listener_for(&wanted) else {
            // Nothing is listening there. For a unix socket whose node
            // exists this is `ECONNREFUSED`, and for one whose node does not
            // the resolve above already answered `ENOENT` — which is the
            // distinction a program probing for a daemon depends on.
            return Outcome::Done(Errno::ConnectionRefused.as_result());
        };
        // Two rings, crossed. Each starts with one reader and one writer,
        // which is one endpoint on each side of it.
        let up = self.rings.borrow_mut().create();
        let down = self.rings.borrow_mut().create();
        let mut sockets = self.sockets.borrow_mut();
        let local = match &wanted {
            Address::Inet { .. } => match sockets.ephemeral() {
                Some(port) => Address::Inet {
                    address: INADDR_LOOPBACK,
                    port,
                },
                None => Address::Unbound,
            },
            // A unix client is unnamed unless it bound a path of its own,
            // which is what `getsockname` on one answers.
            _ => Address::Unbound,
        };
        let (family, kind) = match sockets.get(id) {
            Some(socket) => (socket.family, socket.kind),
            None => return Outcome::Done(Errno::BadFile.as_result()),
        };
        // The far end: a socket nothing holds a descriptor for yet. Its one
        // reference is the accept queue's, and `accept4` hands it to a
        // descriptor without changing the count.
        let far = sockets.create(family, kind);
        if let Some(socket) = sockets.get_mut(far) {
            socket.state = State::Connected(Endpoint {
                receive: up,
                transmit: down,
                local: wanted.clone(),
                peer: local.clone(),
                read_shut: false,
                write_shut: false,
            });
attach_endpoint(&mut self.rings.borrow_mut(), up, down);
        }
        if let Err(errno) = sockets.enqueue(listener, far) {
            let retired = sockets.release(far);
            drop(sockets);
            let mut rings = self.rings.borrow_mut();
            detach_endpoint(&mut rings, retired);
            for ring in [up, down] {
                rings.release(ring, End::Read);
                rings.release(ring, End::Write);
            }
            return Outcome::Done(errno.as_result());
        }
        if let Some(socket) = sockets.get_mut(id) {
            socket.state = State::Connected(Endpoint {
                receive: down,
                transmit: up,
                local,
                peer: wanted,
                read_shut: false,
                write_shut: false,
            });
attach_endpoint(&mut self.rings.borrow_mut(), down, up);
        }
        Outcome::Done(0)
    }

    /// `accept(2)` and `accept4(2)`.
    pub(crate) fn accept(&mut self, number: i64, arguments: crate::syscall::Arguments) -> crate::syscall::Outcome {
        use crate::syscall::Outcome;
        let fd = arguments.get(0) as i32;
        let Some(id) = self.socket_of(fd) else {
            return Outcome::Done(self.not_a_socket(fd));
        };
        let flags = match number == crate::syscall::number::ACCEPT4 {
            true => arguments.get(3) as i32,
            false => 0,
        };
        if flags & !(kind::NONBLOCK | kind::CLOEXEC) != 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        if !matches!(
            self.sockets.borrow().get(id).map(|socket| &socket.state),
            Some(State::Listening { .. })
        ) {
            // `accept` on a socket that never listened is `EINVAL`, which is
            // how a program learns it forgot the `listen`.
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let waiting = crate::thread::Accepting {
            listener: id,
            at: arguments.get(1) as u64,
            length_at: arguments.get(2) as u64,
            flags,
        };
        match self.complete_accept(waiting) {
            Some(answer) => Outcome::Done(answer),
            None => {
                let blocking = self
                    .files
                    .description(fd)
                    .map(|file| file.flags & crate::file::open_flags::NONBLOCK == 0)
                    .unwrap_or(false);
                if !blocking {
                    return Outcome::Done(Errno::TryAgain.as_result());
                }
                if !self.machine.park_on_accept(waiting) {
                    return Outcome::Done(Errno::TryAgain.as_result());
                }
                Outcome::Blocked
            }
        }
    }

    /// Pops one connection off a listener's queue and gives it a descriptor,
    /// or answers `None` because the queue is empty.
    ///
    /// Separate from the row because a parked `accept` finishes here too —
    /// on the parked process's own turn, which is the rule every completion
    /// in this kernel obeys, because the address the peer is written to is
    /// that process's memory.
    pub(crate) fn complete_accept(&mut self, waiting: crate::thread::Accepting) -> Option<i64> {
        let accepted = self.sockets.borrow_mut().dequeue(waiting.listener)?;
        let peer = self
            .sockets
            .borrow()
            .endpoint(accepted)
            .map(|endpoint| endpoint.peer.clone())
            .unwrap_or_default();
        if let Err(errno) = self.write_address(waiting.at, waiting.length_at, &peer) {
            let retired = self.sockets.borrow_mut().release(accepted);
            detach_endpoint(&mut self.rings.borrow_mut(), retired);
            return Some(errno.as_result());
        }
        let flags = crate::file::open_flags::READ_WRITE
            | match waiting.flags & kind::NONBLOCK != 0 {
                true => crate::file::open_flags::NONBLOCK,
                false => 0,
            };
        match self.files.open(
            crate::fd::Backing::Socket(accepted),
            flags,
            waiting.flags & kind::CLOEXEC != 0,
        ) {
            Ok(fd) => Some(i64::from(fd)),
            Err(errno) => {
                let retired = self.sockets.borrow_mut().release(accepted);
                detach_endpoint(&mut self.rings.borrow_mut(), retired);
                Some(errno.as_result())
            }
        }
    }

    /// `getsockname(2)` and `getpeername(2)`.
    pub(crate) fn socket_address(&mut self, number: i64, arguments: crate::syscall::Arguments) -> crate::syscall::Outcome {
        use crate::syscall::Outcome;
        let fd = arguments.get(0) as i32;
        let Some(id) = self.socket_of(fd) else {
            return Outcome::Done(self.not_a_socket(fd));
        };
        let peer = number == crate::syscall::number::GETPEERNAME;
        let address = {
            let sockets = self.sockets.borrow();
            let Some(socket) = sockets.get(id) else {
                return Outcome::Done(Errno::BadFile.as_result());
            };
            match (&socket.state, peer) {
                (State::Connected(endpoint), true) => endpoint.peer.clone(),
                (State::Connected(endpoint), false) => endpoint.local.clone(),
                // A listener has no peer, and gunicorn asks — which is what
                // makes this arm a traced fact rather than a guess.
                (_, true) => return Outcome::Done(Errno::NotConnected.as_result()),
                (State::Bound(address), false) => address.clone(),
                (State::Listening { address, .. }, false) => address.clone(),
                (State::Idle, false) => Address::Unbound,
            }
        };
        match self.write_address(arguments.get(1) as u64, arguments.get(2) as u64, &address) {
            Ok(()) => Outcome::Done(0),
            Err(errno) => Outcome::Done(errno.as_result()),
        }
    }

    /// The errno for a descriptor that was handed to a socket row and is not
    /// a socket — which is a different fact from being closed.
    fn not_a_socket(&self, fd: i32) -> i64 {
        match self.files.is_open(fd) {
            true => Errno::NotSocket.as_result(),
            false => Errno::BadFile.as_result(),
        }
    }
}

/// The `send`/`recv` flags this kernel reads.
pub mod message {
    /// Read without consuming.
    pub const PEEK: i32 = 2;
    /// This one call does not block, whatever the descriptor says.
    pub const DONTWAIT: i32 = 0x40;
    /// A broken pipe is an errno and not a signal, for this call. Every
    /// server that writes to a socket it might outlive passes it.
    pub const NOSIGNAL: i32 = 0x4000;
}

impl<S: crate::abi::Store, M: crate::machine::Machine> crate::syscall::Kernel<'_, S, M> {
    /// `setsockopt(2)`.
    ///
    /// Mostly recording, and the honesty is in *which* ones do nothing and
    /// why rather than in a blanket zero. `SO_RCVBUF` and `SO_SNDBUF` are
    /// remembered but do not resize a ring yet; `TCP_NODELAY` and
    /// `SO_KEEPALIVE` cannot do anything, there being no TCP to tune; and
    /// `SO_REUSEADDR` has nothing to relax, because the `TIME_WAIT` it
    /// exists for is a TCP state this kernel does not have.
    pub(crate) fn setsockopt(&mut self, arguments: crate::syscall::Arguments) -> crate::syscall::Outcome {
        use crate::syscall::Outcome;
        let fd = arguments.get(0) as i32;
        let Some(id) = self.socket_of(fd) else {
            return Outcome::Done(self.not_a_socket(fd));
        };
        let level = arguments.get(1) as i32;
        let name = arguments.get(2) as i32;
        let at = arguments.get(3) as u64;
        let length = arguments.get(4) as u64;
        let value = match length >= 4 {
            true => {
                let mut bytes = [0u8; 4];
                if self.pages.read(at, &mut bytes).is_err() {
                    return Outcome::Done(Errno::Fault.as_result());
                }
                i32::from_le_bytes(bytes)
            }
            false => 0,
        };
        let mut sockets = self.sockets.borrow_mut();
        let Some(socket) = sockets.get_mut(id) else {
            return Outcome::Done(Errno::BadFile.as_result());
        };
        match (level, name) {
            (option::SOL_SOCKET, option::REUSEADDR) => socket.options.reuse_address = value != 0,
            (option::SOL_SOCKET, option::KEEPALIVE) => socket.options.keep_alive = value != 0,
            (option::SOL_SOCKET, option::RCVBUF) => socket.options.receive_buffer = value as u32,
            (option::SOL_SOCKET, option::SNDBUF) => socket.options.send_buffer = value as u32,
            (option::SOL_TCP, option::TCP_NODELAY) => socket.options.no_delay = value != 0,
            // Timeouts, which need a parked transfer racing a deadline. Named
            // rather than accepted: a program that sets a receive timeout
            // and is told it worked will wait forever when it fires.
            (option::SOL_SOCKET, option::RCVTIMEO | option::SNDTIMEO) => {
                return Outcome::Fault(crate::syscall::Fault::detailed(
                    crate::syscall::number::SETSOCKOPT,
                    arguments,
                    "`SO_RCVTIMEO`/`SO_SNDTIMEO`, which are a parked transfer \
                     racing a deadline, and nothing here races one",
                ));
            }
            // Everything else is recorded as accepted, which is what Linux
            // does for the options a protocol does not implement.
            _ => {}
        }
        Outcome::Done(0)
    }

    /// `getsockopt(2)`.
    pub(crate) fn getsockopt(&mut self, arguments: crate::syscall::Arguments) -> crate::syscall::Outcome {
        use crate::syscall::Outcome;
        let fd = arguments.get(0) as i32;
        let Some(id) = self.socket_of(fd) else {
            return Outcome::Done(self.not_a_socket(fd));
        };
        let level = arguments.get(1) as i32;
        let name = arguments.get(2) as i32;
        let at = arguments.get(3) as u64;
        let length_at = arguments.get(4) as u64;
        let answer = {
            let mut sockets = self.sockets.borrow_mut();
            let Some(socket) = sockets.get_mut(id) else {
                return Outcome::Done(Errno::BadFile.as_result());
            };
            match (level, name) {
                // Read once and cleared, which is how a non-blocking
                // `connect` reports what happened — and nginx reads it
                // after every connect whether or not one was needed.
                (option::SOL_SOCKET, option::ERROR) => {
                    let held = socket.options.error;
                    socket.options.error = 0;
                    held
                }
                (option::SOL_SOCKET, option::REUSEADDR) => i32::from(socket.options.reuse_address),
                (option::SOL_SOCKET, option::KEEPALIVE) => i32::from(socket.options.keep_alive),
                (option::SOL_SOCKET, option::RCVBUF) => socket.options.receive_buffer as i32,
                (option::SOL_SOCKET, option::SNDBUF) => socket.options.send_buffer as i32,
                (option::SOL_SOCKET, option::TYPE) => socket.kind,
                (option::SOL_SOCKET, option::DOMAIN) => socket.family,
                (option::SOL_SOCKET, option::PROTOCOL) => 0,
                (option::SOL_SOCKET, option::ACCEPTCONN) => {
                    i32::from(matches!(socket.state, State::Listening { .. }))
                }
                (option::SOL_TCP, option::TCP_NODELAY) => i32::from(socket.options.no_delay),
                _ => return Outcome::Done(Errno::NoProtocolOption.as_result()),
            }
        };
        if length_at != 0 {
            let mut room = [0u8; 4];
            if self.pages.read(length_at, &mut room).is_err() {
                return Outcome::Done(Errno::Fault.as_result());
            }
            let room = u32::from_le_bytes(room).min(4);
            if room > 0 && self.pages.write(at, &answer.to_le_bytes()[..room as usize]).is_err() {
                return Outcome::Done(Errno::Fault.as_result());
            }
            if self.pages.write(length_at, &4u32.to_le_bytes()).is_err() {
                return Outcome::Done(Errno::Fault.as_result());
            }
        }
        Outcome::Done(0)
    }

    /// `sendto(2)` and `recvfrom(2)`, which on a connected stream are
    /// `write` and `read` with flags.
    ///
    /// The address argument is what makes them separate calls, and on a
    /// stream socket Linux ignores it — there is one peer and it is already
    /// chosen. What is not ignored is the flags: `MSG_PEEK` reads without
    /// consuming, `MSG_DONTWAIT` makes this one call non-blocking whatever
    /// the descriptor says, and `MSG_NOSIGNAL` turns a broken pipe into an
    /// errno alone, which every server that writes to a socket it might
    /// outlive passes.
    pub(crate) fn send_receive(&mut self, number: i64, arguments: crate::syscall::Arguments) -> crate::syscall::Outcome {
        use crate::syscall::Outcome;
        let fd = arguments.get(0) as i32;
        let buffer = arguments.get(1) as u64;
        let count = arguments.get(2);
        let flags = arguments.get(3) as i32;
        if count < 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let sending = number == crate::syscall::number::SENDTO;
        let direction = match sending {
            true => End::Write,
            false => End::Read,
        };
        let (ring, mut open_flags) = match self.socket_ring(fd, direction) {
            Reach::Ring { ring, flags } => (ring, flags),
            Reach::Finished => {
                if sending {
                    return self.broken(flags);
                }
                return Outcome::Done(0);
            }
            Reach::Refused(errno) => {
                if sending && errno == crate::ring::BROKEN {
                    return self.broken(flags);
                }
                return Outcome::Done(errno.as_result());
            }
            Reach::Elsewhere => return Outcome::Done(self.not_a_socket(fd)),
        };
        if flags & message::PEEK != 0 {
            if sending {
                return Outcome::Done(Errno::Invalid.as_result());
            }
            return self.peek_ring(ring, buffer, count as u64);
        }
        if flags & message::DONTWAIT != 0 {
            open_flags |= crate::file::open_flags::NONBLOCK;
        }
        // `MSG_NOSIGNAL` suppresses the signal and not the errno, so the
        // suppression is recorded on the thread for the transfer to read
        // rather than folded into the flags.
        self.suppress_sigpipe(flags & message::NOSIGNAL != 0);
        let outcome = self.transfer_ring(ring, direction, open_flags, buffer, count as u64);
        self.suppress_sigpipe(false);
        outcome
    }

    /// A write to a direction this endpoint has given up: `EPIPE`, and a
    /// `SIGPIPE` unless the caller said not to.
    fn broken(&mut self, flags: i32) -> crate::syscall::Outcome {
        use crate::syscall::Outcome;
        const SIGPIPE: i32 = 13;
        if flags & message::NOSIGNAL == 0 && self.signal_process(SIGPIPE) {
            return Outcome::Exit(128 + SIGPIPE);
        }
        Outcome::Done(crate::ring::BROKEN.as_result())
    }

    fn peek_ring(&mut self, ring: u32, buffer: u64, count: u64) -> crate::syscall::Outcome {
        use crate::syscall::Outcome;
        if let Err(errno) = self.memory().check(buffer, count) {
            return Outcome::Done(errno.as_result());
        }
        let available = self.rings.borrow().queued(ring) as u64;
        let want = count.min(available) as usize;
        let mut bytes = vec![0u8; want];
        let seen = self.rings.borrow().peek(ring, &mut bytes);
        // SAFETY: the buffer was bounds-checked a moment ago.
        match unsafe { self.memory_mut().write(buffer, &bytes[..seen]) } {
            Ok(()) => Outcome::Done(seen as i64),
            Err(errno) => Outcome::Done(errno.as_result()),
        }
    }
}

/// `struct msghdr` on x86-64.
const MSGHDR: u64 = 56;
/// `struct iovec`: a pointer and a length.
const IOVEC: u64 = 16;

impl<S: crate::abi::Store, M: crate::machine::Machine> crate::syscall::Kernel<'_, S, M> {
    /// `sendmsg(2)` and `recvmsg(2)`.
    ///
    /// On a stream socket with no control data these are `sendto` and
    /// `recvfrom` with the buffer described indirectly, and that is the case
    /// the traced stack uses: nginx's master tells its worker to shut down
    /// over their channel socketpair, one `iovec`, no ancillary data.
    ///
    /// Control data is refused **by name**, because the only thing that
    /// arrives in it here is `SCM_RIGHTS` — descriptor passing, which nginx
    /// reaches the moment `worker_processes` exceeds one, and which is not
    /// built. Accepting the message
    /// and dropping the descriptors would be a worker that thinks it was
    /// handed a listener and was not.
    pub(crate) fn send_receive_message(
        &mut self,
        number: i64,
        arguments: crate::syscall::Arguments,
    ) -> crate::syscall::Outcome {
        use crate::syscall::Outcome;
        let fd = arguments.get(0) as i32;
        let at = arguments.get(1) as u64;
        let flags = arguments.get(2) as i32;
        let sending = number == crate::syscall::number::SENDMSG;

        let mut header = [0u8; MSGHDR as usize];
        if self.pages.read(at, &mut header).is_err() {
            return Outcome::Done(Errno::Fault.as_result());
        }
        let word = |offset: usize| {
            u64::from_le_bytes(header[offset..offset + 8].try_into().expect("eight bytes"))
        };
        // A *sender* with ancillary data is refused by name: the only thing
        // that arrives in it here is `SCM_RIGHTS` — descriptor passing,
        // which is not built — and a worker
        // that thinks it was handed a listener and was not is worse than one
        // told it could not be.
        //
        // A *receiver* offering a buffer is not the same thing at all, and
        // refusing it was wrong: nginx passes a control buffer on every
        // `recvmsg` on its worker channel, because it might be sent a
        // descriptor, and is perfectly happy to be told it was not. What
        // says so is a `msg_controllen` of zero on the way back.
        let control_length = word(40);
        if sending && word(32) != 0 && control_length != 0 {
            return Outcome::Fault(crate::syscall::Fault::detailed(
                number,
                arguments,
                "ancillary data on a sent message, which here means \
                 `SCM_RIGHTS` — descriptor passing, which is not built, and \
                 worse to accept and drop than to refuse",
            ));
        }
        let vectors = word(16);
        let count = word(24);
        if count > 1024 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        // No ancillary data arrived, and nothing was truncated — both of
        // which the caller reads back from the *header* rather than from the
        // return value.
        //
        // `msg_flags` is an out parameter and its contents on the way in are
        // the caller's business, so leaving it alone leaves whatever was in
        // that stack slot: nginx checks it for `MSG_CTRUNC` and logs
        // "recvmsg() truncated data" on every channel message when it is not
        // cleared. An out parameter that is only sometimes written is one
        // the caller cannot use.
        if !sending {
            if control_length != 0 && self.pages.write(at + 40, &0u64.to_le_bytes()).is_err() {
                return Outcome::Done(Errno::Fault.as_result());
            }
            if self.pages.write(at + 48, &0u32.to_le_bytes()).is_err() {
                return Outcome::Done(Errno::Fault.as_result());
            }
        }
        // The address a `sendmsg` names is ignored on a connected stream —
        // there is one peer and it was already chosen — and `recvmsg` fills
        // it in from the endpoint.
        if !sending {
            let name = word(0);
            let name_length = u32::from_le_bytes(header[8..12].try_into().expect("four bytes"));
            if name != 0 && name_length != 0 {
                let peer = self
                    .socket_of(fd)
                    .and_then(|id| self.sockets.borrow().endpoint(id).map(|e| e.peer.clone()))
                    .unwrap_or_default();
                // The length is written back into the header, where the
                // caller reads it.
                let _ = self.write_address_into(name, at + 8, &peer);
            }
        }
        // Each vector in turn, stopping at the first that cannot finish.
        // A stream may move less than it was offered, and a caller that
        // needs all of it loops — which is what makes this correct without
        // a scatter/gather transfer that could park half-way through.
        let mut moved = 0i64;
        for index in 0..count {
            let mut vector = [0u8; IOVEC as usize];
            if self
                .pages
                .read(vectors + index * IOVEC, &mut vector)
                .is_err()
            {
                return Outcome::Done(Errno::Fault.as_result());
            }
            let base = u64::from_le_bytes(vector[0..8].try_into().expect("eight bytes"));
            let length = u64::from_le_bytes(vector[8..16].try_into().expect("eight bytes"));
            if length == 0 {
                continue;
            }
            // Anything already moved turns a would-block into a short
            // transfer, which is what the caller is entitled to see.
            let piece_flags = match moved > 0 {
                true => flags | message::DONTWAIT,
                false => flags,
            };
            let outcome = self.send_receive(
                match sending {
                    true => crate::syscall::number::SENDTO,
                    false => crate::syscall::number::RECVFROM,
                },
                crate::syscall::Arguments::new([
                    i64::from(fd),
                    base as i64,
                    length as i64,
                    i64::from(piece_flags),
                    0,
                    0,
                ]),
            );
            match outcome {
                Outcome::Done(piece) if piece >= 0 => {
                    moved += piece;
                    // A short piece means the ring is empty or full; asking
                    // for the next one would answer zero and look like end
                    // of file.
                    if (piece as u64) < length {
                        break;
                    }
                }
                Outcome::Done(errno) => {
                    return match moved > 0 {
                        true => Outcome::Done(moved),
                        false => Outcome::Done(errno),
                    };
                }
                other => return other,
            }
        }
        Outcome::Done(moved)
    }

    /// The same as `write_address`, with the length in a `msghdr` field.
    fn write_address_into(&mut self, at: u64, length_at: u64, address: &Address) -> Result<(), Errno> {
        self.write_address(at, length_at, address)
    }
}

impl<S: crate::abi::Store, M: crate::machine::Machine> crate::syscall::Kernel<'_, S, M> {
    /// Tells the host a listener exists, and records whether it answered.
    ///
    /// A mapped port opens a real host socket and the guest becomes
    /// reachable from outside; an unmapped one is loopback-only. **Neither
    /// is an error and the guest cannot tell**, which is exactly what
    /// `docker` without `-p` does — and a container with no `/iso/net` mount
    /// at all simply has no edge, which is the whole of the capability
    /// model.
    /// Nothing is read back, and that is not a shortcut. Whether the port
    /// was mapped changes nothing this kernel does: an unmapped one simply
    /// never produces an event, so the listener is loopback-only by having
    /// nothing arrive on it. A guest that could tell the difference would be
    /// a guest that can see its own port mapping, which `docker` does not
    /// give it either.
    fn register_listener(&mut self, port: u16) {
        let _ = self
            .store
            .write(crate::paths::NET_LISTEN, port.to_string().as_bytes());
    }

    /// Moves bytes across the edge, in both directions, and turns the host's
    /// events into arena state.
    ///
    /// **Kernel state only.** Rings, the socket arena, the store — never any
    /// process's memory — which is what lets this run on any turn without
    /// breaking the rule the whole process table is built on. It runs at two
    /// points that are a pure function of execution: when a process has used
    /// a whole slice, and when nothing in the container can run.
    pub fn pump(&mut self, waiting: Option<u64>) -> bool {
        // Out first, so a response written just before the container went
        // idle is on its way before anything waits — and on its own, every
        // quantum, from the run loop.
        let mut moved = self.flush_edges();
        // Then in. A wait is the same read with a deadline on it, which is
        // the one store read allowed to take time.
        let mut batch = Vec::new();
        let present = match waiting {
            Some(milliseconds) => {
                let path: Vec<Vec<u8>> = vec![
                    b"iso".to_vec(),
                    b"net".to_vec(),
                    b"wait".to_vec(),
                    milliseconds.to_string().into_bytes(),
                ];
                let borrowed: Vec<&[u8]> = path.iter().map(|held| held.as_slice()).collect();
                self.store.read(&borrowed, &mut batch)
            }
            None => self.store.read(crate::paths::NET_EVENTS, &mut batch),
        };
        if present != crate::abi::StoreOutcome::Present {
            return moved;
        }
        for line in batch.split(|byte| *byte == b'\n') {
            if self.handle_event(line) {
                moved = true;
            }
        }
        moved
    }

    /// Sends whatever the guest has written and the host has not yet been
    /// given, and answers whether anything moved.
    ///
    /// Split out of [`Self::pump`] because the two halves want completely
    /// different frequencies. Sending is what a *response* waits on, so it
    /// wants to happen often; the inbound read is a host call with a
    /// deadline on it, and doing that sixteen times as often cost more
    /// throughput than the promptness was worth — four concurrent clients
    /// measured slower, because this work scales with the number of open
    /// connections and the frequency multiplied it.
    ///
    /// The early return matters for the same reason: most containers have
    /// no edge at all, and this now runs every quantum in every one of them.
    pub fn flush_edges(&mut self) -> bool {
        let edges = self.sockets.borrow().edges();
        if edges.is_empty() {
            return false;
        }
        let mut moved = false;
        for (id, conn) in edges {
            let Some(ring) = self
                .sockets
                .borrow()
                .endpoint(id)
                .map(|endpoint| endpoint.transmit)
            else {
                continue;
            };
            let queued = self.rings.borrow().queued(ring);
            if queued == 0 {
                continue;
            }
            let mut bytes = vec![0u8; queued];
            self.rings.borrow_mut().take(ring, &mut bytes);
            let path = edge_path(conn, b"tx");
            let borrowed: Vec<&[u8]> = path.iter().map(|held| held.as_slice()).collect();
            self.store.write(&borrowed, &bytes);
            moved = true;
        }
        moved
    }

    /// Tells the host an edge connection is over, having first sent
    /// whatever the guest wrote and had not sent yet.
    ///
    /// The flush is the point. A program that writes a reply and closes has
    /// put those bytes in a ring, and closing is what releases the ring —
    /// so a `close` that did not drain first would discard exactly the
    /// answer the connection existed to deliver. Linux sends the queued
    /// data and then the FIN, and this is that, over the store.
    ///
    /// `how` is `close` for both directions or `shutdown` for the write
    /// half, which the host turns into the same two things a kernel does.
    pub(crate) fn end_edge(&mut self, conn: u32, ring: u32, how: &[u8]) {
        let queued = self.rings.borrow().queued(ring);
        if queued > 0 {
            let mut bytes = vec![0u8; queued];
            self.rings.borrow_mut().take(ring, &mut bytes);
            let path = edge_path(conn, b"tx");
            let borrowed: Vec<&[u8]> = path.iter().map(|held| held.as_slice()).collect();
            self.store.write(&borrowed, &bytes);
        }
        let path = edge_path(conn, b"ctl");
        let borrowed: Vec<&[u8]> = path.iter().map(|held| held.as_slice()).collect();
        self.store.write(&borrowed, how);
    }

    /// One line of the host's event stream.
    fn handle_event(&mut self, line: &[u8]) -> bool {
        let mut words = line.split(|byte| *byte == b' ');
        let kind = words.next().unwrap_or(b"");
        let number = |held: Option<&[u8]>| -> Option<u32> {
            core::str::from_utf8(held?).ok()?.trim().parse().ok()
        };
        match kind {
            b"open" => {
                let (Some(conn), Some(port)) = (number(words.next()), number(words.next())) else {
                    return false;
                };
                // Who connected, which `accept4` and `getpeername` answer
                // with — and which nginx writes into its access log, so a
                // container whose log says `unix:` for every request is one
                // that dropped this on the floor.
                let peer = words.next().and_then(peer_address).unwrap_or_default();
                self.accept_edge(conn, port as u16, peer)
            }
            b"data" => {
                let Some(conn) = number(words.next()) else {
                    return false;
                };
                self.drain_edge(conn)
            }
            b"eof" => {
                let Some(conn) = number(words.next()) else {
                    return false;
                };
                // Take what is left, then let go of the writing end — which
                // makes the guest's next read drain and then return zero,
                // exactly as a pipe whose last writer closed does.
                self.drain_edge(conn);
                let Some(id) = self.socket_for_edge(conn) else {
                    return false;
                };
                let receive = self
                    .sockets
                    .borrow()
                    .endpoint(id)
                    .map(|endpoint| endpoint.receive);
                if let Some(receive) = receive {
                    self.rings.borrow_mut().release(receive, End::Write);
                }
                true
            }
            _ => false,
        }
    }

    /// A connection the host accepted, materialized as one the guest can.
    ///
    /// From here it is indistinguishable from a loopback connection, which
    /// is the boundary the whole design turns on: **everything above the rings
    /// is one code path**.
    fn accept_edge(&mut self, conn: u32, port: u16, peer: Address) -> bool {
        let wanted = Address::Inet {
            address: INADDR_LOOPBACK,
            port,
        };
        let Some(listener) = self.sockets.borrow().listener_for(&wanted) else {
            return false;
        };
        let up = self.rings.borrow_mut().create();
        let down = self.rings.borrow_mut().create();
        let mut sockets = self.sockets.borrow_mut();
        let (family, kind) = match sockets.get(listener) {
            Some(socket) => (socket.family, socket.kind),
            None => return false,
        };
        let accepted = sockets.create(family, kind);
        if let Some(socket) = sockets.get_mut(accepted) {
            socket.edge = Some(conn);
            socket.state = State::Connected(Endpoint {
                // The guest reads what the host wrote, and writes what the
                // host will read.
                receive: down,
                transmit: up,
                local: wanted,
                peer,
                read_shut: false,
                write_shut: false,
            });
attach_endpoint(&mut self.rings.borrow_mut(), down, up);
        }
        sockets.enqueue(listener, accepted).is_ok()
    }

    /// Takes what the host has for a connection, up to the ring's room.
    ///
    /// The room is *in the path*, so the host never delivers more than
    /// there is space for — which is how the guest's backpressure reaches
    /// all the way back to whoever is talking to us.
    fn drain_edge(&mut self, conn: u32) -> bool {
        let Some(id) = self.socket_for_edge(conn) else {
            return false;
        };
        let Some(receive) = self
            .sockets
            .borrow()
            .endpoint(id)
            .map(|endpoint| endpoint.receive)
        else {
            return false;
        };
        let room = self.rings.borrow().room(receive);
        if room == 0 {
            return false;
        }
        let path: Vec<Vec<u8>> = vec![
            b"iso".to_vec(),
            b"net".to_vec(),
            b"conn".to_vec(),
            conn.to_string().into_bytes(),
            b"rx".to_vec(),
            room.to_string().into_bytes(),
        ];
        let borrowed: Vec<&[u8]> = path.iter().map(|held| held.as_slice()).collect();
        let mut bytes = Vec::new();
        if self.store.read(&borrowed, &mut bytes) != crate::abi::StoreOutcome::Present
            || bytes.is_empty()
        {
            return false;
        }
        self.rings.borrow_mut().give(receive, &bytes);
        true
    }

    fn socket_for_edge(&self, conn: u32) -> Option<u32> {
        self.sockets
            .borrow()
            .edges()
            .into_iter()
            .find(|(_, edge)| *edge == conn)
            .map(|(id, _)| id)
    }
}

/// Parses the `address:port` the host reports a peer as.
///
/// Faithful live *and under replay*, because it arrived on the tape: the
/// address is a store answer like any other, so a recorded run reproduces
/// the same one.
fn peer_address(text: &[u8]) -> Option<Address> {
    let text = core::str::from_utf8(text).ok()?.trim();
    let (address, port) = text.rsplit_once(':')?;
    // IPv6 arrives bracketed and this kernel has no `AF_INET6`; reporting an
    // unnamed peer is better than reporting half an address as a whole one.
    if address.contains(':') {
        return None;
    }
    let mut packed = 0u32;
    let mut parts = 0;
    for part in address.split('.') {
        packed = (packed << 8) | u32::from(part.parse::<u8>().ok()?);
        parts += 1;
    }
    if parts != 4 {
        return None;
    }
    Some(Address::Inet {
        address: packed,
        port: port.parse().ok()?,
    })
}
