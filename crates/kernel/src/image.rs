//! The baked image: a packed binary index over a flat blob of file contents.
//!
//! The empirical trace that shaped this design made the filesystem 80% of a
//! real application's syscalls, and a large share of those are *misses* — an
//! interpreter walking its module path for files that are not there. So the
//! format is tuned for the thing that dominates: a negative lookup must be as
//! cheap as a hit, and `stat` must be an index multiply and some field copies
//! with no parsing and no allocation anywhere.
//!
//! That is why this is packed binary rather than the structfs-native records
//! the rest of the system speaks. A decode and an allocation inside each of a
//! thousand `stat`s is not a cost the working-first doctrine gets to defer.
//! The `/img` store *presents* this same data as ordinary structfs values for
//! tooling and diffing; the hot path walks bytes.
//!
//! Nothing here allocates and nothing here panics. Every accessor is bounds
//! checked against the region it reads and returns a named error, because a
//! malformed index is a bug in the packager and a bug that reads out of bounds
//! is one nobody can locate.
//!
//! "Bounds checked" means *checked at full width*. `usize` is 32 bits inside
//! the module, so `start + 2 > region.len()` with a `start` of `0xFFFF_FFFF`
//! wraps to `1`, passes, and then traps on the slice — a guard that reads
//! like a guard and is not one. Every offset here is therefore combined with
//! `checked_add`/`checked_mul` on a type wide enough to hold the sum, and
//! narrowed only after the range is known to fit.
//!
//! ```text
//! header    magic, version, region offsets and counts
//! inodes    fixed 64-byte records, one per inode (hardlinks share one)
//! dirents   per-directory sorted arrays, binary-searched by name
//! strings   length-prefixed byte strings: names, symlink targets,
//!           owner names, xattr names and values
//! xattrs    per-inode blocks referencing the strings region
//! command   the boot command line: NUL-separated argument strings
//! environ   the boot environment: NUL-separated `NAME=value` strings
//! workdir   the directory the container starts in, or nothing
//! ```
//!
//! The command line and the environment are regions rather than files
//! because they are facts about the container and not part of the guest's
//! filesystem — the same argument the process-and-thread model makes about
//! `/iso`, which the guest cannot see either. An OCI image config carries
//! `Env` beside `Cmd` for the same reason. The packager owns what they say; an
//! empty region means the default.
//!
//! The environment is not decoration. A program that cannot read `HOME`
//! does not fail — it takes a different path: CPython falls through
//! `posixpath.expanduser` into `pwd.getpwuid`, which is glibc's NSS, which
//! probes nscd over `AF_UNIX`. Measured on this machine, `python3 -c
//! 'print("hello")'` opens two sockets and reads `/etc/passwd` with an empty
//! environment and neither with `HOME` set. A container with no environment
//! is not a container with a smaller syscall surface; it is one with a
//! different and larger surface than the run it will be diffed against.

/// `KISI` — the kernel image.
pub const MAGIC: u32 = u32::from_le_bytes(*b"KISI");
pub const VERSION: u32 = 7;

pub const HEADER_SIZE: usize = 72;
/// Fixed and packed: `stat` is `inode_offset + index * INODE_SIZE`.
pub const INODE_SIZE: usize = 64;
pub const DIRENT_SIZE: usize = 12;

/// What went wrong reading the index. Every variant names a place, because
/// the only useful thing to say about a malformed image is where it broke.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImageError {
    /// Not an image, or one this build does not understand.
    BadMagic,
    UnsupportedVersion(u32),
    /// A region's declared extent runs past the index.
    TruncatedRegion(&'static str),
    /// An index into a region that does not contain it.
    OutOfRange(&'static str),
    /// A field whose value cannot be what it claims — a directory whose
    /// entry array overruns, a string whose length runs past its region.
    Malformed(&'static str),
    /// The operation does not apply to this kind of inode: reading a
    /// directory's contents, following a regular file as a symlink.
    WrongKind,
}

/// POSIX file-type bits, as they appear in `mode`.
pub mod file_type {
    pub const MASK: u32 = 0o170000;
    pub const FIFO: u32 = 0o010000;
    pub const CHARACTER: u32 = 0o020000;
    pub const DIRECTORY: u32 = 0o040000;
    pub const BLOCK: u32 = 0o060000;
    pub const REGULAR: u32 = 0o100000;
    pub const SYMLINK: u32 = 0o120000;
    pub const SOCKET: u32 = 0o140000;
}

/// `getdents64`'s `d_type`, precomputed at bake time.
///
/// Not decoration: CPython's importer uses `d_type` to decide whether to
/// `stat` an entry at all, so having it right removes calls rather than
/// answering them.
pub mod directory_entry_type {
    pub const UNKNOWN: u8 = 0;
    pub const FIFO: u8 = 1;
    pub const CHARACTER: u8 = 2;
    pub const DIRECTORY: u8 = 4;
    pub const BLOCK: u8 = 6;
    pub const REGULAR: u8 = 8;
    pub const SYMLINK: u8 = 10;
    pub const SOCKET: u8 = 12;

    /// The `d_type` a mode implies. The one mapping, used by the packager when
    /// it writes the entry and available to anything that needs to agree.
    pub fn of_mode(mode: u32) -> u8 {
        match mode & super::file_type::MASK {
            super::file_type::FIFO => FIFO,
            super::file_type::CHARACTER => CHARACTER,
            super::file_type::DIRECTORY => DIRECTORY,
            super::file_type::BLOCK => BLOCK,
            super::file_type::REGULAR => REGULAR,
            super::file_type::SYMLINK => SYMLINK,
            super::file_type::SOCKET => SOCKET,
            _ => UNKNOWN,
        }
    }
}

/// Packager-derived facts about an inode that the tar header does not carry.
pub mod inode_flags {
    /// The blob placed this file's contents at a 4 KiB boundary congruent
    /// with the addresses a mapping would use. Serves the zero-copy aliasing
    /// optimisation, which is designed, flagged and off — v0 copies.
    pub const MMAP_ALIGNED: u32 = 1 << 0;
    /// The file's contents are produced when it is read rather than stored.
    /// A synthetic mount's window onto kernel state — `/proc/self/maps` is
    /// a rendering of the VMA tree, and a snapshot taken at boot would
    /// describe an address space that no longer exists. The inode's
    /// `payload` says which view it is.
    pub const GENERATED: u32 = 1 << 2;
    /// The file's bytes are stored as a zstd frame, behind a four-byte
    /// length, and are decompressed the first time they are asked for.
    /// `size` is still the file's own length, which is what `stat`
    /// answers and what the decompressed bytes are checked against.
    ///
    /// The module used to carry the rootfs uncompressed — 124 MB of the
    /// Django demo's 170 — and it compresses 3.4×. Files below
    /// [`COMPRESS_FLOOR`] and files that do not compress are stored raw,
    /// so a small file costs nothing to open and the bake never makes a
    /// file bigger.
    pub const COMPRESSED: u32 = 1 << 3;
}

/// Files shorter than this are stored raw: the frame's own overhead and a
/// decoder's set-up are not worth it, and the small files are the ones a
/// boot opens by the hundred.
pub const COMPRESS_FLOOR: usize = 4096;

/// The bytes before a compressed file's frame: its compressed length.
pub const COMPRESSED_PREFIX: usize = 4;

thread_local! {
    /// Decompressed files, by blob and offset. See [`Image::decompressed`].
    static DECOMPRESSED: core::cell::RefCell<
        std::collections::HashMap<(usize, u64, u64), &'static [u8]>,
    > = core::cell::RefCell::new(std::collections::HashMap::new());
}

/// Decodes one zstd frame that must come to exactly `expected` bytes.
///
/// The length is the check, and it is a loud one: a frame that decodes
/// short or long is corruption in the bake or the blob, and a guest that
/// read fewer bytes than the file has would fail somewhere far from here.
fn decompress(frame: &[u8], expected: u64) -> Result<Vec<u8>, ImageError> {
    use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};
    let mut source: &[u8] = frame;
    let mut decoder = FrameDecoder::new();
    decoder
        .reset(&mut source)
        .map_err(|_| ImageError::Malformed("a compressed file's frame header"))?;
    decoder
        .decode_blocks(&mut source, BlockDecodingStrategy::All)
        .map_err(|_| ImageError::Malformed("a compressed file's frame"))?;
    let bytes = decoder
        .collect()
        .ok_or(ImageError::Malformed("a compressed file decoded to nothing"))?;
    if bytes.len() as u64 != expected {
        return Err(ImageError::Malformed(
            "a compressed file decoded to a different length than the index says",
        ));
    }
    Ok(bytes)
}

/// One inode, as the index stores it.
///
/// The packager preserves metadata completely — ownership, the setuid, setgid
/// and sticky bits, timestamps, symbolic owner names, xattrs — and whether
/// the kernel ever *honours* any given field is a separate and deliberately
/// undecided question. Preservation has to be total precisely because it is
/// undecided: nothing can later enforce what the bake threw away.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Inode {
    /// `st_mode` verbatim: type and permission bits, setuid included.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// Computed by the packager after whiteout processing; tar hardlink entries
    /// resolve to one shared record.
    pub nlink: u32,
    pub size: u64,
    pub mtime_sec: i64,
    /// Tar gives seconds; PAX can give nanoseconds. Zero where the archive
    /// said nothing, and the format does not pretend `ctime`/`atime` exist.
    pub mtime_nsec: u32,
    /// Offset into the xattr region, or zero for none.
    pub xattr_ref: u32,
    /// A union by file type: blob offset for a regular file, string
    /// reference for a symlink target, dirent-region offset for a directory,
    /// packed `rdev` for a device node.
    pub payload: u64,
    /// Tar's *symbolic* owner names, which numeric ids do not capture. the kernel
    /// ignores them; faithful re-export and tooling want them.
    pub uname_ref: u32,
    pub gname_ref: u32,
    pub flags: u32,
    /// For a directory, the inode of the directory containing it; the root's
    /// is itself. Zero and meaningless for anything else.
    ///
    /// Stored rather than tracked during a walk because a walk does not
    /// always start at the root: `openat` begins at a descriptor and
    /// `getcwd` begins nowhere at all. POSIX forbids hard links to
    /// directories, so a directory has exactly one parent and this is
    /// well-defined — which is the same reason real kernels can get away
    /// with `..` being a real entry.
    pub parent: u32,
}

impl Inode {
    pub fn file_type(&self) -> u32 {
        self.mode & file_type::MASK
    }

    pub fn is_directory(&self) -> bool {
        self.file_type() == file_type::DIRECTORY
    }

    pub fn is_regular(&self) -> bool {
        self.file_type() == file_type::REGULAR
    }

    pub fn is_symlink(&self) -> bool {
        self.file_type() == file_type::SYMLINK
    }

    /// Serializes into the index's packed form. Written here rather than in
    /// the packager so that the reader and the writer cannot disagree about the
    /// layout — the same mistake, made once, is caught by any round trip.
    pub fn write(&self, into: &mut [u8; INODE_SIZE]) {
        into[0..4].copy_from_slice(&self.mode.to_le_bytes());
        into[4..8].copy_from_slice(&self.uid.to_le_bytes());
        into[8..12].copy_from_slice(&self.gid.to_le_bytes());
        into[12..16].copy_from_slice(&self.nlink.to_le_bytes());
        into[16..24].copy_from_slice(&self.size.to_le_bytes());
        into[24..32].copy_from_slice(&self.mtime_sec.to_le_bytes());
        into[32..36].copy_from_slice(&self.mtime_nsec.to_le_bytes());
        into[36..40].copy_from_slice(&self.xattr_ref.to_le_bytes());
        into[40..48].copy_from_slice(&self.payload.to_le_bytes());
        into[48..52].copy_from_slice(&self.uname_ref.to_le_bytes());
        into[52..56].copy_from_slice(&self.gname_ref.to_le_bytes());
        into[56..60].copy_from_slice(&self.flags.to_le_bytes());
        into[60..64].copy_from_slice(&self.parent.to_le_bytes());
    }

    fn read(bytes: &[u8]) -> Self {
        Self {
            mode: word(bytes, 0),
            uid: word(bytes, 4),
            gid: word(bytes, 8),
            nlink: word(bytes, 12),
            size: long(bytes, 16),
            mtime_sec: long(bytes, 24) as i64,
            mtime_nsec: word(bytes, 32),
            xattr_ref: word(bytes, 36),
            payload: long(bytes, 40),
            uname_ref: word(bytes, 48),
            gname_ref: word(bytes, 52),
            flags: word(bytes, 56),
            parent: word(bytes, 60),
        }
    }
}

/// One directory entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DirectoryEntry {
    pub name_ref: u32,
    pub inode: u32,
    /// Precomputed from the target's mode, so `getdents64` never has to
    /// resolve an inode to answer.
    pub entry_type: u8,
}

/// The index's region table.
///
/// Public, and written through by name rather than by position. The writer
/// used to take fourteen bare `u32`s in a row and the reader unpacked them
/// by offset, which is a shape where adding a region means adding a
/// fifteenth argument to a call whose arguments are indistinguishable from
/// one another — and where transposing two of them produces an index that
/// parses and describes a different filesystem. There is no way to write
/// that mistake now.
#[derive(Clone, Copy, Debug, Default)]
pub struct Header {
    pub inode_count: u32,
    pub inode_offset: u32,
    pub dirent_offset: u32,
    pub dirent_size: u32,
    pub string_offset: u32,
    pub string_size: u32,
    pub xattr_offset: u32,
    pub xattr_size: u32,
    pub root_inode: u32,
    /// How many bytes of blob the index describes. The index's own length is
    /// derivable from the regions; the blob's is not, and a module that
    /// carries both as bare symbols has no other way to learn it.
    pub blob_size: u32,
    pub command_offset: u32,
    pub command_size: u32,
    pub environment_offset: u32,
    pub environment_size: u32,
    /// The directory the container starts in.
    ///
    /// An OCI image says this and it is not decoration: `CMD ["python",
    /// "app.py"]` beside `WORKDIR /app` names a file that only exists
    /// relative to somewhere. Empty means the image did not say, and the
    /// container starts at the root.
    pub working_directory_offset: u32,
    pub working_directory_size: u32,
}

impl Header {
    /// Reads a header without validating it against an index.
    ///
    /// Shared by [`Image::parse`], which then validates, and by
    /// [`index_length`], which has only the header to work from.
    fn read(header: &[u8]) -> Self {
        Self {
            inode_count: word(header, 8),
            inode_offset: word(header, 12),
            dirent_offset: word(header, 16),
            dirent_size: word(header, 20),
            string_offset: word(header, 24),
            string_size: word(header, 28),
            xattr_offset: word(header, 32),
            xattr_size: word(header, 36),
            root_inode: word(header, 40),
            blob_size: word(header, 44),
            command_offset: word(header, 48),
            command_size: word(header, 52),
            environment_offset: word(header, 56),
            environment_size: word(header, 60),
            working_directory_offset: word(header, 64),
            working_directory_size: word(header, 68),
        }
    }

    /// Every region the index contains: its name, where it starts, and how
    /// long it is.
    ///
    /// **One list, and both consumers read it.** Validation needs it to
    /// refuse an index that does not contain what its header claims, and
    /// [`index_length`] needs it to say how many bytes an index is. Those
    /// used to be two lists, and a region added to one and not the other is
    /// an image that parses at the bake and is refused inside the kernel's
    /// own construction, where there is no kernel yet to report it. That
    /// happened twice while the two lists existed, which is twice more than
    /// a comment saying "remember to update both" was worth.
    ///
    /// Sizes are `u64` and *not* narrowed on the way: an inode count of 2^26
    /// makes the byte count 2^32, which as a `u32` is zero — a region of
    /// nothing, which validates against any index at all and then traps the
    /// first time an inode is read.
    fn regions(&self) -> [(&'static str, u32, u64); 7] {
        [
            (
                "inodes",
                self.inode_offset,
                (self.inode_count as u64) * (INODE_SIZE as u64),
            ),
            ("dirents", self.dirent_offset, self.dirent_size as u64),
            ("strings", self.string_offset, self.string_size as u64),
            ("xattrs", self.xattr_offset, self.xattr_size as u64),
            ("command", self.command_offset, self.command_size as u64),
            (
                "environment",
                self.environment_offset,
                self.environment_size as u64,
            ),
            (
                "working directory",
                self.working_directory_offset,
                self.working_directory_size as u64,
            ),
        ]
    }
}

/// A baked image: the index and the blob its regular files live in.
///
/// Borrowed rather than owned, and every accessor hands back borrowed bytes.
/// A `read(2)` of an image file is a copy straight out of the blob, which is
/// the whole reason the filesystem torrent never reaches the host.
///
/// `Copy`, because it is a view: two slices and a parsed header. Holding one
/// by value is how a syscall row keeps reading an image while the rest of the
/// kernel is borrowed mutably.
#[derive(Clone, Copy)]
pub struct Image<'a> {
    index: &'a [u8],
    blob: &'a [u8],
    header: Header,
}

impl<'a> Image<'a> {
    pub fn parse(index: &'a [u8], blob: &'a [u8]) -> Result<Self, ImageError> {
        if index.len() < HEADER_SIZE {
            return Err(ImageError::TruncatedRegion("header"));
        }
        if word(index, 0) != MAGIC {
            return Err(ImageError::BadMagic);
        }
        let version = word(index, 4);
        if version != VERSION {
            return Err(ImageError::UnsupportedVersion(version));
        }
        let header = Header::read(index);

        // Validate the regions once, here, so that everything after is an
        // offset within a region already known to exist.
        for (name, offset, size) in header.regions() {
            let end = (offset as u64)
                .checked_add(size)
                .ok_or(ImageError::TruncatedRegion(name))?;
            if end > index.len() as u64 {
                return Err(ImageError::TruncatedRegion(name));
            }
        }
        if header.root_inode >= header.inode_count {
            return Err(ImageError::OutOfRange("root inode"));
        }
        if (header.blob_size as usize) > blob.len() {
            return Err(ImageError::TruncatedRegion("blob"));
        }

        Ok(Self {
            index,
            blob,
            header,
        })
    }

    pub fn root(&self) -> u32 {
        self.header.root_inode
    }

    pub fn inode_count(&self) -> u32 {
        self.header.inode_count
    }

    /// `stat`, as far as the index is concerned: an index multiply and some
    /// field copies.
    pub fn inode(&self, number: u32) -> Result<Inode, ImageError> {
        if number >= self.header.inode_count {
            return Err(ImageError::OutOfRange("inode"));
        }
        let start = (self.header.inode_offset as u64) + (number as u64) * (INODE_SIZE as u64);
        let start = u32::try_from(start).map_err(|_| ImageError::OutOfRange("inode"))?;
        Ok(Inode::read(span(
            self.index,
            start,
            INODE_SIZE as u64,
            "inode",
        )?))
    }

    /// The command line the bake recorded: the arguments, in order.
    ///
    /// Empty when the bake said nothing, which is what leaves the boot path
    /// at its default. Strings are NUL-separated and the region's own length
    /// is the end, so an argument containing no NUL is the only thing that
    /// can be represented — which is exactly what an argument is.
    pub fn command_line(&self) -> impl Iterator<Item = &'a [u8]> {
        let region = self
            .index
            .get(
                self.header.command_offset as usize
                    ..self.header.command_offset as usize + self.header.command_size as usize,
            )
            .unwrap_or(&[]);
        region
            .split(|byte| *byte == 0)
            .filter(|argument| !argument.is_empty())
    }

    /// The environment the bake recorded: `NAME=value`, in order.
    ///
    /// Empty when the bake said nothing. See the module header for why a
    /// container without one is not merely a container with less: it takes
    /// different paths through its own libraries.
    pub fn environment(&self) -> impl Iterator<Item = &'a [u8]> {
        let region = self
            .index
            .get(
                self.header.environment_offset as usize
                    ..self.header.environment_offset as usize
                        + self.header.environment_size as usize,
            )
            .unwrap_or(&[]);
        region
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
    }

    /// The directory the container starts in, or empty when the image did
    /// not say.
    ///
    /// A fact about the container rather than a file in it, which is why it
    /// is a region and not a path somebody has to know to read — the same
    /// reasoning as the command line and the environment.
    pub fn working_directory(&self) -> &'a [u8] {
        self.index
            .get(
                self.header.working_directory_offset as usize
                    ..self.header.working_directory_offset as usize
                        + self.header.working_directory_size as usize,
            )
            .unwrap_or(&[])
    }

    /// A length-prefixed byte string from the strings region.
    pub fn string(&self, reference: u32) -> Result<&'a [u8], ImageError> {
        let region = self.region(self.header.string_offset, self.header.string_size);
        let header = span(region, reference, 2, "string")?;
        let length = u16::from_le_bytes([header[0], header[1]]) as u64;
        let body = reference
            .checked_add(2)
            .ok_or(ImageError::OutOfRange("string"))?;
        span(region, body, length, "string runs past its region")
    }

    /// How many entries a directory has.
    pub fn entry_count(&self, directory: &Inode) -> Result<u32, ImageError> {
        if !directory.is_directory() {
            return Err(ImageError::WrongKind);
        }
        let region = self.region(self.header.dirent_offset, self.header.dirent_size);
        let payload =
            u32::try_from(directory.payload).map_err(|_| ImageError::OutOfRange("directory"))?;
        let count = word(span(region, payload, 4, "directory")?, 0);
        // The whole array has to be present before any entry is read, so that
        // `entry` needs no check of its own beyond its position.
        let entries = (count as u64) * (DIRENT_SIZE as u64);
        let after_count = payload
            .checked_add(4)
            .ok_or(ImageError::Malformed("directory entry array overruns"))?;
        span(
            region,
            after_count,
            entries,
            "directory entry array overruns",
        )?;
        Ok(count)
    }

    /// One entry of a directory, by position. Entries are sorted by name, so
    /// position is also the order `getdents64` reports them in.
    pub fn entry(&self, directory: &Inode, position: u32) -> Result<DirectoryEntry, ImageError> {
        let count = self.entry_count(directory)?;
        if position >= count {
            return Err(ImageError::OutOfRange("directory entry"));
        }
        let region = self.region(self.header.dirent_offset, self.header.dirent_size);
        // `entry_count` has already established that the whole array fits, so
        // this offset is inside it — but it is computed at full width anyway,
        // because a cast that is only safe because of something a few lines
        // up is a cast that stops being safe when those lines move.
        let start = (directory.payload)
            .checked_add(4)
            .and_then(|after| after.checked_add((position as u64) * (DIRENT_SIZE as u64)))
            .and_then(|start| u32::try_from(start).ok())
            .ok_or(ImageError::OutOfRange("directory entry"))?;
        let record = span(region, start, DIRENT_SIZE as u64, "directory entry")?;
        Ok(DirectoryEntry {
            name_ref: word(record, 0),
            inode: word(record, 4),
            entry_type: record[8],
        })
    }

    /// The one question the resolution loop asks: does this directory have an
    /// entry with this name?
    ///
    /// A binary search over the sorted array, so a miss costs the same as a
    /// hit — which is the case that dominates, since most of the `stat`
    /// torrent is an interpreter probing paths that do not exist.
    pub fn lookup(
        &self,
        directory: &Inode,
        name: &[u8],
    ) -> Result<Option<DirectoryEntry>, ImageError> {
        let count = self.entry_count(directory)?;
        let (mut low, mut high) = (0u32, count);
        while low < high {
            let middle = low + (high - low) / 2;
            let entry = self.entry(directory, middle)?;
            match self.string(entry.name_ref)?.cmp(name) {
                core::cmp::Ordering::Less => low = middle + 1,
                core::cmp::Ordering::Greater => high = middle,
                core::cmp::Ordering::Equal => return Ok(Some(entry)),
            }
        }
        Ok(None)
    }

    /// A regular file's bytes: borrowed straight from the blob, or — for a
    /// file the bake compressed — decompressed the first time and kept.
    pub fn contents(&self, inode: &Inode) -> Result<&'a [u8], ImageError> {
        if !inode.is_regular() {
            return Err(ImageError::WrongKind);
        }
        if inode.flags & inode_flags::COMPRESSED != 0 {
            return self.decompressed(inode);
        }
        let end = inode
            .payload
            .checked_add(inode.size)
            .ok_or(ImageError::Malformed("file contents overrun the blob"))?;
        if end > self.blob.len() as u64 {
            return Err(ImageError::Malformed("file contents overrun the blob"));
        }
        Ok(&self.blob[inode.payload as usize..end as usize])
    }

    /// `length` bytes of the blob at `at`, checked at full width.
    fn blob_span(&self, at: u64, length: u64) -> Result<&'a [u8], ImageError> {
        let end = at
            .checked_add(length)
            .ok_or(ImageError::Malformed("file contents overrun the blob"))?;
        if end > self.blob.len() as u64 {
            return Err(ImageError::Malformed("file contents overrun the blob"));
        }
        Ok(&self.blob[at as usize..end as usize])
    }

    /// A compressed file's bytes.
    ///
    /// Decompressed once and remembered for the life of the thread, keyed
    /// by the blob and the file's place in it. The image is a `Copy` view
    /// held by value in syscall rows, so the memory cannot live in it; and
    /// the bytes are leaked rather than reference-counted because every
    /// caller wants a `&'a [u8]` with the image's own lifetime, which is
    /// the container's. What this costs is that a file once opened stays
    /// decompressed until the container ends — bounded by the rootfs, and
    /// in practice by what a boot touches, which is a fraction of what the
    /// blob used to hold uncompressed all along.
    fn decompressed(&self, inode: &Inode) -> Result<&'a [u8], ImageError> {
        // The claimed length is part of the key, so that an index that
        // lies about a file's size is caught at every open and not just
        // the first.
        let key = (self.blob.as_ptr() as usize, inode.payload, inode.size);
        if let Some(held) = DECOMPRESSED.with_borrow(|held| held.get(&key).copied()) {
            return Ok(held);
        }
        let header = self.blob_span(inode.payload, COMPRESSED_PREFIX as u64)?;
        let length = u64::from(word(header, 0));
        let frame = self.blob_span(inode.payload + COMPRESSED_PREFIX as u64, length)?;
        let bytes = decompress(frame, inode.size)?;
        let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        DECOMPRESSED.with_borrow_mut(|held| {
            held.insert(key, leaked);
        });
        Ok(leaked)
    }

    pub fn symlink_target(&self, inode: &Inode) -> Result<&'a [u8], ImageError> {
        if !inode.is_symlink() {
            return Err(ImageError::WrongKind);
        }
        let reference =
            u32::try_from(inode.payload).map_err(|_| ImageError::OutOfRange("symlink target"))?;
        self.string(reference)
    }

    /// How many extended attributes an inode carries.
    pub fn xattr_count(&self, inode: &Inode) -> Result<u32, ImageError> {
        if inode.xattr_ref == 0 {
            return Ok(0);
        }
        let region = self.region(self.header.xattr_offset, self.header.xattr_size);
        Ok(word(span(region, inode.xattr_ref, 4, "xattr block")?, 0))
    }

    /// One extended attribute, as the name and value byte strings the guest
    /// would see. Stored and served verbatim: `security.capability` is a
    /// packed binary struct, and interpreting it at bake time would destroy
    /// the option of honouring it later.
    pub fn xattr(&self, inode: &Inode, position: u32) -> Result<(&'a [u8], &'a [u8]), ImageError> {
        let count = self.xattr_count(inode)?;
        if position >= count {
            return Err(ImageError::OutOfRange("xattr"));
        }
        let region = self.region(self.header.xattr_offset, self.header.xattr_size);
        let start = (inode.xattr_ref as u64)
            .checked_add(4)
            .and_then(|after| after.checked_add((position as u64) * 8))
            .and_then(|start| u32::try_from(start).ok())
            .ok_or(ImageError::Malformed("xattr block overruns"))?;
        let record = span(region, start, 8, "xattr block overruns")?;
        Ok((self.string(word(record, 0))?, self.string(word(record, 4))?))
    }

    fn region(&self, offset: u32, size: u32) -> &'a [u8] {
        // Validated in `parse`, so this cannot be out of range.
        &self.index[offset as usize..offset as usize + size as usize]
    }
}

/// The bytes at `offset..offset + length` of `region`, or a named error.
///
/// The arithmetic is `u64` because the inputs are `u32` values read out of
/// the index and `usize` is 32 bits on the target: adding two of them in
/// `usize` can wrap past the check that is supposed to catch them.
fn span<'a>(
    region: &'a [u8],
    offset: u32,
    length: u64,
    what: &'static str,
) -> Result<&'a [u8], ImageError> {
    let start = offset as u64;
    let end = start
        .checked_add(length)
        .ok_or(ImageError::OutOfRange(what))?;
    if end > region.len() as u64 {
        return Err(ImageError::OutOfRange(what));
    }
    Ok(&region[start as usize..end as usize])
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn long(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

/// Writes an index header. Here rather than in the packager for the same reason
/// [`Inode::write`] is: one definition, so a round trip is a real check.
pub fn write_header(into: &mut [u8; HEADER_SIZE], header: &Header) {
    into[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    into[4..8].copy_from_slice(&VERSION.to_le_bytes());
    into[8..12].copy_from_slice(&header.inode_count.to_le_bytes());
    into[12..16].copy_from_slice(&header.inode_offset.to_le_bytes());
    into[16..20].copy_from_slice(&header.dirent_offset.to_le_bytes());
    into[20..24].copy_from_slice(&header.dirent_size.to_le_bytes());
    into[24..28].copy_from_slice(&header.string_offset.to_le_bytes());
    into[28..32].copy_from_slice(&header.string_size.to_le_bytes());
    into[32..36].copy_from_slice(&header.xattr_offset.to_le_bytes());
    into[36..40].copy_from_slice(&header.xattr_size.to_le_bytes());
    into[40..44].copy_from_slice(&header.root_inode.to_le_bytes());
    into[44..48].copy_from_slice(&header.blob_size.to_le_bytes());
    into[48..52].copy_from_slice(&header.command_offset.to_le_bytes());
    into[52..56].copy_from_slice(&header.command_size.to_le_bytes());
    into[56..60].copy_from_slice(&header.environment_offset.to_le_bytes());
    into[60..64].copy_from_slice(&header.environment_size.to_le_bytes());
    into[64..68].copy_from_slice(&header.working_directory_offset.to_le_bytes());
    into[68..72].copy_from_slice(&header.working_directory_size.to_le_bytes());
}

/// The total length of an index whose header is `header`.
///
/// Derivable rather than stored: the xattr region is last, so its end is the
/// end of the index. A module carrying the index as a bare symbol needs this
/// to know how many bytes it may look at.
pub fn index_length(header: &[u8]) -> Result<usize, ImageError> {
    if header.len() < HEADER_SIZE {
        return Err(ImageError::TruncatedRegion("header"));
    }
    if word(header, 0) != MAGIC {
        return Err(ImageError::BadMagic);
    }
    // The end of the region that ends last, rather than of whichever region
    // the writer happens to put last — and taken from `Header::regions`, so
    // that a region added to the format is accounted for here without
    // anybody having to remember to come back.
    let end = Header::read(header)
        .regions()
        .into_iter()
        .map(|(_, offset, size)| offset as u64 + size)
        .max()
        .expect("the array is not empty")
        .max(HEADER_SIZE as u64);
    usize::try_from(end).map_err(|_| ImageError::TruncatedRegion("index"))
}

/// The blob length an index header declares.
pub fn blob_length(header: &[u8]) -> Result<usize, ImageError> {
    if header.len() < HEADER_SIZE {
        return Err(ImageError::TruncatedRegion("header"));
    }
    if word(header, 0) != MAGIC {
        return Err(ImageError::BadMagic);
    }
    Ok(word(header, 44) as usize)
}

// The image the module carries, found through link-time symbols.
//
// The packager emits these as two data segments and `wasm-ld` places them;
// nothing here knows or needs an address. Every container links an image,
// even an empty one, so a missing image is an undefined symbol at link time
// rather than a module that silently has no files.
#[cfg(target_arch = "wasm32")]
unsafe extern "C" {
    #[link_name = "__image_index"]
    static IMAGE_INDEX: u8;
    #[link_name = "__image_blob"]
    static IMAGE_BLOB: u8;
}

/// Parses the linked image. The header describes both regions' extents, so
/// two bare symbols are enough — no third symbol carrying a length, and
/// nothing to keep in step.
#[cfg(target_arch = "wasm32")]
pub fn baked() -> Result<Image<'static>, ImageError> {
    // SAFETY: the symbols name data segments the linker placed, and the
    // lengths come from the header those bytes begin with — validated by
    // `index_length` and `blob_length` before either slice is formed.
    unsafe {
        let index_start = &raw const IMAGE_INDEX;
        let header = core::slice::from_raw_parts(index_start, HEADER_SIZE);
        let index = core::slice::from_raw_parts(index_start, index_length(header)?);
        let blob = core::slice::from_raw_parts(&raw const IMAGE_BLOB, blob_length(header)?);
        Image::parse(index, blob)
    }
}

/// Off wasm there is no linked image; native tests bake their own.
#[cfg(not(target_arch = "wasm32"))]
pub fn baked() -> Result<Image<'static>, ImageError> {
    unreachable!("the linked image exists only inside the wasm module")
}
