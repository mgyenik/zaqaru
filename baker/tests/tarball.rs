//! The tar reader, against GNU tar's own output.
//!
//! The oracle is `tar` itself: a tree is built on disk, archived, and read
//! back — so what is compared is not this reader against my idea of the
//! format but against what the tool every image is built with actually
//! writes. Three formats, because they disagree about exactly the fields an
//! image cares about: `ustar` splits long paths and truncates timestamps,
//! `gnu` uses long-name records and base-256 numbers, and `pax` uses extended
//! headers for all of it.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use baker::tar::{Entry, Kind};

struct Workspace {
    root: PathBuf,
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn workspace(label: &str) -> Workspace {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("baker-tar-{label}-{unique}"));
    std::fs::create_dir_all(&root).expect("mkdir");
    Workspace { root }
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write");
}

/// A tree with one of everything the reader has to carry.
fn build_tree(root: &Path) {
    std::fs::create_dir_all(root.join("etc")).expect("mkdir");
    std::fs::create_dir_all(root.join("usr/bin")).expect("mkdir");
    write(&root.join("etc/hosts"), b"127.0.0.1 localhost\n");
    // Explicitly, rather than whatever the umask gives: the mode is what is
    // being compared.
    std::fs::set_permissions(
        root.join("etc/hosts"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("chmod");
    write(&root.join("usr/bin/tool"), b"#!/bin/sh\nexit 0\n");
    std::fs::set_permissions(
        root.join("usr/bin/tool"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
    std::os::unix::fs::symlink("usr/bin", root.join("bin")).expect("symlink");
    // A hardlink, which tar records as a `1` naming the earlier entry.
    std::fs::hard_link(root.join("usr/bin/tool"), root.join("usr/bin/tool2")).expect("link");
    let fifo = std::ffi::CString::new(root.join("etc/pipe").as_os_str().as_bytes()).expect("path");
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
}

fn archive(workspace: &Workspace, arguments: &[&str]) -> Vec<u8> {
    let output = workspace.root.join("archive.tar");
    let status = std::process::Command::new("tar")
        .arg("-cf")
        .arg(&output)
        .arg("-C")
        .arg(workspace.root.join("tree"))
        .args(arguments)
        .arg(".")
        .status()
        .expect("run tar");
    assert!(status.success(), "tar failed");
    std::fs::read(&output).expect("read the archive")
}

fn find<'a>(entries: &'a [Entry], path: &str) -> &'a Entry {
    entries
        .iter()
        .find(|entry| entry.path == path.as_bytes())
        .unwrap_or_else(|| {
            let names: Vec<String> = entries
                .iter()
                .map(|entry| String::from_utf8_lossy(&entry.path).into_owned())
                .collect();
            panic!("no entry `{path}` in {names:?}")
        })
}

#[test]
fn every_format_reads_back_as_the_tree_it_was() {
    for format in ["ustar", "gnu", "pax"] {
        let workspace = workspace(&format!("shape-{format}"));
        build_tree(&workspace.root.join("tree"));
        let bytes = archive(&workspace, &[&format!("--format={format}")]);
        let entries = baker::tar::read(&bytes)
            .unwrap_or_else(|error| panic!("[{format}] reading the archive: {error}"));

        let hosts = find(&entries, "./etc/hosts");
        assert_eq!(hosts.kind, Kind::Regular, "[{format}]");
        assert_eq!(hosts.contents, b"127.0.0.1 localhost\n", "[{format}]");
        assert_eq!(hosts.mode & 0o777, 0o644, "[{format}]");
        assert_eq!(hosts.uid, unsafe { libc::getuid() }, "[{format}]");

        let tool = find(&entries, "./usr/bin/tool");
        assert_eq!(tool.mode & 0o777, 0o755, "[{format}]");

        // The second name for one file is a hardlink record naming the
        // first, not a second copy of the bytes.
        let twin = find(&entries, "./usr/bin/tool2");
        assert_eq!(twin.kind, Kind::HardLink, "[{format}]");
        assert_eq!(twin.link, b"./usr/bin/tool", "[{format}]");
        assert!(twin.contents.is_empty(), "[{format}]");

        let link = find(&entries, "./bin");
        assert_eq!(link.kind, Kind::Symlink, "[{format}]");
        assert_eq!(link.link, b"usr/bin", "[{format}]");

        assert_eq!(find(&entries, "./etc/pipe").kind, Kind::Fifo, "[{format}]");
        // Directories carry tar's trailing slash, kept as written: the
        // typeflag says what it is, and normalising a path is the
        // consumer's job rather than the reader's.
        assert_eq!(find(&entries, "./etc/").kind, Kind::Directory, "[{format}]");

        // Timestamps survive, which is what `.pyc` staleness turns on.
        let on_disk = std::fs::metadata(workspace.root.join("tree/etc/hosts")).expect("stat");
        use std::os::unix::fs::MetadataExt;
        assert_eq!(hosts.mtime_sec, on_disk.mtime(), "[{format}]");
    }
}

/// A path longer than the 100 bytes a tar header holds. `ustar` splits it
/// across `prefix` and `name`, `gnu` writes an `L` record, and `pax` writes
/// a `path` record — three different mechanisms for one fact.
#[test]
fn a_long_path_survives_in_every_format() {
    for format in ["ustar", "gnu", "pax"] {
        let workspace = workspace(&format!("long-{format}"));
        let tree = workspace.root.join("tree");
        // Deep enough that `ustar` must split it, and one component long
        // enough that the split cannot fall just anywhere.
        let mut deep = tree.join("very-long-directory-name-for-the-prefix-field");
        for index in 0..4 {
            deep = deep.join(format!("component-number-{index}-with-padding"));
        }
        std::fs::create_dir_all(&deep).expect("mkdir");
        write(&deep.join("file-with-a-long-name-as-well"), b"deep\n");

        let bytes = archive(&workspace, &[&format!("--format={format}")]);
        let entries = baker::tar::read(&bytes).expect("read");
        let relative = deep
            .strip_prefix(&tree)
            .expect("under the tree")
            .join("file-with-a-long-name-as-well");
        let expected = format!("./{}", relative.display());
        assert!(expected.len() > 100, "the path is not long enough to test");
        let entry = find(&entries, &expected);
        assert_eq!(entry.contents, b"deep\n", "[{format}]");
    }
}

/// Extended attributes ride in PAX `SCHILY.xattr.*` records, which is how
/// `security.capability` reaches an image.
#[test]
fn extended_attributes_ride_in_pax_records() {
    let workspace = workspace("xattr");
    let tree = workspace.root.join("tree");
    std::fs::create_dir_all(&tree).expect("mkdir");
    write(&tree.join("tool"), b"binary\n");
    set_xattr(&tree.join("tool"), b"user.origin", b"the archive");
    set_xattr(&tree.join("tool"), b"user.aardvark", &[0x00, 0xff, 0x10]);

    let bytes = archive(&workspace, &["--format=pax", "--xattrs"]);
    let entries = baker::tar::read(&bytes).expect("read");
    let tool = find(&entries, "./tool");
    assert_eq!(
        tool.xattrs,
        vec![
            (b"user.aardvark".to_vec(), vec![0x00, 0xff, 0x10]),
            (b"user.origin".to_vec(), b"the archive".to_vec()),
        ],
        "sorted, and byte-faithful including a value that is not text"
    );
}

/// A sub-second timestamp, which only PAX can carry — and which is exactly
/// what CPython compares to decide a `.pyc` is stale.
#[test]
fn a_fractional_timestamp_survives_pax() {
    let workspace = workspace("mtime");
    let tree = workspace.root.join("tree");
    std::fs::create_dir_all(&tree).expect("mkdir");
    let file = tree.join("stamped");
    write(&file, b"x");
    let times = [
        libc::timespec {
            tv_sec: 1_700_000_000,
            tv_nsec: 123_456_789,
        },
        libc::timespec {
            tv_sec: 1_700_000_000,
            tv_nsec: 123_456_789,
        },
    ];
    let path = std::ffi::CString::new(file.as_os_str().as_bytes()).expect("path");
    assert_eq!(
        unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) },
        0,
        "{}",
        std::io::Error::last_os_error()
    );

    let bytes = archive(&workspace, &["--format=pax"]);
    let entries = baker::tar::read(&bytes).expect("read");
    let entry = find(&entries, "./stamped");
    assert_eq!(entry.mtime_sec, 1_700_000_000);
    assert_eq!(entry.mtime_nsec, 123_456_789);
}

/// A damaged header is refused by name rather than read as whatever the
/// bytes happen to say. A tar of tars has no other framing: a reader that
/// lost its alignment would produce plausible nonsense.
#[test]
fn a_damaged_header_is_refused() {
    let workspace = workspace("damaged");
    build_tree(&workspace.root.join("tree"));
    let bytes = archive(&workspace, &["--format=gnu"]);

    let mut flipped = bytes.clone();
    flipped[10] ^= 0xff;
    let refusal = baker::tar::read(&flipped).expect_err("a damaged header must be refused");
    assert!(
        format!("{refusal}").contains("checksum"),
        "the refusal does not say what failed: {refusal}"
    );

    // And a record type the reader does not implement is refused rather
    // than skipped: a skipped record is a file missing from the image.
    let mut unknown = bytes.clone();
    unknown[156] = b'Z';
    fix_checksum(&mut unknown, 0);
    let refusal = baker::tar::read(&unknown).expect_err("an unknown record must be refused");
    assert!(format!("{refusal}").contains('Z'), "{refusal}");

    // A truncated archive is refused rather than read as the entries that
    // happened to fit. Cut in the middle of the records, not merely past the
    // end-of-archive marker.
    let whole = baker::tar::read(&bytes).expect("read");
    assert!(whole.len() > 4, "the fixture is smaller than it looks");
    // Cut inside a record: the record says how many bytes follow it and they
    // are not there.
    let refusal = baker::tar::read(&bytes[..bytes.len() / 2])
        .expect_err("a truncated archive must be refused");
    assert!(
        format!("{refusal}").contains("past the end"),
        "the refusal does not say what failed: {refusal}"
    );
    // Cut at the end-of-archive marker itself: every record is whole, so
    // nothing local is wrong — and without the marker there is no way to
    // tell this from a complete archive, which is the whole reason the
    // marker is required. (`tar` pads to a 10 KiB record, so this is found
    // rather than counted back from the end.)
    let marker = bytes
        .chunks_exact(512)
        .position(|block| block.iter().all(|byte| *byte == 0))
        .expect("the archive has an end-of-archive marker")
        * 512;
    let refusal =
        baker::tar::read(&bytes[..marker]).expect_err("an unmarked archive must be refused");
    assert!(
        format!("{refusal}").contains("cut short"),
        "the refusal does not say what failed: {refusal}"
    );
}

fn fix_checksum(archive: &mut [u8], at: usize) {
    archive[at + 148..at + 156].copy_from_slice(b"        ");
    let sum: u32 = archive[at..at + 512].iter().map(|byte| *byte as u32).sum();
    let text = format!("{sum:06o}\0 ");
    archive[at + 148..at + 156].copy_from_slice(text.as_bytes());
}

fn set_xattr(path: &Path, name: &[u8], value: &[u8]) {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path");
    let name = std::ffi::CString::new(name).expect("name");
    let result = unsafe {
        libc::lsetxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
}
