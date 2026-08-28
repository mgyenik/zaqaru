/* The M5 differential guest: the memory rows, exercised from C.
 *
 * Addresses are deliberately absent from the report. The two sides lay out
 * their address spaces differently and always will — one is a real kernel's
 * virtual memory, the other is a stretch of wasm linear memory — so what is
 * compared is the *rules*: whether a mapping reads as zeros, whether it keeps
 * what was written, whether replacing part of it leaves the rest, which
 * arguments are refused and with which errno.
 *
 * `brk` is not here. The native side runs inside this test process, whose
 * heap is the break, and moving it would be moving glibc's allocator out from
 * under the harness. It is covered natively in `kisal/tests/memory.rs`, where
 * the arena belongs to nobody else.
 */

typedef long ssize;

#define SYS_read 0
#define SYS_write 1
#define SYS_open 2
#define SYS_close 3
#define SYS_mmap 9
#define SYS_mprotect 10
#define SYS_munmap 11
#define SYS_madvise 28

#define O_RDONLY 0

#define PROT_NONE 0
#define PROT_READ 1
#define PROT_WRITE 2
#define PROT_EXEC 4

#define MAP_SHARED 0x01
#define MAP_PRIVATE 0x02
#define MAP_FIXED 0x10
#define MAP_ANONYMOUS 0x20
#define MAP_FIXED_NOREPLACE 0x100000

#define MADV_DONTNEED 4
#define MADV_FREE 8
#define MADV_HUGEPAGE 14

#define PAGE 4096

static ssize sys3(long number, long a, long b, long c) {
	ssize result;
	__asm__ volatile("syscall"
	                 : "=a"(result)
	                 : "a"(number), "D"(a), "S"(b), "d"(c)
	                 : "rcx", "r11", "memory");
	return result;
}

static ssize sys6(long number, long a, long b, long c, long d, long e, long f) {
	ssize result;
	register long r10 __asm__("r10") = d;
	register long r8 __asm__("r8") = e;
	register long r9 __asm__("r9") = f;
	__asm__ volatile("syscall"
	                 : "=a"(result)
	                 : "a"(number), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8), "r"(r9)
	                 : "rcx", "r11", "memory");
	return result;
}

static long join(volatile char *out, const char *prefix, const char *name) {
	long at = 0;
	for (long i = 0; prefix[i] != 0; i++) {
		out[at++] = prefix[i];
	}
	for (long i = 0; name[i] != 0; i++) {
		out[at++] = name[i];
	}
	out[at] = 0;
	return at;
}

static void emit(long out, long tag, long result, long a, long b, long c, long d, long e) {
	long record[8];
	record[0] = tag;
	record[1] = result;
	record[2] = a;
	record[3] = b;
	record[4] = c;
	record[5] = d;
	record[6] = e;
	record[7] = 0;
	sys3(SYS_write, out, (long)record, sizeof(record));
}

/* Whether every byte of a range is zero, and a sum of the bytes — enough to
 * tell "the right bytes" from "some other bytes" without reporting an
 * address. */
static long all_zero(volatile unsigned char *at, long length) {
	for (long i = 0; i < length; i++) {
		if (at[i] != 0) {
			return 0;
		}
	}
	return 1;
}

static long checksum(volatile unsigned char *at, long length) {
	long sum = 0;
	for (long i = 0; i < length; i++) {
		sum = sum * 31 + at[i];
	}
	return sum;
}

static void fill(volatile unsigned char *at, long length, unsigned char byte) {
	for (long i = 0; i < length; i++) {
		at[i] = byte;
	}
}

/* An errno as itself, and any success as 1: an address is the one thing the
 * two sides cannot agree on. */
static long outcome(long result) {
	return result < 0 ? result : 1;
}

static long anonymous(long length) {
	return sys6(SYS_mmap, 0, length, PROT_READ | PROT_WRITE,
	            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
}

long guest_memory(long out, const char *prefix) {
	volatile char path[512];
	static const char none[] = "";
	if (prefix == 0) {
		prefix = none;
	}

	/* Anonymous memory reads as zeros and keeps what is written. */
	long base = anonymous(4 * PAGE);
	emit(out, 100, base > 0, 0, 0, 0, 0, 0);
	if (base <= 0) {
		return 0;
	}
	volatile unsigned char *bytes = (volatile unsigned char *)base;
	emit(out, 101, all_zero(bytes, 4 * PAGE), 0, 0, 0, 0, 0);
	fill(bytes, 4 * PAGE, 0xa5);
	emit(out, 102, checksum(bytes, 64), 0, 0, 0, 0, 0);

	/* A second mapping is its own zeros, not a view of the first. */
	long second = anonymous(PAGE);
	emit(out, 103, second > 0, all_zero((volatile unsigned char *)second, PAGE), 0, 0, 0, 0);

	/* MAP_FIXED replaces what it lands on, and leaves the rest. */
	long middle = base + PAGE;
	long replaced = sys6(SYS_mmap, middle, 2 * PAGE, PROT_READ | PROT_WRITE,
	                     MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
	emit(out, 110, replaced == middle, 0, 0, 0, 0, 0);
	emit(out, 111, all_zero((volatile unsigned char *)middle, 2 * PAGE), 0, 0, 0, 0, 0);
	emit(out, 112, checksum(bytes, 64), 0, 0, 0, 0, 0);
	emit(out, 113, checksum((volatile unsigned char *)(base + 3 * PAGE), 64), 0, 0, 0, 0, 0);

	/* MAP_FIXED_NOREPLACE refuses rather than destroying. */
	emit(out, 114,
	     outcome(sys6(SYS_mmap, middle, PAGE, PROT_READ,
	                  MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0)),
	     0, 0, 0, 0, 0);

	/* A partial unmap leaves the parts it did not cover. */
	fill(bytes, 4 * PAGE, 0x5c);
	emit(out, 120, sys3(SYS_munmap, base + PAGE, PAGE, 0), 0, 0, 0, 0, 0);
	emit(out, 121, checksum(bytes, 64), 0, 0, 0, 0, 0);
	emit(out, 122, checksum((volatile unsigned char *)(base + 2 * PAGE), 64), 0, 0, 0, 0, 0);
	/* And the hole reads as zeros when it is mapped again. */
	long reused = sys6(SYS_mmap, base + PAGE, PAGE, PROT_READ | PROT_WRITE,
	                   MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
	emit(out, 123, reused == base + PAGE,
	     all_zero((volatile unsigned char *)(base + PAGE), PAGE), 0, 0, 0, 0);

	/* madvise(MADV_DONTNEED) has visible semantics: the range reads as
	 * zeros afterwards. This is the one every allocator depends on. */
	fill(bytes, 4 * PAGE, 0xd7);
	emit(out, 130, sys3(SYS_madvise, base + PAGE, 2 * PAGE, MADV_DONTNEED), 0, 0, 0, 0, 0);
	emit(out, 131, all_zero((volatile unsigned char *)(base + PAGE), 2 * PAGE), 0, 0, 0, 0, 0);
	emit(out, 132, checksum(bytes, 64), 0, 0, 0, 0, 0);
	emit(out, 133, checksum((volatile unsigned char *)(base + 3 * PAGE), 64), 0, 0, 0, 0, 0);
	/* MADV_FREE is the lazy cousin; eager zeroing implements it. */
	fill(bytes, PAGE, 0xe2);
	emit(out, 134, sys3(SYS_madvise, base, PAGE, MADV_FREE), 0, 0, 0, 0, 0);
	/* Advice that really is record-and-ignore changes nothing. */
	fill(bytes, 64, 0x42);
	emit(out, 135, sys3(SYS_madvise, base, PAGE, MADV_HUGEPAGE), 0, 0, 0, 0, 0);
	emit(out, 136, checksum(bytes, 64), 0, 0, 0, 0, 0);

	/* mprotect splits and records; the bytes are untouched by it. */
	fill(bytes, 4 * PAGE, 0x31);
	emit(out, 140, sys3(SYS_mprotect, base + PAGE, PAGE, PROT_READ), 0, 0, 0, 0, 0);
	emit(out, 141, checksum((volatile unsigned char *)(base + PAGE), 64), 0, 0, 0, 0, 0);
	emit(out, 142, sys3(SYS_mprotect, base + PAGE, PAGE, PROT_READ | PROT_WRITE), 0, 0, 0, 0, 0);

	/* The arguments each row refuses.
	 *
	 * An `mmap` that succeeds reports 1 rather than where it landed: the
	 * two sides lay out their address spaces differently, so an address
	 * is the one thing here that cannot be compared. An errno is
	 * compared exactly, which is the part under test. */
	emit(out, 200, outcome(sys6(SYS_mmap, 0, 0, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0)), 0, 0, 0, 0, 0);
	/* Unknown protection bits: `mmap` ignores them where `mprotect`
	 * refuses them, which is an asymmetry Linux really has. */
	emit(out, 201, outcome(sys6(SYS_mmap, 0, PAGE, 0x40, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0)), 0, 0, 0, 0, 0);
	emit(out, 202, outcome(sys6(SYS_mmap, 0, PAGE, PROT_READ, MAP_ANONYMOUS, -1, 0)), 0, 0, 0, 0, 0);
	emit(out, 203, outcome(sys6(SYS_mmap, base + 1, PAGE, PROT_READ,
	                            MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0)), 0, 0, 0, 0, 0);
	emit(out, 204, sys3(SYS_munmap, base + 1, PAGE, 0), 0, 0, 0, 0, 0);
	emit(out, 205, sys3(SYS_munmap, base, 0, 0), 0, 0, 0, 0, 0);
	emit(out, 206, sys3(SYS_mprotect, base + 1, PAGE, PROT_READ), 0, 0, 0, 0, 0);
	emit(out, 207, sys3(SYS_mprotect, base, PAGE, 0x40), 0, 0, 0, 0, 0);
	emit(out, 208, sys3(SYS_madvise, base + 1, PAGE, MADV_DONTNEED), 0, 0, 0, 0, 0);
	emit(out, 209, sys3(SYS_madvise, base, PAGE, 9999), 0, 0, 0, 0, 0);
	/* Unmapping what was never mapped is not an error. */
	long spare = anonymous(PAGE);
	sys3(SYS_munmap, spare, PAGE, 0);
	emit(out, 210, sys3(SYS_munmap, spare, PAGE, 0), 0, 0, 0, 0, 0);

	/* A file mapping holds the file's bytes, and past its end, zeros. */
	join(path, prefix, "/patterned");
	long fd = sys3(SYS_open, (long)path, O_RDONLY, 0);
	emit(out, 300, fd >= 0, 0, 0, 0, 0, 0);
	if (fd >= 0) {
		/* Five pages: the file is four and a bit, so this covers its
		 * last, partial page and stops there. A page *entirely* past
		 * the end is not backed on Linux and touching one raises
		 * SIGBUS — a fault, which is the one thing this kernel has no
		 * way to reproduce. The zero-fill of the partial page is the
		 * portable guarantee, and it is what is checked. */
		long mapped = sys6(SYS_mmap, 0, 5 * PAGE, PROT_READ, MAP_PRIVATE, fd, 0);
		emit(out, 301, mapped > 0, 0, 0, 0, 0, 0);
		if (mapped > 0) {
			volatile unsigned char *at = (volatile unsigned char *)mapped;
			emit(out, 302, checksum(at, 4 * PAGE), 0, 0, 0, 0, 0);
			/* Against `read(2)` of the same file: the two must agree. */
			long buffer = anonymous(8 * PAGE);
			long read = sys3(SYS_read, fd, buffer, 4 * PAGE);
			emit(out, 303, read, checksum((volatile unsigned char *)buffer, 4 * PAGE), 0, 0, 0, 0);
			/* The last page holds the file's tail and then zeros. */
			emit(out, 304, all_zero(at + 4 * PAGE + 1000, PAGE - 1000), 0, 0, 0, 0, 0);
			/* A mapping at a page offset starts there in the file. */
			long offset_map = sys6(SYS_mmap, 0, PAGE, PROT_READ, MAP_PRIVATE, fd, 2 * PAGE);
			emit(out, 305, offset_map > 0, 0, 0, 0, 0, 0);
			if (offset_map > 0) {
				emit(out, 306, checksum((volatile unsigned char *)offset_map, PAGE),
				     checksum(at + 2 * PAGE, PAGE), 0, 0, 0, 0);
			}
			/* An unaligned file offset is refused. */
			emit(out, 307, outcome(sys6(SYS_mmap, 0, PAGE, PROT_READ, MAP_PRIVATE, fd, 1)), 0, 0, 0, 0, 0);
		}

		/* The loader's carving sequence: the whole file, then each
		 * segment over it at its own offset. Every byte must still be
		 * the file's afterwards. */
		long extent = sys6(SYS_mmap, 0, 4 * PAGE, PROT_READ, MAP_PRIVATE, fd, 0);
		emit(out, 310, extent > 0, 0, 0, 0, 0, 0);
		if (extent > 0) {
			long text = sys6(SYS_mmap, extent + PAGE, PAGE, PROT_READ | PROT_EXEC,
			                 MAP_PRIVATE | MAP_FIXED, fd, PAGE);
			emit(out, 311, text == extent + PAGE, 0, 0, 0, 0, 0);
			long data = sys6(SYS_mmap, extent + 2 * PAGE, 2 * PAGE, PROT_READ,
			                 MAP_PRIVATE | MAP_FIXED, fd, 2 * PAGE);
			emit(out, 312, data == extent + 2 * PAGE, 0, 0, 0, 0, 0);
			emit(out, 313, checksum((volatile unsigned char *)extent, 4 * PAGE), 0, 0, 0, 0, 0);
		}
		sys3(SYS_close, fd, 0, 0);
	}

	return 0;
}
