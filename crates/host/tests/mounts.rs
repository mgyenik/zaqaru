//! The mount table: the one place that decides whether an operation touches
//! the host, and what it touches when it does.
//!
//! Tested on its own because the capability model *is* this table. "No
//! `/iso/net` mount means no network" is a claim about longest-prefix
//! resolution and about what happens when nothing matches, and both are
//! cheaper to falsify here than through an emulated syscall.

use host::store::{MountTable, Sink, Store};

fn path(segments: &[&[u8]]) -> Vec<Vec<u8>> {
    segments.iter().map(|segment| segment.to_vec()).collect()
}

#[test]
fn a_mount_serves_its_whole_subtree() {
    let mut mounts = MountTable::new();
    mounts.mount(&[b"iso", b"console"], Box::new(Sink::new()));

    mounts
        .write(&path(&[b"iso", b"console", b"stdout"]), b"out")
        .expect("write");
    mounts
        .write(&path(&[b"iso", b"console", b"stderr"]), b"err")
        .expect("write");

    assert_eq!(
        mounts
            .read(&path(&[b"iso", b"console", b"stdout"]))
            .expect("read"),
        Some(b"out".to_vec())
    );
    assert_eq!(
        mounts
            .read(&path(&[b"iso", b"console", b"stderr"]))
            .expect("read"),
        Some(b"err".to_vec())
    );
}

/// The more specific mount wins, whatever order the mounts were declared in.
/// Declaration order deciding this would make a configuration file's meaning
/// depend on how it was typed.
#[test]
fn the_longest_prefix_wins_regardless_of_order() {
    for reversed in [false, true] {
        let mut mounts = MountTable::new();
        let general: (&[&[u8]], Box<dyn Store>) = (&[b"iso"], Box::new(Sink::new()));
        let specific: (&[&[u8]], Box<dyn Store>) = (&[b"iso", b"console"], Box::new(Sink::new()));
        let (first, second) = if reversed {
            (specific, general)
        } else {
            (general, specific)
        };
        mounts.mount(first.0, first.1);
        mounts.mount(second.0, second.1);

        mounts
            .write(&path(&[b"iso", b"console", b"stdout"]), b"console")
            .expect("write");
        mounts
            .write(&path(&[b"iso", b"time", b"now"]), b"general")
            .expect("write");

        // Each landed in its own store, so neither can see the other's bytes.
        assert_eq!(
            mounts
                .read(&path(&[b"iso", b"console", b"stdout"]))
                .expect("read"),
            Some(b"console".to_vec())
        );
        assert_eq!(
            mounts
                .read(&path(&[b"iso", b"time", b"now"]))
                .expect("read"),
            Some(b"general".to_vec())
        );
    }
}

/// The capability refusal: an unmounted subtree is an error naming the path,
/// not an empty answer that a caller could mistake for "nothing there".
#[test]
fn an_unmounted_path_is_refused_by_name() {
    let mut mounts = MountTable::new();
    mounts.mount(&[b"iso", b"console"], Box::new(Sink::new()));

    let refusal = mounts
        .read(&path(&[b"iso", b"net", b"connect"]))
        .expect_err("an unmounted path must be refused");
    assert!(
        refusal.contains("/iso/net/connect"),
        "the refusal does not name the path: {refusal}"
    );
    let refusal = mounts
        .write(&path(&[b"iso", b"net", b"connect"]), b"{}")
        .expect_err("an unmounted path must be refused");
    assert!(refusal.contains("/iso/net/connect"), "{refusal}");
}

/// A path is bytes, so a diagnostic must survive a segment that is not text
/// rather than losing it.
#[test]
fn a_path_renders_non_text_segments_escaped() {
    assert_eq!(host::store::render(&path(&[b"iso", b"log"])), "/iso/log");
    assert_eq!(
        host::store::render(&[b"iso".to_vec(), vec![0x00, 0xff]]),
        "/iso/\\x00\\xff"
    );
}

/// A read of a path nothing has written is `ok(none)` — present as an
/// address, empty as a value. The distinction is the store's whole error
/// vocabulary, and the kernel maps it to different errnos.
#[test]
fn an_unwritten_path_under_a_mount_is_absent_not_an_error() {
    let mut mounts = MountTable::new();
    mounts.mount(&[b"iso"], Box::new(Sink::new()));
    assert_eq!(
        mounts.read(&path(&[b"iso", b"nothing"])).expect("read"),
        None
    );
}

/// Every segment of a path is part of the key, and the key is the whole path
/// rather than the part past the mount point.
///
/// The earlier version of this test wrote one path and asserted that a
/// shorter one read back `None`. That assertion could not fail: nothing had
/// ever written the shorter path, so `None` was the answer under the bug and
/// under the fix alike. Two paths that a truncation would collide are what
/// actually distinguishes them — so both are written, with different bytes,
/// and each has to read back its own.
#[test]
fn a_mount_lookup_uses_every_segment_it_was_given() {
    let mut mounts = MountTable::new();
    mounts.mount(&[b"iso", b"console"], Box::new(Sink::new()));
    mounts.mount(&[b"iso"], Box::new(Sink::new()));

    // `/iso/console` is a path in the *general* mount — the longest prefix
    // that matches it is `/iso`, because a mount's own path is not inside
    // it. `/iso/console/stdout` is a path in the console mount. A lookup
    // that dropped the last segment would read one for the other.
    mounts
        .write(&path(&[b"iso", b"console", b"stdout"]), b"console")
        .expect("write");
    mounts
        .write(&path(&[b"iso", b"console"]), b"general")
        .expect("write");

    assert_eq!(
        mounts
            .read(&path(&[b"iso", b"console", b"stdout"]))
            .expect("read"),
        Some(b"console".to_vec()),
        "the deeper path reads its own bytes, not the prefix's"
    );
    assert_eq!(
        mounts.read(&path(&[b"iso", b"console"])).expect("read"),
        Some(b"general".to_vec()),
        "and the prefix reads its own, not the deeper path's"
    );

    // A path under no mount at all is still refused, so the two answers
    // above are not just "everything lands somewhere".
    assert!(mounts.read(&path(&[b"other", b"thing"])).is_err());
}

/// The boot-time capability question, which decides whether a kernel can
/// report a fault at all.
#[test]
fn a_mount_table_knows_what_it_covers() {
    let mut mounts = MountTable::new();
    mounts.mount(&[b"iso", b"log"], Box::new(Sink::new()));
    assert!(mounts.resolves(&path(&[b"iso", b"log", b"error"])));
    assert!(!mounts.resolves(&path(&[b"iso", b"net", b"connect"])));
}
