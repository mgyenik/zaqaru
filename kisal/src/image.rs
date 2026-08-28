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
//! malformed index is a bug in the baker and a bug that reads out of bounds
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
//! ```

/// `KISI` — kisal image.
pub const MAGIC: u32 = u32::from_le_bytes(*b"KISI");
pub const VERSION: u32 = 1;

pub const HEADER_SIZE: usize = 48;
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

    /// The `d_type` a mode implies. The one mapping, used by the baker when
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

/// Baker-derived facts about an inode that the tar header does not carry.
pub mod inode_flags {
    /// The blob placed this file's contents at a 4 KiB boundary congruent
    /// with the addresses a mapping would use. Serves the zero-copy aliasing
    /// optimisation, which is designed, flagged and off — v0 copies.
    pub const MMAP_ALIGNED: u32 = 1 << 0;
    /// An ELF the bake transpiled, with an entry in the static exec map.
    pub const EXEC_TRANSPILED: u32 = 1 << 1;
    /// The file's contents are produced when it is read rather than stored.
    /// A synthetic mount's window onto kernel state — `/proc/self/maps` is
    /// a rendering of the VMA tree, and a snapshot taken at boot would
    /// describe an address space that no longer exists. The inode's
    /// `payload` says which view it is.
    pub const GENERATED: u32 = 1 << 2;
}

/// One inode, as the index stores it.
///
/// The baker preserves metadata completely — ownership, the setuid, setgid
/// and sticky bits, timestamps, symbolic owner names, xattrs — and whether
/// kisal ever *honours* any given field is a separate and deliberately
/// undecided question. Preservation has to be total precisely because it is
/// undecided: nothing can later enforce what the bake threw away.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Inode {
    /// `st_mode` verbatim: type and permission bits, setuid included.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// Computed by the baker after whiteout processing; tar hardlink entries
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
    /// Tar's *symbolic* owner names, which numeric ids do not capture. kisal
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
    /// the baker so that the reader and the writer cannot disagree about the
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
#[derive(Clone, Copy, Debug)]
struct Header {
    inode_count: u32,
    inode_offset: u32,
    dirent_offset: u32,
    dirent_size: u32,
    string_offset: u32,
    string_size: u32,
    xattr_offset: u32,
    xattr_size: u32,
    root_inode: u32,
    /// How many bytes of blob the index describes. The index's own length is
    /// derivable from the regions; the blob's is not, and a module that
    /// carries both as bare symbols has no other way to learn it.
    blob_size: u32,
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
        let header = Header {
            inode_count: word(index, 8),
            inode_offset: word(index, 12),
            dirent_offset: word(index, 16),
            dirent_size: word(index, 20),
            string_offset: word(index, 24),
            string_size: word(index, 28),
            xattr_offset: word(index, 32),
            xattr_size: word(index, 36),
            root_inode: word(index, 40),
            blob_size: word(index, 44),
        };

        // Validate the regions once, here, so that everything after is an
        // offset within a region already known to exist.
        // At full width, and *not* narrowed to `u32` on the way: an inode
        // count of 2^26 makes the byte count 2^32, which as a `u32` is zero —
        // a region of nothing, which validates against any index at all and
        // then traps the first time an inode is read.
        let inode_bytes = (header.inode_count as u64) * (INODE_SIZE as u64);
        for (name, offset, size) in [
            ("inodes", header.inode_offset, inode_bytes),
            ("dirents", header.dirent_offset, header.dirent_size as u64),
            ("strings", header.string_offset, header.string_size as u64),
            ("xattrs", header.xattr_offset, header.xattr_size as u64),
        ] {
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
        let payload = u32::try_from(directory.payload)
            .map_err(|_| ImageError::OutOfRange("directory"))?;
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

    /// A regular file's bytes, borrowed straight from the blob.
    pub fn contents(&self, inode: &Inode) -> Result<&'a [u8], ImageError> {
        if !inode.is_regular() {
            return Err(ImageError::WrongKind);
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

    pub fn symlink_target(&self, inode: &Inode) -> Result<&'a [u8], ImageError> {
        if !inode.is_symlink() {
            return Err(ImageError::WrongKind);
        }
        let reference = u32::try_from(inode.payload)
            .map_err(|_| ImageError::OutOfRange("symlink target"))?;
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
        Ok((
            self.string(word(record, 0))?,
            self.string(word(record, 4))?,
        ))
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
    let end = start.checked_add(length).ok_or(ImageError::OutOfRange(what))?;
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

/// Writes an index header. Here rather than in the baker for the same reason
/// [`Inode::write`] is: one definition, so a round trip is a real check.
pub fn write_header(
    into: &mut [u8; HEADER_SIZE],
    inode_count: u32,
    inode_offset: u32,
    dirent_offset: u32,
    dirent_size: u32,
    string_offset: u32,
    string_size: u32,
    xattr_offset: u32,
    xattr_size: u32,
    root_inode: u32,
    blob_size: u32,
) {
    into[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    into[4..8].copy_from_slice(&VERSION.to_le_bytes());
    into[8..12].copy_from_slice(&inode_count.to_le_bytes());
    into[12..16].copy_from_slice(&inode_offset.to_le_bytes());
    into[16..20].copy_from_slice(&dirent_offset.to_le_bytes());
    into[20..24].copy_from_slice(&dirent_size.to_le_bytes());
    into[24..28].copy_from_slice(&string_offset.to_le_bytes());
    into[28..32].copy_from_slice(&string_size.to_le_bytes());
    into[32..36].copy_from_slice(&xattr_offset.to_le_bytes());
    into[36..40].copy_from_slice(&xattr_size.to_le_bytes());
    into[40..44].copy_from_slice(&root_inode.to_le_bytes());
    into[44..48].copy_from_slice(&blob_size.to_le_bytes());
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
    Ok(word(header, 32) as usize + word(header, 36) as usize)
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

/// The image the module carries, found through link-time symbols.
///
/// The baker emits these as two data segments and `wasm-ld` places them;
/// nothing here knows or needs an address. Every container links an image,
/// even an empty one, so a missing image is an undefined symbol at link time
/// rather than a module that silently has no files.
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
