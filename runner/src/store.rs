//! Stores and the mount table.
//!
//! The runner's whole job on the data side is to decide, once at boot, what
//! each `/iso` subtree is backed by. Nothing decides that again per
//! operation, which is what makes capability control configuration rather
//! than code: a container with no `/iso/net` mount has no network, and the
//! refusal is a missing mount, not a check somewhere in a hot path.

/// A store: bytes at paths, and no semantics beyond that.
///
/// Deliberately dumb. Symlinks, `..`, permissions, dirfd relativity — all of
/// POSIX — live in the kernel's resolution loop, never here. A store that
/// knew about any of it would be a second place for POSIX to be subtly
/// wrong.
pub trait Store: Send {
    /// `ok(some)` when the path holds bytes, `ok(none)` when it does not,
    /// `err` when the store itself failed.
    fn read(&mut self, path: &[Vec<u8>]) -> Result<Option<Vec<u8>>, String>;

    /// The result path a write settles at, which for an append-only sink is
    /// the path written.
    fn write(&mut self, path: &[Vec<u8>], data: &[u8]) -> Result<Vec<Vec<u8>>, String>;
}

/// A store that keeps every write and hands it back on read: the console and
/// the kernel log, and the thing a test reads to find out what happened.
#[derive(Default)]
pub struct Sink {
    entries: Vec<(Vec<Vec<u8>>, Vec<u8>)>,
    /// Where to echo writes as they arrive, if anywhere.
    ///
    /// Keeping every write and handing it back at the end is right for a
    /// program that ends. A *server* does not: its logs are how you find out
    /// what it is doing while it does it, and reading them back after exit
    /// is reading them back never. So the console can be teed, and what it
    /// is teed to is the embedder's decision rather than this store's.
    tee: Option<Tee>,
}

/// Which of the runner's own streams a sink echoes to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tee {
    /// By the last segment of the path: `stdout` to stdout and everything
    /// else to stderr, which keeps a container's own output separable from
    /// its diagnostics by the same rule the guest used.
    ByStream,
    /// Everything to standard error, for a log that must not be confused
    /// with the container's output.
    Diagnostics,
}

impl Sink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Puts a value where a guest will read it.
    ///
    /// A sink is normally something the guest fills and the host reads. This
    /// is the other direction, and it is how the host answers a question the
    /// guest asks — a configuration path is a mount whose content the
    /// embedder decided.
    pub fn place(&mut self, path: &[Vec<u8>], value: Vec<u8>) {
        *self.slot(path) = value;
    }

    fn slot(&mut self, path: &[Vec<u8>]) -> &mut Vec<u8> {
        if let Some(position) = self.entries.iter().position(|(key, _)| key == path) {
            return &mut self.entries[position].1;
        }
        self.entries.push((path.to_vec(), Vec::new()));
        let last = self.entries.len() - 1;
        &mut self.entries[last].1
    }
}

impl Sink {
    /// Echoes every write as it arrives, as well as keeping it.
    pub fn teed(mut self, tee: Tee) -> Self {
        self.tee = Some(tee);
        self
    }

    fn echo(&self, path: &[Vec<u8>], data: &[u8]) {
        use std::io::Write;
        let Some(tee) = self.tee else {
            return;
        };
        let text = String::from_utf8_lossy(data);
        let to_stdout = tee == Tee::ByStream
            && path.last().map(Vec::as_slice) == Some(b"stdout");
        match to_stdout {
            true => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            false => {
                eprint!("{text}");
                let _ = std::io::stderr().flush();
            }
        }
    }
}

impl Store for Sink {
    fn read(&mut self, path: &[Vec<u8>]) -> Result<Option<Vec<u8>>, String> {
        Ok(self
            .entries
            .iter()
            .find(|(key, _)| key == path)
            .map(|(_, bytes)| bytes.clone()))
    }

    fn write(&mut self, path: &[Vec<u8>], data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        self.echo(path, data);
        self.slot(path).extend_from_slice(data);
        Ok(path.to_vec())
    }
}

/// `/iso/shutdown`: the container asking how it ended, and the host asking
/// it to.
///
/// Two directions through one mount, which is the boundary's whole shape:
/// `complete` is a write the guest makes on its way out, and `requested` is
/// a read it makes to find out whether anybody wants it to stop.
///
/// **Polled, not pushed.** A signal handler runs on whatever thread the
/// operating system chose and may do almost nothing safely; it certainly
/// may not reach into a `wasmtime::Store` another thread is executing in.
/// So it sets a flag, and the guest reads it at the points it is already
/// asking the host things — which also makes the moment a shutdown is
/// noticed a function of the guest's own execution rather than of when a
/// signal happened to land.
#[derive(Default)]
pub struct Shutdown {
    recorded: Sink,
}

/// Set by the handler, read by the store. `static` because a signal handler
/// has no argument to carry anything in.
static REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn requested(_signal: i32) {
    REQUESTED.store(true, std::sync::atomic::Ordering::Relaxed);
}

impl Shutdown {
    /// Asks to be told about `SIGINT` and `SIGTERM`.
    ///
    /// Both, because a demo ends with Ctrl-C and a supervisor ends with
    /// `docker stop`, and the container should not be able to tell which
    /// happened to it.
    pub fn listening() -> Self {
        // SAFETY: installing a handler that does nothing but set an atomic,
        // which is the one thing a handler is allowed to do.
        unsafe {
            let handler = requested as *const () as libc::sighandler_t;
            libc::signal(libc::SIGINT, handler);
            libc::signal(libc::SIGTERM, handler);
        }
        Self::default()
    }

    /// Whether a stop has been asked for, for a host that wants to know
    /// without going through the guest.
    pub fn asked() -> bool {
        REQUESTED.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Store for Shutdown {
    fn read(&mut self, path: &[Vec<u8>]) -> Result<Option<Vec<u8>>, String> {
        if path.last().map(Vec::as_slice) == Some(b"requested") {
            return Ok(match Self::asked() {
                true => Some(b"1".to_vec()),
                // Absent rather than "0", because the guest asks this at
                // every slice boundary and an absent path is the cheaper
                // answer as well as the truer one: nothing has asked.
                false => None,
            });
        }
        self.recorded.read(path)
    }

    fn write(&mut self, path: &[Vec<u8>], data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        self.recorded.write(path, data)
    }
}

/// One thing the host was asked and what it said.
pub type Answer = (Vec<Vec<u8>>, Result<Option<Vec<u8>>, String>);

/// A tape, as bytes: a count, then each entry as its path segments and its
/// answer, every piece length-prefixed.
///
/// Deliberately dull. A tape is read by exactly one program and its value is
/// that it is exact, so the format optimises for having no way to be
/// ambiguous about a byte — not for being readable, which is what the
/// trace is for.
fn encode(entries: &[Answer]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let piece = |held: &[u8], into: &mut Vec<u8>| {
        into.extend_from_slice(&(held.len() as u32).to_le_bytes());
        into.extend_from_slice(held);
    };
    bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (path, answer) in entries {
        bytes.extend_from_slice(&(path.len() as u32).to_le_bytes());
        for segment in path {
            piece(segment, &mut bytes);
        }
        match answer {
            // Three kinds, and they are three different things a guest
            // branches on: bytes, nothing at that path, and a store that
            // could not answer at all.
            Ok(Some(held)) => {
                bytes.push(1);
                piece(held, &mut bytes);
            }
            Ok(None) => bytes.push(0),
            Err(why) => {
                bytes.push(2);
                piece(why.as_bytes(), &mut bytes);
            }
        }
    }
    bytes
}

type Entries = std::collections::VecDeque<Answer>;

fn decode(bytes: &[u8]) -> Result<Entries, String> {
    let mut at = 0;
    let word = |at: &mut usize| -> Result<usize, String> {
        if *at + 4 > bytes.len() {
            return Err(String::from("a tape that ends inside a length"));
        }
        let held = u32::from_le_bytes(bytes[*at..*at + 4].try_into().expect("four bytes"));
        *at += 4;
        Ok(held as usize)
    };
    let count = word(&mut at)?;
    let mut entries = Entries::new();
    for _ in 0..count {
        let segments = word(&mut at)?;
        let mut path = Vec::new();
        for _ in 0..segments {
            let length = word(&mut at)?;
            if at + length > bytes.len() {
                return Err(String::from("a tape that ends inside a path"));
            }
            path.push(bytes[at..at + length].to_vec());
            at += length;
        }
        if at >= bytes.len() {
            return Err(String::from("a tape that ends where an answer should be"));
        }
        let kind = bytes[at];
        at += 1;
        let held = |at: &mut usize| -> Result<Vec<u8>, String> {
            let length = word(at)?;
            if *at + length > bytes.len() {
                return Err(String::from("a tape that ends inside an answer"));
            }
            let taken = bytes[*at..*at + length].to_vec();
            *at += length;
            Ok(taken)
        };
        let answer = match kind {
            0 => Ok(None),
            1 => Ok(Some(held(&mut at)?)),
            2 => Err(String::from_utf8_lossy(&held(&mut at)?).into_owned()),
            other => return Err(format!("a tape entry of kind {other}")),
        };
        entries.push_back((path, answer));
    }
    Ok(entries)
}

/// What is mounted where, resolved by longest prefix.
///
/// Longest-prefix rather than exact match because a mount is a *subtree*:
/// `/iso/console` is one backend for `stdout` and `stderr` alike, and a
/// later `/iso/console/tty` mount would take precedence over both without
/// anything else changing.
#[derive(Default)]
pub struct MountTable {
    mounts: Vec<(Vec<Vec<u8>>, Box<dyn Store>)>,
    tape: Option<Tape>,
}

/// A record of every answer the host gave, or a replay of one.
///
/// **The design's determinism claim, made checkable.** Nothing inside the
/// container is nondeterministic: the schedule is a function of retired
/// instructions, the guest's instructions are a function of its own bytes,
/// and every input from outside — the clock, the entropy seed, the network,
/// a shutdown request — arrives as a store *read*. So the sequence of read
/// answers is the entire nondeterminism of a run, and recording it is
/// recording the run.
///
/// Reads only. A write leaves the container; replaying one would mean
/// sending the same bytes to a real socket a second time, which is not
/// reproducing a run, it is repeating an effect.
///
/// **Every answer, including a refusal.** A read of a path nothing is
/// mounted at fails, and that failure is an answer the guest acts on — an
/// unmounted `/iso/config/trace` is how a container learns it is not being
/// traced. Recording only the successful ones shifts every later entry by
/// one, and the replay then hands the guest its entropy seed where it asked
/// for a configuration flag. Which is precisely what happened the first time
/// this was written.
///
/// The path is recorded beside the answer and checked on replay. It is not
/// redundant: if a replayed run asks a *different* question than the
/// recorded one did, the determinism claim is false and this is where that
/// is found — loudly, at the first divergence, rather than as a run that
/// quietly goes somewhere else.
pub enum Tape {
    Recording {
        entries: Vec<Answer>,
        to: std::path::PathBuf,
    },
    Replaying {
        entries: std::collections::VecDeque<Answer>,
    },
}

impl MountTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mount(&mut self, prefix: &[&[u8]], store: Box<dyn Store>) {
        let prefix: Vec<Vec<u8>> = prefix.iter().map(|segment| segment.to_vec()).collect();
        self.mounts.push((prefix, store));
        // Longest first, so the first match found is the most specific one.
        self.mounts
            .sort_by(|left, right| right.0.len().cmp(&left.0.len()));
    }

    fn resolve(&mut self, path: &[Vec<u8>]) -> Option<&mut Box<dyn Store>> {
        self.mounts
            .iter_mut()
            .find(|(prefix, _)| path.starts_with(prefix))
            .map(|(_, store)| store)
    }

    /// Whether anything is mounted that would serve this path — the
    /// question a boot-time capability check asks.
    pub fn resolves(&self, path: &[Vec<u8>]) -> bool {
        self.mounts
            .iter()
            .any(|(prefix, _)| path.starts_with(prefix))
    }

    pub fn read(&mut self, path: &[Vec<u8>]) -> Result<Option<Vec<u8>>, String> {
        // A replay never reaches a store at all: the tape *is* the host.
        if let Some(Tape::Replaying { entries }) = &mut self.tape {
            let Some((asked, answer)) = entries.pop_front() else {
                return Err(format!(
                    "the tape ran out at {} — the replayed run asked more of \
                     the host than the recorded one did",
                    render(path)
                ));
            };
            if asked != path {
                return Err(format!(
                    "the tape says {} and the run asked {} — the replay \
                     diverged, which means something in the container is not \
                     a function of its inputs",
                    render(&asked),
                    render(path)
                ));
            }
            return answer;
        }
        let answer = match self.resolve(path) {
            Some(store) => store.read(path),
            None => Err(format!("nothing is mounted at {}", render(path))),
        };
        if let Some(Tape::Recording { entries, .. }) = &mut self.tape {
            entries.push((path.to_vec(), answer.clone()));
        }
        answer
    }

    /// Starts recording every answer, to be written when the run ends.
    pub fn record(&mut self, to: std::path::PathBuf) {
        self.tape = Some(Tape::Recording {
            entries: Vec::new(),
            to,
        });
    }

    /// Answers from a tape instead of from the stores.
    pub fn replay(&mut self, from: &std::path::Path) -> Result<(), String> {
        let bytes = std::fs::read(from).map_err(|error| format!("reading {from:?}: {error}"))?;
        Ok(self.tape = Some(Tape::Replaying {
            entries: decode(&bytes)?,
        }))
    }

    /// Writes a recording out, if one was being made. Answers how many
    /// answers it holds.
    pub fn keep_tape(&mut self) -> Option<Result<usize, String>> {
        let Some(Tape::Recording { entries, to }) = self.tape.take() else {
            return None;
        };
        let held = entries.len();
        Some(
            std::fs::write(&to, encode(&entries))
                .map(|()| held)
                .map_err(|error| format!("writing {to:?}: {error}")),
        )
    }

    pub fn write(&mut self, path: &[Vec<u8>], data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        match self.resolve(path) {
            Some(store) => store.write(path, data),
            None => Err(format!("nothing is mounted at {}", render(path))),
        }
    }
}

/// The host's clock, answering `/iso/time/realtime_ns` and
/// `/iso/time/monotonic_ns`.
///
/// Read on every access rather than sampled once, because that is what a
/// clock is. The two paths are different clocks and not two spellings of
/// one: the wall clock can be set backwards by the host at any moment, and
/// the monotonic one cannot, which is the whole reason a program picks
/// between them.
///
/// Monotonic time is measured from this store's construction, so a
/// container sees a clock that starts near zero when it boots. Linux
/// measures from its own boot and the difference is invisible to any
/// correct program: monotonic time promises an origin that does not move
/// during a run, never a particular origin.
pub struct Clock {
    started: std::time::Instant,
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock {
    pub fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
        }
    }
}

impl Store for Clock {
    fn read(&mut self, path: &[Vec<u8>]) -> Result<Option<Vec<u8>>, String> {
        let leaf = path.last().map(Vec::as_slice);
        let nanoseconds: i128 = match leaf {
            Some(b"realtime_ns") => {
                // Before 1970 is representable and the arithmetic says so,
                // rather than the host's clock being assumed sane.
                match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                    Ok(since) => since.as_nanos() as i128,
                    Err(before) => -(before.duration().as_nanos() as i128),
                }
            }
            Some(b"monotonic_ns") => self.started.elapsed().as_nanos() as i128,
            _ => return Ok(None),
        };
        Ok(Some(nanoseconds.to_string().into_bytes()))
    }

    fn write(&mut self, path: &[Vec<u8>], _data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        Err(format!(
            "{} is a clock; setting the host's time from inside a container \
             is not something this store does",
            render(path)
        ))
    }
}

/// A path as a diagnostic string. Segments are bytes, so anything that is
/// not valid UTF-8 is shown escaped rather than lost.
pub fn render(path: &[Vec<u8>]) -> String {
    let mut rendered = String::new();
    for segment in path {
        rendered.push('/');
        for byte in segment {
            match *byte {
                0x20..=0x7e => rendered.push(*byte as char),
                other => rendered.push_str(&format!("\\x{other:02x}")),
            }
        }
    }
    if rendered.is_empty() {
        rendered.push('/');
    }
    rendered
}
