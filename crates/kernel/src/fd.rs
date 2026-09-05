//! File descriptors, and the open file descriptions underneath them.
//!
//! The two-level structure is POSIX's and it is not decoration. A descriptor
//! is per-process and carries `O_CLOEXEC`; the *description* carries the file
//! offset and the open flags, and `dup` makes a second descriptor point at
//! the same one. That is why `dup2(fd, 1); lseek(1, …)` moves the original's
//! offset too, and why the famous epoll footgun — a registered descriptor
//! closed while a `dup` survives still fires — is a fact about descriptions
//! rather than a bug.
//!
//! Getting this wrong is invisible until something shares a descriptor, and
//! then it is invisible again until the offsets diverge. So it is built the
//! right way now, when it costs a struct.

use crate::errno::Errno;
use crate::mount::Vnode;

/// Linux's default `RLIMIT_NOFILE` soft limit. A guest that wants more asks
/// through `prlimit64`, which arrives with M6.
pub const MAX_DESCRIPTORS: usize = 1024;

/// Which console stream a descriptor names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Console {
    Input,
    Output,
    Error,
}

/// What sits behind a descriptor.
///
/// Three kinds, because there are three: a file in the baked image, a
/// console stream that crosses to the host, and a pipe. Every row that takes
/// a descriptor dispatches on this rather than assuming an inode — which is
/// what makes `write` go where the descriptor points instead of where its
/// *number* suggests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backing {
    /// A file in a mounted filesystem. Resolution happened once, at open;
    /// nothing re-resolves a path afterwards, which is what makes an
    /// unlinked-but-open file keep working — and why the descriptor holds a
    /// vnode rather than a mount-relative inode that could name a different
    /// file in a different filesystem.
    Image(Vnode),
    Console(Console),
    /// One end of a pipe: an index into the ring arena the whole process
    /// tree shares, and which side of it this descriptor holds. An index and
    /// not a pointer, so the descriptor stays `Copy` and a forked table is
    /// still a memcpy — the sharing lives in one place, which is
    /// [`crate::ring::Shared`].
    Pipe {
        ring: u32,
        end: crate::ring::End,
    },
    /// A socket: an index into the socket arena the whole process tree
    /// shares. Which rings it holds is *not* here, because a socket
    /// acquires them at `connect` or `accept` and a descriptor's backing
    /// does not change — the arena is asked. See [`crate::socket`].
    Socket(u32),
    /// An `eventfd`: a counter in the arena the process tree shares.
    Event(u32),
    /// An `epoll` instance, named the same way and shared for the same
    /// reason. A descriptor that *is* a set rather than a file is the whole
    /// idea of `epoll`, and it is why it needs a backing of its own.
    Epoll(u32),
}

/// What `dup` and friends share.
#[derive(Clone, Copy, Debug)]
pub struct OpenFile {
    pub backing: Backing,
    /// The read/write position, shared across every descriptor pointing here.
    pub offset: u64,
    /// The flags the file was opened with — the access mode and the
    /// per-description status bits, not `O_CLOEXEC`, which is per descriptor.
    pub flags: i32,
    /// How many descriptors point at this description.
    references: u32,
    /// The `flock` this description holds, or zero for none. On the
    /// description rather than the descriptor, which is where Linux puts it:
    /// a `dup` shares the lock, and closing any one of the descriptors
    /// releases it only when the last goes.
    lock: i32,
}

/// Where a directory scan stopped: the last name it returned, held inline.
///
/// Inline rather than in a `Vec<u8>`, because `getdents64` is on the path
/// the filesystem design promises is allocation-free and a scan writes this
/// on every batch. A name is bounded by `NAME_MAX`, so it fits a fixed
/// array, and the whole record costs one byte more than the longest name a
/// filesystem can hold.
#[derive(Clone, Copy)]
struct Resume {
    name: [u8; crate::file::MAX_NAME],
    length: u8,
}

impl Resume {
    fn as_slice(&self) -> &[u8] {
        &self.name[..self.length as usize]
    }
}

#[derive(Clone, Copy, Debug)]
struct Descriptor {
    description: usize,
    close_on_exec: bool,
}

#[derive(Default)]
#[derive(Clone)]
pub struct FdTable {
    descriptors: Vec<Option<Descriptor>>,
    /// The last name each directory scan returned, parallel to
    /// `descriptions`.
    ///
    /// A merged directory's listing is a union computed on demand, so an
    /// entry's *position* in it moves when a name is added or removed. A
    /// cookie that is a position therefore stops meaning what it meant:
    /// unlinking during a scan shifts everything after the hole down one,
    /// and the next batch skips the entry that moved into the position
    /// already consumed. `rm -r` does exactly that — readdir and unlink
    /// interleaved — and would silently leave files behind, then fail
    /// `rmdir` with `ENOTEMPTY`.
    ///
    /// A *name* does not move. Resuming after the last name returned gives
    /// back every entry that survived the change, exactly once, which is
    /// what POSIX requires of a scan.
    resume: Vec<Option<Resume>>,
    /// An arena rather than reference-counted pointers: a description's
    /// identity is its index, which is what a future `epoll` registration
    /// records and what fd hoisting at fork will migrate.
    descriptions: Vec<Option<OpenFile>>,
}

impl FdTable {
    /// A table with standard input, output and error already open on the
    /// console, as a process started by any Unix has.
    ///
    /// Not a convenience. With an empty table the guest's first `open`
    /// returns descriptor **0**, which every libc treats as standard input,
    /// and `fstat(1)` returns `EBADF` — which is what CPython checks at
    /// startup before setting `sys.stdout = None` and making every `print`
    /// a silent no-op. The failure has no error and no log line; it is
    /// simply no output.
    pub fn with_standard_streams() -> Self {
        let mut table = Self::default();
        for (expected, console) in [
            (0, Console::Input),
            (1, Console::Output),
            (2, Console::Error),
        ] {
            let flags = if console == Console::Input {
                0 // O_RDONLY
            } else {
                1 // O_WRONLY
            };
            let fd = table
                .open(Backing::Console(console), flags, false)
                .expect("an empty table has room for three descriptors");
            debug_assert_eq!(fd, expected, "the standard streams take 0, 1 and 2");
        }
        table
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Opens a description on an inode and returns the lowest free
    /// descriptor — "lowest free" being load-bearing, because the shell
    /// idiom `close(1); open(...)` depends on it.
    pub fn open(
        &mut self,
        backing: Backing,
        flags: i32,
        close_on_exec: bool,
    ) -> Result<i32, Errno> {
        let description = self.claim_description(OpenFile {
            backing,
            offset: 0,
            flags,
            references: 0,
            lock: 0,
        })?;
        match self.claim_descriptor(description, close_on_exec, 0) {
            Ok(fd) => Ok(fd),
            Err(errno) => {
                self.descriptions[description] = None;
                Err(errno)
            }
        }
    }

    /// Records where a directory scan stopped, so the next batch can pick
    /// up after it whatever has changed in between.
    pub fn set_resume(&mut self, fd: i32, name: &[u8]) -> Result<(), Errno> {
        if name.len() > crate::file::MAX_NAME {
            return Err(Errno::NameTooLong);
        }
        let index = self.descriptor(fd)?.description;
        let mut stored = Resume {
            name: [0; crate::file::MAX_NAME],
            length: name.len() as u8,
        };
        stored.name[..name.len()].copy_from_slice(name);
        // The slot was made when the description was, so this never grows
        // the table and never allocates.
        if let Some(slot) = self.resume.get_mut(index) {
            *slot = Some(stored);
        }
        Ok(())
    }

    /// Where a directory scan stopped, if it has started.
    pub fn resume(&self, fd: i32) -> Option<&[u8]> {
        let index = self.descriptor(fd).ok()?.description;
        self.resume.get(index)?.as_ref().map(Resume::as_slice)
    }

    /// Forgets where a scan stopped — what `rewinddir` and any other seek
    /// on a directory mean.
    pub fn clear_resume(&mut self, fd: i32) {
        let Ok(descriptor) = self.descriptor(fd) else {
            return;
        };
        if let Some(slot) = self.resume.get_mut(descriptor.description) {
            *slot = None;
        }
    }

    pub fn description(&self, fd: i32) -> Result<&OpenFile, Errno> {
        let index = self.descriptor(fd)?.description;
        self.descriptions[index].as_ref().ok_or(Errno::BadFile)
    }

    pub fn description_mut(&mut self, fd: i32) -> Result<&mut OpenFile, Errno> {
        let index = self.descriptor(fd)?.description;
        self.descriptions[index].as_mut().ok_or(Errno::BadFile)
    }

    /// Which open file description a descriptor names.
    ///
    /// The *description*, not the descriptor, is what `epoll` registers
    /// interest against — Linux's rule, and the reason a registered fd that
    /// is closed while a `dup` survives still fires. See
    /// [`crate::poll::Epolls`].
    pub fn description_index(&self, fd: i32) -> Result<usize, Errno> {
        let index = usize::try_from(fd).map_err(|_| Errno::BadFile)?;
        self.descriptors
            .get(index)
            .and_then(|held| *held)
            .map(|held| held.description)
            .ok_or(Errno::BadFile)
    }

    /// The description at an index, for whoever holds one rather than a
    /// descriptor.
    pub fn at(&self, description: usize) -> Option<&OpenFile> {
        self.descriptions.get(description)?.as_ref()
    }

    /// Every description the table still holds open.
    ///
    /// Used to notice when one has *gone*: a description's slot is reused,
    /// so anything holding an index has to be told when the file behind it
    /// is freed, or it will silently start watching whatever opens next.
    pub fn live_descriptions(&self) -> impl Iterator<Item = usize> + '_ {
        self.descriptions
            .iter()
            .enumerate()
            .filter_map(|(index, held)| held.as_ref().map(|_| index))
    }

    /// Every pipe end the table holds, once per *descriptor*.
    ///
    /// What a fork has to raise the counts by, and what an `execve` has to
    /// lower them by for the descriptors it closes: a pipe's readers and
    /// writers are counted in descriptors, so a table that is copied doubles
    /// them.
    pub fn pipe_ends(&self) -> impl Iterator<Item = (u32, crate::ring::End)> + '_ {
        self.descriptors
            .iter()
            .flatten()
            .filter_map(|descriptor| self.descriptions[descriptor.description].as_ref())
            .filter_map(|file| match file.backing {
                Backing::Pipe { ring, end } => Some((ring, end)),
                _ => None,
            })
    }

    /// The same for sockets. What *rings* those sockets hold is not here,
    /// because only the arena knows — see
    /// [`crate::syscall::Kernel::shared_census`].
    pub fn socket_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.descriptors
            .iter()
            .flatten()
            .filter_map(|descriptor| self.descriptions[descriptor.description].as_ref())
            .filter_map(|file| match file.backing {
                Backing::Socket(id) => Some(id),
                _ => None,
            })
    }

    /// The same for `epoll` instances, once per descriptor.
    pub fn epoll_sets(&self) -> impl Iterator<Item = u32> + '_ {
        self.descriptors
            .iter()
            .flatten()
            .filter_map(|descriptor| self.descriptions[descriptor.description].as_ref())
            .filter_map(|file| match file.backing {
                Backing::Epoll(id) => Some(id),
                _ => None,
            })
    }

    /// How many descriptors in this table are on one end of one pipe.
    ///
    /// Asked after a close, to decide whether the *process* has let go of
    /// that end — a `dup`ed pipe end is two descriptors and one of them
    /// going does not close anything.
    pub fn holds_pipe(&self, pipe: u32, end: crate::ring::End) -> usize {
        self.pipe_ends()
            .filter(|&(held, side)| held == pipe && side == end)
            .count()
    }

    /// Every open descriptor and a word for what is behind it, for the
    /// stall report — where "which descriptors did this process still have"
    /// is usually the answer.
    pub fn open_descriptors(&self) -> impl Iterator<Item = (i32, String)> + '_ {
        self.descriptors
            .iter()
            .enumerate()
            .filter_map(|(fd, held)| Some((fd as i32, (*held)?)))
            .filter_map(|(fd, held)| {
                let file = self.descriptions[held.description].as_ref()?;
                let what = match file.backing {
                    Backing::Image(vnode) => format!("file:{}", vnode.inode),
                    Backing::Console(stream) => format!("console:{stream:?}"),
                    Backing::Pipe { ring, end } => format!("pipe{ring}:{end:?}"),
                    Backing::Socket(id) => format!("socket{id}"),
                    Backing::Event(id) => format!("event{id}"),
                    Backing::Epoll(id) => format!("epoll{id}"),
                };
                Some((fd, what))
            })
    }

    /// The highest descriptor this table has open, or `None`.
    pub fn highest(&self) -> Option<i32> {
        self.descriptors
            .iter()
            .rposition(Option::is_some)
            .map(|slot| slot as i32)
    }

    /// Closes every descriptor marked close-on-exec.
    ///
    /// The moment the flag is named for, and the only one: a `fork` carries
    /// marked descriptors into the child untouched, because the flag says
    /// *exec* and a child that never execs keeps them. Which is why this is
    /// a separate call and not part of duplicating the table.
    pub fn close_marked(&mut self) {
        let marked: Vec<i32> = (0..self.descriptors.len() as i32)
            .filter(|&fd| self.close_on_exec(fd).unwrap_or(false))
            .collect();
        for fd in marked {
            let _ = self.close(fd);
        }
    }

    pub fn close_on_exec(&self, fd: i32) -> Result<bool, Errno> {
        Ok(self.descriptor(fd)?.close_on_exec)
    }

    pub fn set_close_on_exec(&mut self, fd: i32, value: bool) -> Result<(), Errno> {
        let slot = self
            .descriptors
            .get_mut(usize::try_from(fd).map_err(|_| Errno::BadFile)?)
            .and_then(Option::as_mut)
            .ok_or(Errno::BadFile)?;
        slot.close_on_exec = value;
        Ok(())
    }

    /// `dup`, and `fcntl(F_DUPFD)`: a second descriptor onto the same
    /// description, at or above `lowest`.
    pub fn duplicate(&mut self, fd: i32, lowest: i32, close_on_exec: bool) -> Result<i32, Errno> {
        let description = self.descriptor(fd)?.description;
        // A floor outside the descriptor space is `EINVAL`, not `EMFILE`:
        // Linux rejects the *argument* before it looks for room, so a caller
        // cannot tell "you asked for something impossible" from "the table is
        // full" if this answers the wrong one.
        let lowest = usize::try_from(lowest).map_err(|_| Errno::Invalid)?;
        if lowest >= MAX_DESCRIPTORS {
            return Err(Errno::Invalid);
        }
        self.claim_descriptor(description, close_on_exec, lowest)
    }

    /// `dup2`/`dup3`: a second descriptor at a number the caller chose,
    /// silently closing whatever was there.
    pub fn duplicate_to(
        &mut self,
        fd: i32,
        target: i32,
        close_on_exec: bool,
    ) -> Result<i32, Errno> {
        let description = self.descriptor(fd)?.description;
        let target_index = usize::try_from(target).map_err(|_| Errno::BadFile)?;
        if target_index >= MAX_DESCRIPTORS {
            return Err(Errno::BadFile);
        }
        if fd == target {
            // `dup2` with equal arguments is a no-op that still validates the
            // descriptor — and notably does *not* apply `O_CLOEXEC`, which is
            // why `dup3` makes it `EINVAL` instead.
            return Ok(target);
        }
        let _ = self.close(target);
        if self.descriptors.len() <= target_index {
            self.descriptors.resize(target_index + 1, None);
        }
        self.descriptors[target_index] = Some(Descriptor {
            description,
            close_on_exec,
        });
        self.retain(description);
        Ok(target)
    }

    pub fn close(&mut self, fd: i32) -> Result<(), Errno> {
        let index = usize::try_from(fd).map_err(|_| Errno::BadFile)?;
        let entry = self
            .descriptors
            .get_mut(index)
            .and_then(Option::take)
            .ok_or(Errno::BadFile)?;
        self.release(entry.description);
        Ok(())
    }

    /// Takes, replaces or releases the `flock` on a description.
    ///
    /// One process holds every lock, so nothing here can conflict — what the
    /// table records is the other half of `flock`'s contract: a second
    /// request replaces the first, and it is per *description*, so a `dup`
    /// shares it.
    pub fn set_lock(&mut self, fd: i32, operation: i32) -> Result<(), Errno> {
        let file = self.description_mut(fd)?;
        file.lock = if operation == 8 { 0 } else { operation };
        Ok(())
    }

    /// What lock a description holds, for the tests that check it is real.
    pub fn lock(&self, fd: i32) -> Result<i32, Errno> {
        Ok(self.description(fd)?.lock)
    }

    /// Whether any open description names an upper node.
    ///
    /// A scan rather than a reference count: the table holds at most a
    /// thousand descriptions, and this is asked on `unlink` and `close`
    /// rather than anywhere hot. Counting instead would put the descriptor
    /// table and the filesystem into a cycle in which neither owns the
    /// other.
    pub fn holds(&self, node: u32) -> bool {
        self.descriptions
            .iter()
            .flatten()
            .any(|file| matches!(file.backing, Backing::Image(vnode) if vnode.inode == node))
    }

    /// Whether a descriptor number is open at all.
    pub fn is_open(&self, fd: i32) -> bool {
        self.descriptor(fd).is_ok()
    }

    fn descriptor(&self, fd: i32) -> Result<Descriptor, Errno> {
        let index = usize::try_from(fd).map_err(|_| Errno::BadFile)?;
        self.descriptors
            .get(index)
            .copied()
            .flatten()
            .ok_or(Errno::BadFile)
    }

    /// Keeps the scan-position table the same length as the descriptions
    /// it parallels.
    ///
    /// Grown here rather than on first use, so that `getdents64` never
    /// allocates: it is on the path the filesystem design promises is free
    /// of that, and a table that grew mid-scan would put one allocation in
    /// the middle of it.
    fn reserve_resume(&mut self, description: usize) {
        if self.resume.len() <= description {
            self.resume.resize(description + 1, None);
        }
    }

    fn claim_description(&mut self, file: OpenFile) -> Result<usize, Errno> {
        if let Some(free) = self.descriptions.iter().position(Option::is_none) {
            self.descriptions[free] = Some(file);
            self.reserve_resume(free);
            return Ok(free);
        }
        self.descriptions.push(Some(file));
        let description = self.descriptions.len() - 1;
        self.reserve_resume(description);
        Ok(description)
    }

    fn claim_descriptor(
        &mut self,
        description: usize,
        close_on_exec: bool,
        lowest: usize,
    ) -> Result<i32, Errno> {
        let mut number = lowest;
        while number < self.descriptors.len() && self.descriptors[number].is_some() {
            number += 1;
        }
        if number >= MAX_DESCRIPTORS {
            return Err(Errno::TooManyFiles);
        }
        if number >= self.descriptors.len() {
            self.descriptors.resize(number + 1, None);
        }
        self.descriptors[number] = Some(Descriptor {
            description,
            close_on_exec,
        });
        self.retain(description);
        Ok(number as i32)
    }

    fn retain(&mut self, description: usize) {
        if let Some(file) = self.descriptions[description].as_mut() {
            file.references += 1;
        }
    }

    fn release(&mut self, description: usize) {
        let Some(file) = self.descriptions[description].as_mut() else {
            return;
        };
        file.references -= 1;
        if file.references == 0 {
            self.descriptions[description] = None;
            // The scan's position goes with the description, so a reused
            // slot cannot resume the previous directory's listing.
            if let Some(slot) = self.resume.get_mut(description) {
                *slot = None;
            }
        }
    }
}
