//! The mounts an image does not carry: `/dev` and `/proc`.
//!
//! A base image ships `/dev/null` as a device node with no driver behind it,
//! and `/proc` as an empty directory — because on a real system both are
//! *mounts*, filled in by the kernel at boot. So they are filled in here, the
//! same way: a small filesystem built at boot and attached over the
//! directory the image provides.
//!
//! Built as ordinary images, in the same packed format the baker writes, so
//! resolution, `stat`, `getdents64` and the rest work on them without
//! knowing they are synthetic. Only *reads* differ, and they differ where a
//! real kernel differs too: an inode whose type is a character device sends
//! its reads to a driver instead of to a blob. That dispatch is on the
//! inode's own type and its device number, which is exactly what a real VFS
//! does, rather than on a path that something could have moved.
//!
//! The format is written with `kisal::image`'s own writers — the same
//! `Inode::write` and `write_header` the baker calls — so there is no second
//! definition of the layout to drift from the first.

use std::vec;
use std::vec::Vec;

use crate::image::{
    HEADER_SIZE, INODE_SIZE, Image, ImageError, Inode, directory_entry_type, file_type, inode_flags,
};

/// The device numbers Linux gives these, which is what a program that looks
/// at `st_rdev` expects to see. `/dev/null` is (1, 3) everywhere.
pub const NULL_DEVICE: (u32, u32) = (1, 3);
pub const ZERO_DEVICE: (u32, u32) = (1, 5);
pub const FULL_DEVICE: (u32, u32) = (1, 7);
pub const RANDOM_DEVICE: (u32, u32) = (1, 8);
pub const URANDOM_DEVICE: (u32, u32) = (1, 9);

/// Which generated view a `GENERATED` inode is. Small and closed: a view
/// nothing renders would be a file that reads as empty, which is a plausible
/// wrong answer.
pub mod view {
    /// `/proc/self/maps`, rendered from the VMA tree.
    pub const MAPS: u64 = 1;
}

/// Linux's packed `dev_t`: minor's low eight bits, major's twelve above
/// them, and minor's remaining twelve above that. Not a plain shift, and the
/// difference is invisible until a minor exceeds 255.
pub const fn make_device(major: u32, minor: u32) -> u64 {
    ((major as u64 & 0xfff) << 8) | (minor as u64 & 0xff) | ((minor as u64 & 0xfff00) << 12)
}

/// One device node to build.
struct Entry {
    name: &'static [u8],
    mode: u32,
    /// Linux's packed `rdev`, which is what the read dispatch keys on.
    rdev: u64,
}

/// `/dev`, with the character devices a libc actually opens.
///
/// Not the whole of a real `/dev`: no `tty`, because v0 decides stdio is not
/// a terminal and a `/dev/tty` that answered would contradict that; no
/// `stdin`/`stdout`/`stderr` symlinks, because they point into `/proc/self/fd`
/// which does not exist until M6 has one to point at. What is here is what a
/// process opens before it does anything else.
pub fn dev() -> Vec<u8> {
    build(&[
        Entry {
            name: b"null",
            mode: file_type::CHARACTER | 0o666,
            rdev: make_device(NULL_DEVICE.0, NULL_DEVICE.1),
        },
        Entry {
            name: b"zero",
            mode: file_type::CHARACTER | 0o666,
            rdev: make_device(ZERO_DEVICE.0, ZERO_DEVICE.1),
        },
        Entry {
            name: b"full",
            mode: file_type::CHARACTER | 0o666,
            rdev: make_device(FULL_DEVICE.0, FULL_DEVICE.1),
        },
        // Both, and both are the same stream. Linux stopped distinguishing
        // them in 5.6 — `/dev/random` no longer blocks once the pool is
        // initialised — and a container that made `/dev/random` block would
        // hang programs that still prefer it out of habit.
        Entry {
            name: b"random",
            mode: file_type::CHARACTER | 0o666,
            rdev: make_device(RANDOM_DEVICE.0, RANDOM_DEVICE.1),
        },
        Entry {
            name: b"urandom",
            mode: file_type::CHARACTER | 0o666,
            rdev: make_device(URANDOM_DEVICE.0, URANDOM_DEVICE.1),
        },
    ])
}

/// `/proc`, which at this milestone is `self/exe` and nothing else.
///
/// `self/exe` and `self/maps`, and nothing else yet. What is absent is
/// absent: `stat("/proc/self/status")` is `ENOENT`, which is what a
/// filesystem says about a name it does not have.
///
/// That is worth being uncomfortable about rather than papering over. A real
/// procfs has no such gaps, so a program that probes for one of these and
/// gets `ENOENT` may conclude there is no procfs at all and take a branch
/// nobody tested. The alternative — a loud fault on every absent name —
/// would kill a container for probing an optional file, which programs do
/// constantly. The files get added as something is found to need them; this
/// comment is here so that the next person to hit it knows the gap is known
/// rather than overlooked.
pub fn proc(executable: &[u8]) -> Vec<u8> {
    // `self` is a directory holding `exe`. Built by hand rather than by the
    // generic builder because it is two levels deep.
    let mut index = Vec::new();
    let mut inodes: Vec<Inode> = Vec::new();
    let mut strings = Strings::default();

    // 0: the root, 1: `self`, 2: `exe`, 3: `maps`.
    inodes.push(directory(0));
    inodes.push(directory(0));
    inodes.push(Inode {
        mode: file_type::SYMLINK | 0o777,
        size: executable.len() as u64,
        payload: strings.intern(executable) as u64,
        nlink: 1,
        ..blank()
    });
    // Size zero, as every procfs file reports: its length is not known
    // until it is read, and a program that trusted a size here would
    // allocate the wrong buffer.
    inodes.push(Inode {
        mode: file_type::REGULAR | 0o444,
        size: 0,
        payload: view::MAPS,
        flags: inode_flags::GENERATED,
        nlink: 1,
        ..blank()
    });

    let mut dirents = Vec::new();
    // The root's array, then `self`'s.
    let root_payload = dirents.len() as u64;
    write_entries(
        &mut dirents,
        &mut strings,
        &[(b"self", 1, directory_entry_type::DIRECTORY)],
    );
    let self_payload = dirents.len() as u64;
    write_entries(
        &mut dirents,
        &mut strings,
        &[
            (b"exe", 2, directory_entry_type::SYMLINK),
            (b"maps", 3, directory_entry_type::REGULAR),
        ],
    );
    inodes[0].payload = root_payload;
    inodes[0].size = (4 + 12) as u64;
    inodes[0].nlink = 3; // `.`, `..`, and `self`
    inodes[1].payload = self_payload;
    inodes[1].size = (4 + 12 * 2) as u64;
    inodes[1].parent = 0;

    assemble(&mut index, &inodes, &dirents, &strings.bytes);
    index
}

/// Builds a one-level directory of entries.
fn build(entries: &[Entry]) -> Vec<u8> {
    let mut index = Vec::new();
    let mut inodes: Vec<Inode> = vec![directory(0)];
    let mut strings = Strings::default();
    let mut listing = Vec::new();

    for entry in entries {
        let number = inodes.len() as u32;
        inodes.push(Inode {
            mode: entry.mode,
            payload: entry.rdev,
            nlink: 1,
            ..blank()
        });
        listing.push((
            entry.name,
            number,
            directory_entry_type::of_mode(entry.mode),
        ));
    }

    let mut dirents = Vec::new();
    write_entries(&mut dirents, &mut strings, &listing);
    inodes[0].payload = 0;
    inodes[0].size = (4 + 12 * entries.len()) as u64;
    inodes[0].nlink = 2;

    assemble(&mut index, &inodes, &dirents, &strings.bytes);
    index
}

fn write_entries(dirents: &mut Vec<u8>, strings: &mut Strings, entries: &[(&[u8], u32, u8)]) {
    dirents.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    // Interned first, so the references are known before any is written and
    // sorted, because lookup is a binary search.
    let mut sorted: Vec<(u32, u32, u8)> = entries
        .iter()
        .map(|(name, inode, kind)| (strings.intern(name), *inode, *kind))
        .collect();
    let order: Vec<usize> = {
        let mut indices: Vec<usize> = (0..entries.len()).collect();
        indices.sort_by_key(|index| entries[*index].0);
        indices
    };
    sorted = order.iter().map(|index| sorted[*index]).collect();
    for (name, inode, kind) in sorted {
        dirents.extend_from_slice(&name.to_le_bytes());
        dirents.extend_from_slice(&inode.to_le_bytes());
        dirents.push(kind);
        dirents.extend_from_slice(&[0; 3]);
    }
}

fn assemble(index: &mut Vec<u8>, inodes: &[Inode], dirents: &[u8], strings: &[u8]) {
    let inode_offset = HEADER_SIZE;
    let dirent_offset = inode_offset + inodes.len() * INODE_SIZE;
    let string_offset = dirent_offset + dirents.len();
    let xattr_offset = string_offset + strings.len();

    index.resize(HEADER_SIZE, 0);
    let header: &mut [u8; HEADER_SIZE] =
        (&mut index[..HEADER_SIZE]).try_into().expect("the header");
    crate::image::write_header(
        header,
        inodes.len() as u32,
        inode_offset as u32,
        dirent_offset as u32,
        dirents.len() as u32,
        string_offset as u32,
        strings.len() as u32,
        xattr_offset as u32,
        0,
        0,
        0,
        // A synthetic mount holds no ELF, so there is nothing to prelink.
        0,
        0,
    );
    for inode in inodes {
        let mut record = [0u8; INODE_SIZE];
        inode.write(&mut record);
        index.extend_from_slice(&record);
    }
    index.extend_from_slice(dirents);
    index.extend_from_slice(strings);
}

fn directory(parent: u32) -> Inode {
    Inode {
        mode: file_type::DIRECTORY | 0o755,
        nlink: 2,
        parent,
        ..blank()
    }
}

fn blank() -> Inode {
    Inode {
        mode: 0,
        uid: 0,
        gid: 0,
        nlink: 0,
        size: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        xattr_ref: 0,
        payload: 0,
        uname_ref: 0,
        gname_ref: 0,
        flags: 0,
        parent: 0,
    }
}

#[derive(Default)]
struct Strings {
    bytes: Vec<u8>,
}

impl Strings {
    /// The same rule the baker's interning follows: the empty string sits at
    /// zero, so a zero reference reads as empty rather than as whatever
    /// happened to be written first.
    fn intern(&mut self, bytes: &[u8]) -> u32 {
        if self.bytes.is_empty() {
            self.bytes.extend_from_slice(&0u16.to_le_bytes());
            if bytes.is_empty() {
                return 0;
            }
        }
        let reference = self.bytes.len() as u32;
        self.bytes
            .extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        self.bytes.extend_from_slice(bytes);
        reference
    }
}

/// Parses a built index, for a caller that wants the image back.
pub fn parse(index: &[u8]) -> Result<Image<'_>, ImageError> {
    Image::parse(index, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic filesystem is an ordinary image, so everything that reads
    /// an image reads it — which is the point of building it this way rather
    /// than special-casing a path.
    #[test]
    fn dev_reads_back_as_an_image() {
        let index = dev();
        let image = parse(&index).expect("the built index parses");
        let root = image.inode(image.root()).expect("root");
        assert!(root.is_directory());
        assert_eq!(image.entry_count(&root).expect("count"), 5);

        for (name, (major, minor)) in [
            (&b"null"[..], NULL_DEVICE),
            (b"zero", ZERO_DEVICE),
            (b"full", FULL_DEVICE),
            (b"random", RANDOM_DEVICE),
            (b"urandom", URANDOM_DEVICE),
        ] {
            let entry = image
                .lookup(&root, name)
                .expect("lookup")
                .unwrap_or_else(|| panic!("/dev/{} is missing", String::from_utf8_lossy(name)));
            assert_eq!(entry.entry_type, directory_entry_type::CHARACTER);
            let inode = image.inode(entry.inode).expect("inode");
            assert_eq!(inode.file_type(), file_type::CHARACTER);
            assert_eq!(inode.mode & 0o777, 0o666);
            assert_eq!(inode.payload, make_device(major, minor));
            assert_eq!(inode.nlink, 1);
        }

        // Sorted, because lookup is a binary search: an unsorted array finds
        // some names and not others, which is the worst possible failure.
        let names: Vec<Vec<u8>> = (0..image.entry_count(&root).expect("count"))
            .map(|position| {
                let entry = image.entry(&root, position).expect("entry");
                image.string(entry.name_ref).expect("name").to_vec()
            })
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        assert!(image.lookup(&root, b"nothing").expect("lookup").is_none());
    }

    #[test]
    fn proc_carries_the_executable_path() {
        let index = proc(b"/usr/bin/python3");
        let image = parse(&index).expect("parse");
        let root = image.inode(image.root()).expect("root");
        let entry = image
            .lookup(&root, b"self")
            .expect("lookup")
            .expect("/proc/self");
        assert_eq!(entry.entry_type, directory_entry_type::DIRECTORY);
        let self_dir = image.inode(entry.inode).expect("inode");
        assert!(self_dir.is_directory());
        assert_eq!(self_dir.parent, image.root(), "`..` goes back to /proc");

        let exe = image
            .lookup(&self_dir, b"exe")
            .expect("lookup")
            .expect("/proc/self/exe");
        let exe = image.inode(exe.inode).expect("inode");
        assert!(exe.is_symlink());
        assert_eq!(
            image.symlink_target(&exe).expect("target"),
            b"/usr/bin/python3"
        );
        assert_eq!(exe.size, 16, "a symlink's size is its target's length");
    }

    /// Linux's packed device number, which is not a shift — and a minor
    /// above 255 is where the two stop agreeing.
    #[test]
    fn a_device_number_is_packed_the_way_linux_packs_it() {
        assert_eq!(make_device(1, 3), 0x103);
        assert_eq!(make_device(1, 9), 0x109);
        // `makedev(8, 300)` is 0x10082c, not 0x82c.
        assert_eq!(make_device(8, 300), 0x0010_082c);
        assert_eq!(make_device(0, 0), 0);
    }
}
