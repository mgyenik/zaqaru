//! The memory rows, against a real buffer.
//!
//! The address space is linear memory, so a native test can be the real
//! thing: the fixture owns a buffer, tells the kernel that the guest's
//! memory is that buffer, and every address the kernel hands out is one the
//! test can read and write. What is being checked is not arithmetic about
//! addresses but what the guest *sees* at them.

use std::path::PathBuf;

use kisal::abi::{Store, StoreOutcome};
use kisal::errno::Errno;
use kisal::machine::Registers;
use kisal::space::{Space, advice, map, prot};
use kisal::syscall::{Arguments, Kernel, Outcome, number};

// ---- the fixture -----------------------------------------------------------

/// A store with nothing in it. These tests are about the address space, and
/// a container with no mounts is the simplest thing that can hold one.
struct Silent;

impl Store for Silent {
    fn read(&mut self, _path: &[&[u8]], _into: &mut Vec<u8>) -> StoreOutcome {
        StoreOutcome::Absent
    }
    fn write(&mut self, _path: &[&[u8]], _data: &[u8]) -> StoreOutcome {
        StoreOutcome::Present
    }
}

struct Tree {
    root: PathBuf,
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The guest's whole address space, mapped at guest addresses.
///
/// It used to be a `Box<[u8]>` whose host address the kernel was handed as a
/// guest address. The page table forbids that — a host heap pointer is far
/// outside the four gigabytes the machine has — so the region is mapped
/// low, where a guest's memory really is.
struct Space_ {
    region: targum::arena::Arena,
}

impl Space_ {
    fn base(&self) -> u64 {
        self.region.base()
    }

    fn end(&self) -> u64 {
        self.region.limit()
    }

    fn read(&self, address: u64, length: usize) -> &[u8] {
        assert!(
            address >= self.base() && address + length as u64 <= self.end(),
            "{address:#x} is outside the guest's memory"
        );
        // SAFETY: inside the committed region, checked above.
        unsafe { core::slice::from_raw_parts(address as usize as *const u8, length) }
    }

    fn write(&mut self, address: u64, bytes: &[u8]) {
        assert!(
            address >= self.base() && address + bytes.len() as u64 <= self.end(),
            "{address:#x} is outside the guest's memory"
        );
        // SAFETY: as above.
        unsafe {
            (address as usize as *mut u8).copy_from(bytes.as_ptr(), bytes.len());
        }
    }
}

struct Fixture {
    kernel: Kernel<'static, Silent, Registers>,
    memory: Space_,
    /// Where the kernel's arenas start, and where the scratch area the test
    /// uses for its own arguments ends.
    scratch: u64,
    used: u64,
    _tree: Tree,
}

/// Four megabytes of address space and a small `brk` arena. Small on
/// purpose: the ceiling is what makes `brk` fail over to `mmap`, and a test
/// that wants to watch that should not have to allocate the production
/// arena to see it.
const ARENA: usize = 4 * 1024 * 1024;
const BRK_ARENA: u64 = 64 * 1024;
/// The first stretch of the buffer belongs to the test, for paths and
/// buffers it passes to syscalls. The kernel's arenas start above it.
const SCRATCH: u64 = 64 * 1024;
/// The patterned fixture file's length: four pages and a bit, so a mapping
/// can cover whole pages of it and still run off the end.
const PATTERNED: usize = 4 * 4096 + 1000;

fn fixture(label: &str) -> Fixture {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("kisal-mem-{label}-{unique}"));
    std::fs::create_dir_all(root.join("etc")).expect("mkdir");
    // The mount points the kernel fills in at boot.
    std::fs::create_dir_all(root.join("dev")).expect("mkdir");
    std::fs::create_dir_all(root.join("proc")).expect("mkdir");
    std::fs::write(root.join("etc/hosts"), b"127.0.0.1 localhost\n").expect("write");
    // A file long enough to span pages, with a pattern that says which byte
    // of it any given byte is.
    let patterned: Vec<u8> = (0..PATTERNED as u32)
        .map(|index| (index % 251) as u8)
        .collect();
    std::fs::write(root.join("etc/patterned"), &patterned).expect("write");
    let tree = Tree { root };

    let baked: &'static baker::Image =
        Box::leak(Box::new(baker::bake_directory(&tree.root).expect("bake")));
    let image = kisal::image::Image::parse(&baked.index, &baked.blob).expect("parse");

    let memory = Space_ {
        region: targum::arena::Arena::new(ARENA as u64),
    };
    let scratch = memory.base();
    let arena_start = scratch + SCRATCH;
    let mut kernel = Kernel::new(
        Silent,
        Registers {
            // The guest can reach the scratch area at first; the arenas
            // above it become reachable as the kernel grows into them.
            memory_limit: arena_start,
            ceiling: memory.end(),
            ..Default::default()
        },
        image,
    );
    kernel.space = Space::with_brk_ceiling(arena_start, BRK_ARENA);
    Fixture {
        kernel,
        memory,
        scratch,
        used: 0,
        _tree: tree,
    }
}

impl Fixture {
    fn call(&mut self, number: i64, arguments: [i64; 6]) -> i64 {
        match self.kernel.dispatch(number, Arguments::new(arguments)) {
            Outcome::Done(value) => value,
            other => panic!("syscall {number} produced {other:?} instead of a result"),
        }
    }

    /// Puts bytes in the test's own scratch area and returns the address.
    fn place(&mut self, bytes: &[u8]) -> i64 {
        let at = self.scratch + self.used.next_multiple_of(8);
        assert!(
            at + bytes.len() as u64 <= self.scratch + SCRATCH,
            "scratch exhausted"
        );
        self.memory.write(at, bytes);
        self.used = at - self.scratch + bytes.len() as u64;
        at as i64
    }

    fn path(&mut self, path: &str) -> i64 {
        let mut bytes = path.as_bytes().to_vec();
        bytes.push(0);
        self.place(&bytes)
    }

    fn open(&mut self, path: &str) -> i64 {
        let address = self.path(path);
        let fd = self.call(number::OPEN, [address, 0, 0, 0, 0, 0]);
        assert!(fd >= 0, "opening {path} failed with {fd}");
        fd
    }

    fn map(&mut self, hint: i64, length: i64, prot: i32, flags: i32) -> i64 {
        self.call(
            number::MMAP,
            [hint, length, prot as i64, flags as i64, -1, 0],
        )
    }

    fn anonymous(&mut self, length: i64) -> u64 {
        let address = self.map(
            0,
            length,
            prot::READ | prot::WRITE,
            map::PRIVATE | map::ANONYMOUS,
        );
        assert!(address > 0, "mmap failed with {address}");
        address as u64
    }

    fn bytes(&self, address: u64, length: usize) -> &[u8] {
        self.memory.read(address, length)
    }

    fn dirty(&mut self, address: u64, length: usize, byte: u8) {
        self.memory.write(address, &vec![byte; length]);
    }

    /// What `/proc/self/maps` would say right now, without going through
    /// the file rows.
    fn maps(&mut self) -> String {
        let fd = self.open("/proc/self/maps");
        let buffer = self.place(&[0u8; 8192]);
        let read = self.call(number::READ, [fd, buffer, 8192, 0, 0, 0]);
        assert!(read >= 0, "reading /proc/self/maps failed with {read}");
        let text = String::from_utf8(self.bytes(buffer as u64, read as usize).to_vec())
            .expect("the file is text");
        self.call(number::CLOSE, [fd, 0, 0, 0, 0, 0]);
        text
    }
}

const PAGE: i64 = 4096;

// ---- anonymous memory ------------------------------------------------------

/// The contract every allocator is built on: fresh anonymous memory reads as
/// zeros. There are no faults here, so nothing can be zeroed lazily — it is
/// zeroed when it is handed out or it is never zeroed at all.
#[test]
fn anonymous_memory_reads_as_zeros_and_keeps_what_is_written() {
    let mut fixture = fixture("anonymous");
    let address = fixture.anonymous(3 * PAGE);
    assert_eq!(address % PAGE as u64, 0, "a mapping is page-aligned");
    assert_eq!(
        fixture.bytes(address, 3 * PAGE as usize),
        vec![0u8; 3 * PAGE as usize]
    );

    fixture.dirty(address, 3 * PAGE as usize, 0xa5);
    assert_eq!(fixture.bytes(address + 8, 4), &[0xa5; 4]);

    // A second mapping is somewhere else, and is its own zeros.
    let second = fixture.anonymous(PAGE);
    assert!(
        second >= address + 3 * PAGE as u64 || second + PAGE as u64 <= address,
        "the two mappings overlap: {address:#x} and {second:#x}"
    );
    assert_eq!(
        fixture.bytes(second, PAGE as usize),
        vec![0u8; PAGE as usize]
    );
}

/// A length that is not a whole number of pages is rounded up, so the tail
/// of the last page belongs to the mapping too — which is what makes
/// `mmap(1)` give a program a page rather than a byte.
#[test]
fn a_length_rounds_up_to_a_whole_page() {
    let mut fixture = fixture("rounding");
    let address = fixture.anonymous(1);
    fixture.dirty(address, PAGE as usize, 0x11);
    // The next mapping starts past the whole page, not one byte past the
    // request.
    let next = fixture.anonymous(1);
    assert!(next >= address + PAGE as u64 || next + PAGE as u64 <= address);
    assert_eq!(fixture.bytes(next, 16), &[0u8; 16], "and it is zeros");
}

#[test]
fn mmap_refuses_what_it_cannot_mean() {
    let mut fixture = fixture("mmap-edges");
    // Zero length.
    assert_eq!(
        fixture.map(0, 0, prot::READ, map::PRIVATE | map::ANONYMOUS),
        Errno::Invalid.as_result()
    );
    // A protection bit `mmap` does not know is *ignored*, not refused —
    // measured against this machine's kernel, which accepts even
    // `0x80000000` here and answers `EINVAL` to the same bit in
    // `mprotect`. The asymmetry is real and the rows differ accordingly.
    let ignored = fixture.map(0, PAGE, 0x40, map::PRIVATE | map::ANONYMOUS);
    assert!(
        ignored > 0,
        "mmap refused an unknown protection bit: {ignored}"
    );
    assert_eq!(
        fixture
            .kernel
            .space
            .find(ignored as u64)
            .expect("mapped")
            .prot,
        prot::NONE,
        "and nothing it did not understand was recorded"
    );
    // Neither shared nor private.
    assert_eq!(
        fixture.map(0, PAGE, prot::READ, map::ANONYMOUS),
        Errno::Invalid.as_result()
    );
    // Both `MAP_FIXED` and `MAP_FIXED_NOREPLACE`, which contradict.
    assert_eq!(
        fixture.map(
            0x100000,
            PAGE,
            prot::READ,
            map::PRIVATE | map::ANONYMOUS | map::FIXED | map::FIXED_NOREPLACE
        ),
        Errno::Invalid.as_result()
    );
    // A fixed address that is not page-aligned.
    assert_eq!(
        fixture.map(
            0x100001,
            PAGE,
            prot::READ,
            map::PRIVATE | map::ANONYMOUS | map::FIXED
        ),
        Errno::Invalid.as_result()
    );
    // A file offset that is not page-aligned.
    let fd = fixture.open("/etc/hosts");
    assert_eq!(
        fixture.call(
            number::MMAP,
            [0, PAGE, prot::READ as i64, map::PRIVATE as i64, fd, 1]
        ),
        Errno::Invalid.as_result()
    );
}

// ---- the address space's shape --------------------------------------------

/// `MAP_FIXED` replaces whatever it lands on, and `MAP_FIXED_NOREPLACE`
/// refuses to. The difference is the whole reason a caller can ask for an
/// address safely.
#[test]
fn a_fixed_mapping_replaces_and_no_replace_refuses() {
    let mut fixture = fixture("fixed");
    let base = fixture.anonymous(4 * PAGE);
    fixture.dirty(base, 4 * PAGE as usize, 0xcc);

    // Over the middle two pages.
    let middle = base as i64 + PAGE;
    let replaced = fixture.map(
        middle,
        2 * PAGE,
        prot::READ | prot::WRITE,
        map::PRIVATE | map::ANONYMOUS | map::FIXED,
    );
    assert_eq!(replaced, middle, "MAP_FIXED lands where it was told");
    // Fresh anonymous memory, so the replaced pages read as zeros again…
    assert_eq!(
        fixture.bytes(middle as u64, 2 * PAGE as usize),
        vec![0u8; 2 * PAGE as usize]
    );
    // …and the pages on either side were not touched.
    assert_eq!(fixture.bytes(base, 16), &[0xcc; 16]);
    assert_eq!(fixture.bytes(base + 3 * PAGE as u64, 16), &[0xcc; 16]);

    // The tree now has three mappings where there was one.
    let covering: Vec<_> = fixture
        .kernel
        .space
        .vmas()
        .iter()
        .filter(|vma| vma.start >= base && vma.start < base + 4 * PAGE as u64)
        .map(|vma| (vma.start - base, vma.length))
        .collect();
    assert_eq!(
        covering,
        vec![
            (0, PAGE as u64),
            (PAGE as u64, 2 * PAGE as u64),
            (3 * PAGE as u64, PAGE as u64)
        ],
        "the replaced range split the mapping it landed in"
    );

    // `MAP_FIXED_NOREPLACE` over anything mapped is `EEXIST`, and changes
    // nothing.
    assert_eq!(
        fixture.map(
            middle,
            PAGE,
            prot::READ,
            map::PRIVATE | map::ANONYMOUS | map::FIXED_NOREPLACE
        ),
        Errno::Exists.as_result()
    );
    // Over a range that is free, it succeeds and lands there.
    let free = base + 8 * PAGE as u64;
    assert_eq!(
        fixture.map(
            free as i64,
            PAGE,
            prot::READ,
            map::PRIVATE | map::ANONYMOUS | map::FIXED_NOREPLACE
        ),
        free as i64
    );
}

/// A partial `munmap` leaves the parts it did not cover, which is the case a
/// naive implementation drops on the floor.
#[test]
fn a_partial_unmap_splits_the_mapping() {
    let mut fixture = fixture("partial-unmap");
    let base = fixture.anonymous(5 * PAGE);
    fixture.dirty(base, 5 * PAGE as usize, 0x77);

    // Punch the middle page out.
    assert_eq!(
        fixture.call(number::MUNMAP, [base as i64 + 2 * PAGE, PAGE, 0, 0, 0, 0]),
        0
    );
    let pieces: Vec<_> = fixture
        .kernel
        .space
        .vmas()
        .iter()
        .filter(|vma| vma.start >= base && vma.start < base + 5 * PAGE as u64)
        .map(|vma| (vma.start - base, vma.length))
        .collect();
    assert_eq!(
        pieces,
        vec![(0, 2 * PAGE as u64), (3 * PAGE as u64, 2 * PAGE as u64)]
    );

    // The hole is available again, and comes back as zeros even though the
    // guest wrote to it — which is the obligation a space that never
    // shrinks has to meet.
    let reused = fixture.map(
        0,
        PAGE,
        prot::READ | prot::WRITE,
        map::PRIVATE | map::ANONYMOUS,
    );
    assert_eq!(reused, base as i64 + 2 * PAGE, "the hole was reused");
    assert_eq!(
        fixture.bytes(reused as u64, PAGE as usize),
        vec![0u8; PAGE as usize]
    );
    // And the pages around it still hold what was written.
    assert_eq!(fixture.bytes(base, 8), &[0x77; 8]);
    assert_eq!(fixture.bytes(base + 3 * PAGE as u64, 8), &[0x77; 8]);
}

#[test]
fn munmap_and_mprotect_refuse_an_unaligned_address() {
    let mut fixture = fixture("alignment");
    let base = fixture.anonymous(2 * PAGE);
    for row in [number::MUNMAP, number::MPROTECT] {
        assert_eq!(
            fixture.call(row, [base as i64 + 1, PAGE, prot::READ as i64, 0, 0, 0]),
            Errno::Invalid.as_result(),
            "row {row}"
        );
    }
    // A length of zero is `EINVAL` too, for both.
    assert_eq!(
        fixture.call(number::MUNMAP, [base as i64, 0, 0, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
    // Unmapping something that was never mapped is *not* an error: it is
    // how a program cleans up a range it is unsure about.
    assert_eq!(
        fixture.call(number::MUNMAP, [base as i64 + 64 * PAGE, PAGE, 0, 0, 0, 0]),
        0
    );
    // `mprotect` across a hole is `ENOMEM`, and Linux checks that before it
    // changes anything.
    assert_eq!(
        fixture.call(
            number::MPROTECT,
            [base as i64, 64 * PAGE, prot::READ as i64, 0, 0, 0]
        ),
        Errno::NoMemory.as_result()
    );
}

/// `mprotect` splits and records. Nothing is enforced — wasm has no page
/// protection — so what the record is for is `/proc/self/maps`.
#[test]
fn mprotect_splits_and_records() {
    let mut fixture = fixture("mprotect");
    let base = fixture.anonymous(4 * PAGE);
    assert_eq!(
        fixture.call(
            number::MPROTECT,
            [
                base as i64 + PAGE,
                2 * PAGE,
                (prot::READ | prot::EXEC) as i64,
                0,
                0,
                0
            ]
        ),
        0
    );
    let shape: Vec<_> = fixture
        .kernel
        .space
        .vmas()
        .iter()
        .filter(|vma| vma.start >= base && vma.start < base + 4 * PAGE as u64)
        .map(|vma| (vma.start - base, vma.length, vma.prot))
        .collect();
    assert_eq!(
        shape,
        vec![
            (0, PAGE as u64, prot::READ | prot::WRITE),
            (PAGE as u64, 2 * PAGE as u64, prot::READ | prot::EXEC),
            (3 * PAGE as u64, PAGE as u64, prot::READ | prot::WRITE),
        ]
    );
    // The bytes are untouched by a protection change.
    fixture.dirty(base + PAGE as u64, 8, 0x5a);
    assert_eq!(fixture.bytes(base + PAGE as u64, 8), &[0x5a; 8]);

    // `mprotect` *does* validate, which `mmap` does not — and `PROT_SEM`
    // is the one unknown-looking bit it accepts, doing nothing with it.
    assert_eq!(
        fixture.call(number::MPROTECT, [base as i64, PAGE, 0x40, 0, 0, 0]),
        Errno::Invalid.as_result(),
        "a protection bit that is not one"
    );
    assert_eq!(
        fixture.call(
            number::MPROTECT,
            [base as i64, PAGE, (prot::READ | 8) as i64, 0, 0, 0]
        ),
        0,
        "PROT_SEM is accepted and means nothing"
    );
    assert_eq!(
        fixture.kernel.space.find(base).expect("mapped").prot,
        prot::READ,
        "and is not recorded"
    );
}

// ---- the star: MADV_DONTNEED ----------------------------------------------

/// `madvise(MADV_DONTNEED)` has visible semantics, and getting that wrong
/// hands a program its own freed heap back.
///
/// glibc's arena-free path uses it — seven times in the trace this design
/// was built from — and on Linux a subsequent read of anonymous memory sees
/// zeros. A kernel that recorded the advice and moved on would be one where
/// `malloc` occasionally returns memory with somebody else's data in it, and
/// nothing would fail until something did.
#[test]
fn madv_dontneed_zeroes_the_range() {
    let mut fixture = fixture("dontneed");
    let base = fixture.anonymous(4 * PAGE);
    fixture.dirty(base, 4 * PAGE as usize, 0xde);

    // Advise away the middle two pages.
    assert_eq!(
        fixture.call(
            number::MADVISE,
            [
                base as i64 + PAGE,
                2 * PAGE,
                advice::DONTNEED as i64,
                0,
                0,
                0
            ]
        ),
        0
    );
    assert_eq!(
        fixture.bytes(base + PAGE as u64, 2 * PAGE as usize),
        vec![0u8; 2 * PAGE as usize],
        "the advised range reads as zeros"
    );
    // And only that range.
    assert_eq!(fixture.bytes(base, 8), &[0xde; 8]);
    assert_eq!(fixture.bytes(base + 3 * PAGE as u64, 8), &[0xde; 8]);
    // The mapping is still there: `MADV_DONTNEED` frees the contents, not
    // the address.
    assert!(fixture.kernel.space.find(base + PAGE as u64).is_some());

    // `MADV_FREE` is the lazy cousin, and zeroing eagerly implements it.
    fixture.dirty(base, 4 * PAGE as usize, 0xbe);
    assert_eq!(
        fixture.call(
            number::MADVISE,
            [base as i64, PAGE, advice::FREE as i64, 0, 0, 0]
        ),
        0
    );
    assert_eq!(fixture.bytes(base, PAGE as usize), vec![0u8; PAGE as usize]);

    // The advice that really is record-and-ignore does not touch anything.
    fixture.dirty(base, 64, 0x42);
    for what in [
        advice::NORMAL,
        advice::RANDOM,
        advice::SEQUENTIAL,
        advice::WILLNEED,
        advice::HUGEPAGE,
        advice::DONTDUMP,
    ] {
        assert_eq!(
            fixture.call(number::MADVISE, [base as i64, PAGE, what as i64, 0, 0, 0]),
            0,
            "advice {what}"
        );
    }
    assert_eq!(fixture.bytes(base, 8), &[0x42; 8]);

    // An advice this kernel does not know is `EINVAL`, which is what Linux
    // answers for one it does not know either.
    assert_eq!(
        fixture.call(number::MADVISE, [base as i64, PAGE, 9999, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
    // And an unaligned address is `EINVAL` before anything else.
    assert_eq!(
        fixture.call(
            number::MADVISE,
            [base as i64 + 1, PAGE, advice::DONTNEED as i64, 0, 0, 0]
        ),
        Errno::Invalid.as_result()
    );
}

/// The other half of the same obligation: a range that is unmapped and
/// mapped again is fresh memory, not the last owner's.
#[test]
fn a_remapped_range_comes_back_as_zeros() {
    let mut fixture = fixture("reuse");
    let first = fixture.anonymous(2 * PAGE);
    fixture.dirty(first, 2 * PAGE as usize, 0x99);
    assert_eq!(
        fixture.call(number::MUNMAP, [first as i64, 2 * PAGE, 0, 0, 0, 0]),
        0
    );

    let second = fixture.anonymous(2 * PAGE);
    assert_eq!(second, first, "the range was reused");
    assert_eq!(
        fixture.bytes(second, 2 * PAGE as usize),
        vec![0u8; 2 * PAGE as usize],
        "and it is not what the last owner left"
    );
}

/// `MADV_DONTNEED` on a file mapping is left alone rather than zeroed.
///
/// On Linux it restores the file's contents, which needs a faulting layer
/// this has not got. Zeroing instead would destroy data the guest can still
/// read, which is worse than doing nothing.
#[test]
fn madv_dontneed_does_not_touch_a_file_mapping() {
    let mut fixture = fixture("dontneed-file");
    let fd = fixture.open("/etc/hosts");
    let mapped = fixture.call(
        number::MMAP,
        [0, PAGE, prot::READ as i64, map::PRIVATE as i64, fd, 0],
    );
    assert!(mapped > 0);
    assert_eq!(fixture.bytes(mapped as u64, 9), b"127.0.0.1");
    assert_eq!(
        fixture.call(
            number::MADVISE,
            [mapped, PAGE, advice::DONTNEED as i64, 0, 0, 0]
        ),
        0
    );
    assert_eq!(
        fixture.bytes(mapped as u64, 9),
        b"127.0.0.1",
        "the file's bytes are still there"
    );
}

// ---- brk -------------------------------------------------------------------

/// `brk` is a bump pointer, and what matters about it is what happens when
/// it runs out: glibc allocates through it until it fails and then falls
/// back to `mmap`. That fallback is a tested path in every libc, and a
/// ceiling here is what exercises it.
#[test]
fn brk_bumps_within_its_arena_and_stops_at_the_ceiling() {
    let mut fixture = fixture("brk");
    // A request of zero asks where the break is, which is how every libc
    // starts.
    let start = fixture.call(number::BRK, [0, 0, 0, 0, 0, 0]);
    assert!(start > 0);
    assert_eq!(start % PAGE, 0);

    // Moving it up hands over zeroed memory.
    let grown = fixture.call(number::BRK, [start + 2 * PAGE, 0, 0, 0, 0, 0]);
    assert_eq!(grown, start + 2 * PAGE);
    assert_eq!(
        fixture.bytes(start as u64, 2 * PAGE as usize),
        vec![0u8; 2 * PAGE as usize]
    );
    fixture.dirty(start as u64, 2 * PAGE as usize, 0x33);

    // Past the ceiling the break does not move, and the answer is where it
    // still is — not an errno. glibc reads the unchanged value as failure;
    // an errno would be a divergence from a path every program takes.
    let refused = fixture.call(number::BRK, [start + (BRK_ARENA as i64) * 2, 0, 0, 0, 0, 0]);
    assert_eq!(refused, start + 2 * PAGE, "the break did not move");

    // Shrinking and growing again gives zeros back, not what was there.
    assert_eq!(fixture.call(number::BRK, [start, 0, 0, 0, 0, 0]), start);
    assert_eq!(
        fixture.call(number::BRK, [start + PAGE, 0, 0, 0, 0, 0]),
        start + PAGE
    );
    assert_eq!(
        fixture.bytes(start as u64, PAGE as usize),
        vec![0u8; PAGE as usize],
        "a re-grown break is fresh memory"
    );

    // An address below the arena is refused the same way: nothing moves.
    let below = fixture.call(number::BRK, [16, 0, 0, 0, 0, 0]);
    assert_eq!(below, start + PAGE);
}

/// The two arenas do not overlap: what `brk` owns is never handed out by
/// `mmap`, however many mappings are made.
#[test]
fn the_arenas_do_not_overlap() {
    let mut fixture = fixture("arenas");
    let start = fixture.call(number::BRK, [0, 0, 0, 0, 0, 0]) as u64;
    let ceiling = start + BRK_ARENA;
    for _ in 0..8 {
        let address = fixture.anonymous(PAGE);
        assert!(
            address >= ceiling,
            "a mapping at {address:#x} is inside the brk arena [{start:#x}, {ceiling:#x})"
        );
    }
    // And the break can still be moved through its whole arena afterwards.
    assert_eq!(
        fixture.call(number::BRK, [(ceiling - PAGE as u64) as i64, 0, 0, 0, 0, 0]),
        (ceiling - PAGE as u64) as i64
    );
}

// ---- file mappings ---------------------------------------------------------

/// A file mapping is an eager copy, and the bytes are the file's.
///
/// A copy for every kind of file, deliberately: POSIX leaves post-map
/// visibility of writes unspecified for a private mapping, so a copy is
/// conformant — and the alternative, pointing the guest into the shared
/// image blob, has no answer for a later `mprotect(PROT_WRITE)`, which Linux
/// permits however the descriptor was opened.
#[test]
fn a_file_mapping_holds_the_files_bytes() {
    let mut fixture = fixture("file-map");
    let fd = fixture.open("/etc/patterned");
    let mapped = fixture.call(
        number::MMAP,
        [0, 3 * PAGE, prot::READ as i64, map::PRIVATE as i64, fd, 0],
    );
    assert!(mapped > 0, "mmap failed with {mapped}");

    let expected: Vec<u8> = (0..PATTERNED as u32)
        .map(|index| (index % 251) as u8)
        .collect();
    assert_eq!(
        fixture.bytes(mapped as u64, 3 * PAGE as usize),
        &expected[..3 * PAGE as usize]
    );

    // Writing to the mapping does not change the file, which is what
    // `MAP_PRIVATE` means.
    fixture.dirty(mapped as u64, 16, 0xff);
    let buffer = fixture.place(&[0u8; 32]);
    let fd = fixture.open("/etc/patterned");
    assert_eq!(fixture.call(number::READ, [fd, buffer, 32, 0, 0, 0]), 32);
    assert_eq!(fixture.bytes(buffer as u64, 4), &expected[..4]);
}

/// A mapping that runs past the end of the file reads as zeros there, which
/// is what Linux guarantees for the tail of the last page.
#[test]
fn a_mapping_past_the_end_of_a_file_is_zeros() {
    let mut fixture = fixture("file-tail");
    let fd = fixture.open("/etc/hosts");
    let mapped = fixture.call(
        number::MMAP,
        [0, 2 * PAGE, prot::READ as i64, map::PRIVATE as i64, fd, 0],
    );
    assert!(mapped > 0);
    assert_eq!(fixture.bytes(mapped as u64, 20), b"127.0.0.1 localhost\n");
    assert_eq!(
        fixture.bytes(mapped as u64 + 20, 2 * PAGE as usize - 20),
        vec![0u8; 2 * PAGE as usize - 20],
        "everything past the file is zeros"
    );
}

/// The sequence a dynamic loader performs: map the whole file, then
/// `MAP_FIXED` each segment over it at its own file offset.
///
/// This is the shape from the trace this design was built from, tested as
/// pure address-space surgery several milestones before a loader exists to
/// run it — so that the tier which needs it inherits a substrate that has
/// been checked rather than one that looks plausible.
#[test]
fn the_loader_carving_sequence_lands_the_right_bytes() {
    let mut fixture = fixture("carving");
    let fd = fixture.open("/etc/patterned");
    let expected: Vec<u8> = (0..PATTERNED as u32)
        .map(|index| (index % 251) as u8)
        .collect();

    // The extent: the whole file, read-only, wherever the kernel likes.
    let extent = fixture.call(
        number::MMAP,
        [0, 3 * PAGE, prot::READ as i64, map::PRIVATE as i64, fd, 0],
    );
    assert!(extent > 0);

    // Then each segment over it, at its own file offset — the second page
    // as text, the third as read-only data.
    //
    // The middle segment is carved writable rather than executable, and
    // that is the half of ld.so's sequence this fixture can honestly
    // represent: an executable mapping of a file the bake did not translate
    // is a named refusal — see
    // `an_executable_mapping_of_an_untranslated_file_is_refused`, which owns
    // that rule — while the writable data segment is the one a real loader
    // genuinely re-copies, and the one whose protection differs from its
    // neighbours' so that the shape below can tell them apart. A real
    // loader carving a real module maps text executable and is answered,
    // because the module *was* translated; `tests/dynamic_boot.rs` runs
    // exactly that.
    let text = fixture.call(
        number::MMAP,
        [
            extent + PAGE,
            PAGE,
            (prot::READ | prot::WRITE) as i64,
            (map::PRIVATE | map::FIXED) as i64,
            fd,
            PAGE,
        ],
    );
    assert_eq!(text, extent + PAGE);
    let data = fixture.call(
        number::MMAP,
        [
            extent + 2 * PAGE,
            PAGE,
            prot::READ as i64,
            (map::PRIVATE | map::FIXED) as i64,
            fd,
            2 * PAGE,
        ],
    );
    assert_eq!(data, extent + 2 * PAGE);

    // Every byte of the extent is still the file's, because each segment
    // was copied from the offset that matches where it landed.
    assert_eq!(
        fixture.bytes(extent as u64, 3 * PAGE as usize),
        &expected[..3 * PAGE as usize]
    );

    // And the tree records three mappings with the protections asked for.
    let shape: Vec<_> = fixture
        .kernel
        .space
        .vmas()
        .iter()
        .filter(|vma| vma.start >= extent as u64 && vma.start < extent as u64 + 3 * PAGE as u64)
        .map(|vma| (vma.start - extent as u64, vma.length, vma.prot))
        .collect();
    assert_eq!(
        shape,
        vec![
            (0, PAGE as u64, prot::READ),
            (PAGE as u64, PAGE as u64, prot::READ | prot::WRITE),
            (2 * PAGE as u64, PAGE as u64, prot::READ),
        ]
    );

    // A segment mapped from the *wrong* offset would put the wrong bytes
    // there, which is what makes the check above worth making.
    let wrong = fixture.call(
        number::MMAP,
        [
            extent + PAGE,
            PAGE,
            prot::READ as i64,
            (map::PRIVATE | map::FIXED) as i64,
            fd,
            2 * PAGE,
        ],
    );
    assert_eq!(wrong, extent + PAGE);
    assert_ne!(
        fixture.bytes(extent as u64 + PAGE as u64, 16),
        &expected[PAGE as usize..PAGE as usize + 16]
    );
}

#[test]
fn a_file_mapping_refuses_what_cannot_be_mapped() {
    let mut fixture = fixture("file-refusals");
    // A descriptor that is not open.
    assert_eq!(
        fixture.call(
            number::MMAP,
            [0, PAGE, prot::READ as i64, map::PRIVATE as i64, 99, 0]
        ),
        Errno::BadFile.as_result()
    );
    // A directory.
    let directory = fixture.path("/etc");
    let fd = fixture.call(
        number::OPEN,
        [
            directory,
            kisal::file::open_flags::DIRECTORY as i64,
            0,
            0,
            0,
            0,
        ],
    );
    assert!(fd >= 0);
    assert_eq!(
        fixture.call(
            number::MMAP,
            [0, PAGE, prot::READ as i64, map::PRIVATE as i64, fd, 0]
        ),
        Errno::NoDevice.as_result()
    );
    // Standard output, which is a stream and has no bytes at an offset.
    assert_eq!(
        fixture.call(
            number::MMAP,
            [0, PAGE, prot::READ as i64, map::PRIVATE as i64, 1, 0]
        ),
        Errno::NoDevice.as_result()
    );
}

/// A writable shared file mapping needs write-back, and nothing here has
/// built it. A named fault rather than a mapping whose writes vanish.
#[test]
fn a_writable_shared_file_mapping_is_refused_by_name() {
    let mut fixture = fixture("shared-write");
    let fd = fixture.open("/etc/hosts");
    let outcome = fixture.kernel.dispatch(
        number::MMAP,
        Arguments::new([
            0,
            PAGE,
            (prot::READ | prot::WRITE) as i64,
            map::SHARED as i64,
            fd,
            0,
        ]),
    );
    let Outcome::Fault(fault) = outcome else {
        panic!("a writable shared mapping produced {outcome:?}");
    };
    let mut message = String::new();
    fault.message(&mut message);
    assert!(message.contains("write-back"), "{message}");

    // Read-only shared is fine: it is indistinguishable from private when
    // both are copies.
    assert!(
        fixture.call(
            number::MMAP,
            [0, PAGE, prot::READ as i64, map::SHARED as i64, fd, 0]
        ) > 0
    );
}

// ---- mremap ----------------------------------------------------------------

#[test]
fn mremap_grows_in_place_shrinks_and_moves() {
    let mut fixture = fixture("mremap");
    let base = fixture.anonymous(2 * PAGE);
    fixture.dirty(base, 2 * PAGE as usize, 0x21);

    // Growing into free space stays put, and the new part is zeros.
    let grown = fixture.call(number::MREMAP, [base as i64, 2 * PAGE, 4 * PAGE, 0, 0, 0]);
    assert_eq!(grown, base as i64, "there was room above");
    assert_eq!(fixture.bytes(base, 8), &[0x21; 8], "the old bytes survived");
    assert_eq!(
        fixture.bytes(base + 2 * PAGE as u64, 2 * PAGE as usize),
        vec![0u8; 2 * PAGE as usize],
        "and the new part is zeros"
    );

    // Shrinking truncates, and the tail becomes available again.
    let shrunk = fixture.call(number::MREMAP, [base as i64, 4 * PAGE, PAGE, 0, 0, 0]);
    assert_eq!(shrunk, base as i64);
    assert_eq!(
        fixture
            .kernel
            .space
            .find(base)
            .expect("still mapped")
            .length,
        PAGE as u64
    );
    assert!(fixture.kernel.space.find(base + PAGE as u64).is_none());

    // Blocked from growing in place, and told it may move: it moves, and
    // carries the bytes.
    let blocker = fixture.map(
        base as i64 + PAGE,
        PAGE,
        prot::READ,
        map::PRIVATE | map::ANONYMOUS | map::FIXED,
    );
    assert_eq!(blocker, base as i64 + PAGE);
    fixture.dirty(base, 8, 0x64);
    let moved = fixture.call(
        number::MREMAP,
        [
            base as i64,
            PAGE,
            2 * PAGE,
            kisal::space::remap::MAYMOVE as i64,
            0,
            0,
        ],
    );
    assert!(moved > 0 && moved != base as i64, "it had to move");
    assert_eq!(
        fixture.bytes(moved as u64, 8),
        &[0x64; 8],
        "the bytes came along"
    );
    assert_eq!(
        fixture.bytes(moved as u64 + PAGE as u64, PAGE as usize),
        vec![0u8; PAGE as usize],
        "and the new part is zeros"
    );
    assert!(
        fixture.kernel.space.find(base).is_none(),
        "the old range is gone"
    );

    // Without `MREMAP_MAYMOVE` and with nowhere to grow, `ENOMEM`.
    let pinned = fixture.anonymous(PAGE);
    let _ = fixture.map(
        pinned as i64 + PAGE,
        PAGE,
        prot::READ,
        map::PRIVATE | map::ANONYMOUS | map::FIXED,
    );
    assert_eq!(
        fixture.call(number::MREMAP, [pinned as i64, PAGE, 2 * PAGE, 0, 0, 0]),
        Errno::NoMemory.as_result()
    );
}

#[test]
fn mremap_refuses_what_it_cannot_mean() {
    let mut fixture = fixture("mremap-edges");
    let base = fixture.anonymous(2 * PAGE);
    // An address that is not a mapping's start.
    assert_eq!(
        fixture.call(
            number::MREMAP,
            [base as i64 + PAGE, PAGE, 2 * PAGE, 0, 0, 0]
        ),
        Errno::Fault.as_result()
    );
    // A length that is not the mapping's.
    assert_eq!(
        fixture.call(number::MREMAP, [base as i64, PAGE, 2 * PAGE, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
    // A flag that promises something specific about where the result lands.
    assert_eq!(
        fixture.call(
            number::MREMAP,
            [
                base as i64,
                2 * PAGE,
                4 * PAGE,
                kisal::space::remap::FIXED as i64,
                0,
                0
            ]
        ),
        Errno::Invalid.as_result()
    );
    assert_eq!(
        fixture.call(number::MREMAP, [base as i64, 2 * PAGE, 0, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
}

// ---- /proc/self/maps -------------------------------------------------------

/// The file glibc's `pthread_getattr_np` reads to find a thread's stack
/// bounds. It is a rendering of the tree, so it has to change when the tree
/// does — a snapshot taken at boot would describe an address space that no
/// longer exists.
#[test]
fn proc_self_maps_renders_the_tree_as_it_stands() {
    let mut fixture = fixture("maps");
    let anonymous = fixture.anonymous(2 * PAGE);
    let fd = fixture.open("/etc/patterned");
    let file = fixture.call(
        number::MMAP,
        [0, PAGE, prot::READ as i64, map::PRIVATE as i64, fd, PAGE],
    );
    assert!(file > 0);
    // Executable *afterwards*, which is the only way this kernel reaches an
    // `r-xp` file line for a file the bake did not translate: `mmap` refuses
    // that protection on such a file by name, and `mprotect` records
    // whatever it is asked for and enforces nothing, exactly as the design
    // says. So the state is reachable, and the rendering has to describe it.
    assert_eq!(
        fixture.call(
            number::MPROTECT,
            [file, PAGE, (prot::READ | prot::EXEC) as i64, 0, 0, 0]
        ),
        0
    );

    let rendered = fixture.maps();
    let lines: Vec<&str> = rendered.lines().collect();
    assert!(lines.len() >= 2, "{rendered}");

    // Address order, which is what a parser walking the file assumes.
    let starts: Vec<u64> = lines
        .iter()
        .map(|line| u64::from_str_radix(line.split('-').next().expect("start"), 16).expect("hex"))
        .collect();
    let mut sorted = starts.clone();
    sorted.sort_unstable();
    assert_eq!(starts, sorted);

    let anonymous_line = lines
        .iter()
        .find(|line| line.starts_with(&format!("{anonymous:x}-")))
        .unwrap_or_else(|| panic!("no line for the anonymous mapping:\n{rendered}"));
    assert!(
        anonymous_line.contains(&format!("{:x} rw-p ", anonymous + 2 * PAGE as u64)),
        "{anonymous_line}"
    );
    assert!(
        anonymous_line.ends_with("00000000 00:00 0 "),
        "{anonymous_line}"
    );

    let file_line = lines
        .iter()
        .find(|line| line.starts_with(&format!("{file:x}-")))
        .unwrap_or_else(|| panic!("no line for the file mapping:\n{rendered}"));
    assert!(file_line.contains(" r-xp "), "{file_line}");
    assert!(
        file_line.contains(&format!(" {:08x} ", PAGE)),
        "the file offset is the mapped one: {file_line}"
    );
    assert!(
        file_line.trim_end().ends_with("/etc/patterned"),
        "the pathname is the file's: {file_line}"
    );

    // Unmapping removes the line, which is the property a snapshot could
    // not have.
    assert_eq!(
        fixture.call(number::MUNMAP, [anonymous as i64, 2 * PAGE, 0, 0, 0, 0]),
        0
    );
    let after = fixture.maps();
    assert!(
        !after.contains(&format!("{anonymous:x}-")),
        "the unmapped range is still listed:\n{after}"
    );
}

/// And the guest can read it, through the ordinary file rows.
#[test]
fn a_guest_reads_proc_self_maps_as_a_file() {
    let mut fixture = fixture("maps-read");
    let mapped = fixture.anonymous(PAGE);
    let fd = fixture.open("/proc/self/maps");

    let buffer = fixture.place(&[0u8; 4096]);
    let read = fixture.call(number::READ, [fd, buffer, 4096, 0, 0, 0]);
    assert!(read > 0, "read returned {read}");
    let text = String::from_utf8(fixture.bytes(buffer as u64, read as usize).to_vec())
        .expect("the file is text");
    assert!(
        text.contains(&format!("{mapped:x}-")),
        "the mapping is not in the file:\n{text}"
    );
    // Reading it twice gives the same answer, since nothing changed in
    // between — the second read goes through the same generator.
    let again = fixture.maps();
    assert!(again.contains(&format!("{mapped:x}-")), "{again}");

    // Reading on returns nothing more: the offset advanced past the end.
    assert_eq!(fixture.call(number::READ, [fd, buffer, 4096, 0, 0, 0]), 0);
}

/// A range a fixed mapping took must not be handed out again.
///
/// The address space knows two things: which mappings exist, and which
/// ranges are free to give away. `MAP_FIXED` used to update only the first —
/// it removed the mappings it replaced, and left the range sitting in the
/// free pool. The next `mmap` with no hint could then be handed an address a
/// mapping already occupied, and the two would share memory: one program's
/// writes landing in another's buffer, with `/proc/self/maps` showing both.
///
/// It stayed invisible because nothing enforced what the tree recorded. The
/// interpreter's page table does, so it noticed the tree holding two
/// overlapping mappings at once.
#[test]
fn a_fixed_mapping_is_not_handed_out_twice() {
    let mut fixture = fixture("fixed-reuse");
    // A region, then most of it given back, so the pool holds a big range.
    let base = fixture.anonymous(4 * PAGE);
    let shrunk = fixture.call(
        number::MUNMAP,
        [base as i64 + PAGE, 3 * PAGE, 0, 0, 0, 0],
    );
    assert_eq!(shrunk, 0);

    // A fixed mapping inside what the pool now holds.
    let claimed = base + PAGE as u64;
    let fixed = fixture.map(
        claimed as i64,
        PAGE,
        prot::READ,
        map::PRIVATE | map::ANONYMOUS | map::FIXED,
    );
    assert_eq!(fixed, claimed as i64);

    // Now ask for memory repeatedly. None of it may land on the fixed
    // mapping, and no two answers may overlap each other either.
    let mut taken: Vec<(u64, u64)> = vec![(claimed, PAGE as u64)];
    for _ in 0..4 {
        let address = fixture.anonymous(PAGE);
        for (start, length) in &taken {
            assert!(
                address + PAGE as u64 <= *start || address >= start + length,
                "{address:#x} overlaps the mapping at {start:#x}"
            );
        }
        taken.push((address, PAGE as u64));
    }
}
