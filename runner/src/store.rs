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
}

impl Sink {
    pub fn new() -> Self {
        Self::default()
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

impl Store for Sink {
    fn read(&mut self, path: &[Vec<u8>]) -> Result<Option<Vec<u8>>, String> {
        Ok(self
            .entries
            .iter()
            .find(|(key, _)| key == path)
            .map(|(_, bytes)| bytes.clone()))
    }

    fn write(&mut self, path: &[Vec<u8>], data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        self.slot(path).extend_from_slice(data);
        Ok(path.to_vec())
    }
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
        match self.resolve(path) {
            Some(store) => store.read(path),
            None => Err(format!("nothing is mounted at {}", render(path))),
        }
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
