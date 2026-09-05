//! The edge: real TCP, terminated by the host.
//!
//! The split this rests on: a
//! connection with both ends in the guest never comes here — that is kernel
//! state, rings in an arena — and a connection with one end outside is a
//! *stream* the host terminates and hands across as bytes. Neither is a
//! packet, which is why there is no netstack in this project and no
//! ambition to write one.
//!
//! The boundary stays the two `ll-store` imports. Everything below is paths
//! read and written:
//!
//! ```text
//! write /iso/net/listen        <guest port>   -> a result path saying mapped or not
//! read  /iso/net/events                       -> a batch of lines, or nothing
//! read  /iso/net/conn/{j}/rx/{room}           -> up to {room} bytes
//! write /iso/net/conn/{j}/tx   <bytes>
//! write /iso/net/conn/{j}/ctl  <"shutdown"|"close">
//! ```
//!
//! The room a read may return is *in the path* rather than assumed, so the
//! host never delivers more than the guest's ring can hold — which is how
//! real TCP backpressure reaches all the way back to whoever is talking to
//! us.
//!
//! # Threads, and why the guest cannot see them
//!
//! Accepting and reading block, so they happen on host threads. What the
//! guest sees is one serialized stream of store answers, and the
//! serialization is what keeps the tape a single stream: every answer is
//! taken from one queue under one lock, at points that are a pure function
//! of the guest's own execution. Record the answers and a replay reproduces
//! the run, which is the same claim the clock and the random seed make.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};

use crate::store::Store;

/// What the host has to tell the guest about.
///
/// Lines rather than a structure, because the boundary carries bytes and a
/// format nobody has to agree on twice is worth more here than a compact
/// one. A batch is however many have accumulated since the last read.
#[derive(Debug)]
enum Event {
    /// A connection arrived on a mapped port.
    Open { conn: u32, port: u16, peer: String },
    /// Bytes are waiting, or room has appeared, or the peer is done.
    Data(u32),
    Eof(u32),
}

impl Event {
    fn render(&self) -> String {
        match self {
            Event::Open { conn, port, peer } => format!("open {conn} {port} {peer}\n"),
            Event::Data(conn) => format!("data {conn}\n"),
            Event::Eof(conn) => format!("eof {conn}\n"),
        }
    }
}

/// One accepted connection, from the store's side.
struct Connection {
    /// Bytes the host has read and the guest has not taken.
    incoming: VecDeque<u8>,
    /// The write half, for `tx`. A clone of the reader thread's stream.
    outgoing: Option<TcpStream>,
    /// The peer is done sending, and `incoming` is all there will be.
    finished: bool,
}

#[derive(Default)]
struct Net {
    events: VecDeque<Event>,
    connections: Vec<Option<Connection>>,
    /// Guest ports a listener has been opened for, so a second `listen` on
    /// one does not open a second host socket.
    listening: Vec<u16>,
}

impl Net {
    fn connection(&mut self, id: u32) -> Option<&mut Connection> {
        self.connections.get_mut(id as usize)?.as_mut()
    }
}

/// `/iso/net`, backed by the host's own sockets.
pub struct NetStore {
    net: Arc<Mutex<Net>>,
    /// Something to wake the wait read; see `wait`.
    arrival: Arc<Condvar>,
    /// `-p HOST:GUEST`, the docker convention, living host-side as
    /// configuration — which is what the capability model always said the
    /// firewall was.
    map: Vec<(u16, u16)>,
}

impl NetStore {
    pub fn new(map: Vec<(u16, u16)>) -> Self {
        Self {
            net: Arc::new(Mutex::new(Net::default())),
            arrival: Arc::new(Condvar::new()),
            map,
        }
    }

    /// Opens a host listener for a guest port, if the map has one.
    ///
    /// Answers whether it did. An unmapped port is not an error: the guest
    /// binds and listens happily and its listener is loopback-only, which
    /// is exactly what a container without `-p` does.
    fn listen(&mut self, guest: u16) -> bool {
        let Some((host, _)) = self.map.iter().find(|(_, mapped)| *mapped == guest).copied() else {
            return false;
        };
        {
            let mut net = self.net.lock().expect("the net lock");
            if net.listening.contains(&guest) {
                return true;
            }
            net.listening.push(guest);
        }
        let Ok(listener) = TcpListener::bind(("0.0.0.0", host)) else {
            eprintln!("zaqaru: could not listen on host port {host}");
            return false;
        };
        let net = Arc::clone(&self.net);
        let arrival = Arc::clone(&self.arrival);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let peer = stream
                    .peer_addr()
                    .map(|address| address.to_string())
                    .unwrap_or_else(|_| String::from("0.0.0.0:0"));
                let Ok(writing) = stream.try_clone() else { continue };
                let id = {
                    let mut held = net.lock().expect("the net lock");
                    let id = held.connections.len() as u32;
                    held.connections.push(Some(Connection {
                        incoming: VecDeque::new(),
                        outgoing: Some(writing),
                        finished: false,
                    }));
                    held.events.push_back(Event::Open {
                        conn: id,
                        port: guest,
                        peer,
                    });
                    id
                };
                arrival.notify_all();
                read_into(Arc::clone(&net), Arc::clone(&arrival), id, stream);
            }
        });
        true
    }

    /// Blocks for up to `milliseconds` waiting for something to report.
    ///
    /// The one store read allowed to take time: nothing in the container is
    /// runnable, so
    /// the alternative is a spin. A blocking read is observationally a slow
    /// store — nothing about the ABI changes, the host function simply does
    /// not return yet.
    fn wait(&mut self, milliseconds: u64) -> Option<Vec<u8>> {
        let net = self.net.lock().expect("the net lock");
        let (net, _) = self
            .arrival
            .wait_timeout_while(
                net,
                std::time::Duration::from_millis(milliseconds),
                |net| net.events.is_empty(),
            )
            .expect("the net lock");
        drain(net)
    }
}

/// Reads a connection until the peer is done, on its own thread.
fn read_into(net: Arc<Mutex<Net>>, arrival: Arc<Condvar>, id: u32, mut stream: TcpStream) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => {
                    let mut held = net.lock().expect("the net lock");
                    if let Some(connection) = held.connection(id) {
                        connection.finished = true;
                    }
                    held.events.push_back(Event::Eof(id));
                    drop(held);
                    arrival.notify_all();
                    return;
                }
                Ok(read) => {
                    let mut held = net.lock().expect("the net lock");
                    if let Some(connection) = held.connection(id) {
                        connection.incoming.extend(&chunk[..read]);
                    }
                    held.events.push_back(Event::Data(id));
                    drop(held);
                    arrival.notify_all();
                }
            }
        }
    });
}

fn drain(mut net: std::sync::MutexGuard<'_, Net>) -> Option<Vec<u8>> {
    if net.events.is_empty() {
        return None;
    }
    let mut batch = String::new();
    while let Some(event) = net.events.pop_front() {
        batch.push_str(&event.render());
    }
    Some(batch.into_bytes())
}

impl Store for NetStore {
    fn read(&mut self, path: &[Vec<u8>]) -> Result<Option<Vec<u8>>, String> {
        match path.get(2).map(Vec::as_slice) {
            Some(b"events") => {
                let net = self.net.lock().expect("the net lock");
                Ok(drain(net))
            }
            // `/iso/net/wait/{ms}`: the same batch, but the answer may take
            // up to that long to arrive.
            Some(b"wait") => {
                let milliseconds = path
                    .get(3)
                    .and_then(|held| std::str::from_utf8(held).ok())
                    .and_then(|text| text.parse::<u64>().ok())
                    .unwrap_or(0);
                Ok(self.wait(milliseconds))
            }
            // `/iso/net/conn/{j}/rx/{room}`.
            Some(b"conn") => {
                let id = number(path.get(3))?;
                if path.get(4).map(Vec::as_slice) != Some(b"rx") {
                    return Ok(None);
                }
                let room = path
                    .get(5)
                    .and_then(|held| std::str::from_utf8(held).ok())
                    .and_then(|text| text.parse::<usize>().ok())
                    .unwrap_or(0);
                let mut net = self.net.lock().expect("the net lock");
                let Some(connection) = net.connection(id) else {
                    return Ok(None);
                };
                let taken = room.min(connection.incoming.len());
                let bytes: Vec<u8> = connection.incoming.drain(..taken).collect();
                Ok(Some(bytes))
            }
            _ => Ok(None),
        }
    }

    fn write(&mut self, path: &[Vec<u8>], data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        match path.get(2).map(Vec::as_slice) {
            Some(b"listen") => {
                let port = std::str::from_utf8(data)
                    .ok()
                    .and_then(|text| text.trim().parse::<u16>().ok())
                    .ok_or_else(|| String::from("a listen for something that is not a port"))?;
                let mapped = self.listen(port);
                // The result path says which it was, and the guest reads
                // only that: a mapped listener is reachable from outside,
                // and an unmapped one is loopback-only. Neither is an error.
                Ok(vec![
                    b"iso".to_vec(),
                    b"net".to_vec(),
                    match mapped {
                        true => b"listener".to_vec(),
                        false => b"loopback".to_vec(),
                    },
                    port.to_string().into_bytes(),
                ])
            }
            Some(b"conn") => {
                let id = number(path.get(3))?;
                let mut net = self.net.lock().expect("the net lock");
                let Some(connection) = net.connection(id) else {
                    return Err(String::from("a connection that is not open"));
                };
                match path.get(4).map(Vec::as_slice) {
                    Some(b"tx") => {
                        if let Some(stream) = connection.outgoing.as_mut() {
                            // A failed write is a peer that has gone, which
                            // the `eof` from the reader thread already says
                            // or shortly will.
                            let _ = stream.write_all(data);
                            let _ = stream.flush();
                        }
                        Ok(path.to_vec())
                    }
                    Some(b"ctl") => {
                        match data {
                            b"shutdown" => {
                                if let Some(stream) = connection.outgoing.as_ref() {
                                    let _ = stream.shutdown(std::net::Shutdown::Write);
                                }
                            }
                            _ => {
                                // The reader thread holds a clone of this
                                // socket, so letting go of ours closes
                                // nothing: `shutdown` is what actually
                                // sends the end of file the peer is
                                // waiting for, and it is what ends the
                                // reader rather than leaving it parked on
                                // a connection nobody is on the other end
                                // of. A guest that closes a connection has
                                // closed it.
                                if let Some(stream) = connection.outgoing.as_ref() {
                                    let _ = stream.shutdown(std::net::Shutdown::Both);
                                }
                                connection.outgoing = None;
                                net.connections[id as usize] = None;
                            }
                        }
                        Ok(path.to_vec())
                    }
                    _ => Err(String::from("a connection operation with no name")),
                }
            }
            _ => Err(String::from("nothing at that path in `/iso/net`")),
        }
    }
}

fn number(segment: Option<&Vec<u8>>) -> Result<u32, String> {
    segment
        .and_then(|held| std::str::from_utf8(held).ok())
        .and_then(|text| text.parse::<u32>().ok())
        .ok_or_else(|| String::from("a connection identifier that is not a number"))
}
