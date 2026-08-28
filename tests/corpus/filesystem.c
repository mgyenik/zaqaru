/* The M3 differential guest: the read-only filesystem, exercised from C.
 *
 * No libc. Every call is the `syscall` instruction directly, which is what
 * makes this runnable on both sides — assembled for x86-64 the real kernel
 * answers it, transpiled it reaches kisal — and what keeps the comparison
 * about the kernel rather than about a C library's buffering.
 *
 * Paths are built from a caller-supplied prefix because the two sides mount
 * the same tree in different places: natively it is a directory under /tmp,
 * and in the image it is the root. Everything else is identical, which is the
 * point.
 *
 * The report is binary rather than formatted: `long` records written straight
 * out, so there is no `printf` and nothing for a compiler to turn into a
 * `memcpy` call. Fields whose values cannot agree across a real filesystem
 * and an image — `st_dev`, `st_ino`, `st_blocks` — are deliberately absent;
 * see `tests/filesystem_differential.rs` for why each one is excluded.
 */

typedef long ssize;

#define SYS_read 0
#define SYS_write 1
#define SYS_open 2
#define SYS_close 3
#define SYS_stat 4
#define SYS_lstat 6
#define SYS_lseek 8
#define SYS_getdents64 217
#define SYS_readlink 89
#define SYS_access 21
#define SYS_statx 332

#define AT_FDCWD -100

#define O_RDONLY 0
#define O_DIRECTORY 0200000

static ssize sys3(long number, long a, long b, long c) {
	ssize result;
	__asm__ volatile("syscall"
	                 : "=a"(result)
	                 : "a"(number), "D"(a), "S"(b), "d"(c)
	                 : "rcx", "r11", "memory");
	return result;
}

static ssize sys5(long number, long a, long b, long c, long d, long e) {
	ssize result;
	register long r10 __asm__("r10") = d;
	register long r8 __asm__("r8") = e;
	__asm__ volatile("syscall"
	                 : "=a"(result)
	                 : "a"(number), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8)
	                 : "rcx", "r11", "memory");
	return result;
}

/* Joins the prefix and a relative path into `out`, which the caller owns.
 * `volatile` on the destination is deliberate: it stops the compiler
 * recognising the loop as a `memcpy` and calling one, which would be an
 * undefined symbol in a container that links no libc. */
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

/* One record: a tag, a result, and up to six values. Fixed width so the two
 * sides' reports can be compared without parsing anything. */
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

/* The comparable fields of `struct stat`, at the x86-64 offsets. */
#define ST_NLINK 2  /* byte 16  */
#define ST_MODE 6   /* byte 24, low half */
#define ST_SIZE 6   /* byte 48  */

static long stat_field(const long *stat, long byte_offset) {
	return stat[byte_offset / 8];
}

static void report_stat(long out, long tag, long number, volatile char *path) {
	long stat[18]; /* 144 bytes */
	long result = sys3(number, (long)path, (long)stat, 0);
	if (result < 0) {
		emit(out, tag, result, 0, 0, 0, 0, 0);
		return;
	}
	long mode_and_uid = stat_field(stat, 24);
	long gid_and_pad = stat_field(stat, 32);
	emit(out, tag, 0,
	     stat_field(stat, 16),          /* st_nlink */
	     mode_and_uid & 0xffffffff,     /* st_mode  */
	     stat_field(stat, 48),          /* st_size  */
	     (mode_and_uid >> 32) |         /* st_uid   */
	         ((gid_and_pad & 0xffffffff) << 32), /* st_gid packed above it */
	     stat_field(stat, 88));         /* st_mtime seconds */
}

long guest_filesystem(long out, const char *prefix) {
	volatile char path[512];
	static const char none[] = "";

	/* A null prefix means the tree is mounted at the root, which is how the
	 * emulated side runs. Spelling it this way means the harness never has
	 * to place anything in guest memory — there is no host-side lifetime to
	 * get wrong, because there is no host-side buffer. */
	if (prefix == 0) {
		prefix = none;
	}

	/* Metadata, present and absent. A miss is most of a real workload's
	 * traffic, so it is checked as carefully as a hit. */
	static const char *const names[] = {
	    "/etc/hosts", "/etc/hostname", "/script", "/usr/lib/libthing.so",
	    "/etc",       "/",             "/lib",    "/hosts-link",
	    "/etc/nothing", "/nowhere/at/all", "/etc/hosts/deeper", "/etc/hosts/",
	    0,
	};
	for (long i = 0; names[i] != 0; i++) {
		join(path, prefix, names[i]);
		report_stat(out, 100 + i, SYS_stat, path);
		report_stat(out, 200 + i, SYS_lstat, path);
	}

	/* Contents, and the offset behaviour around them. */
	join(path, prefix, "/etc/hosts");
	long fd = sys3(SYS_open, (long)path, O_RDONLY, 0);
	emit(out, 300, fd >= 0, 0, 0, 0, 0, 0);
	if (fd >= 0) {
		volatile char buffer[64];
		long read = sys3(SYS_read, fd, (long)buffer, 8);
		long first = 0;
		for (long i = 0; i < 8; i++) {
			first = (first << 8) | (unsigned char)buffer[i];
		}
		emit(out, 301, read, first, 0, 0, 0, 0);

		emit(out, 302, sys3(SYS_lseek, fd, 10, 0), 0, 0, 0, 0, 0);
		read = sys3(SYS_read, fd, (long)buffer, 64);
		long tail = 0;
		for (long i = 0; i < 8; i++) {
			tail = (tail << 8) | (unsigned char)buffer[i];
		}
		emit(out, 303, read, tail, 0, 0, 0, 0);

		emit(out, 304, sys3(SYS_lseek, fd, 0, 2), 0, 0, 0, 0, 0);
		emit(out, 305, sys3(SYS_lseek, fd, -3, 1), 0, 0, 0, 0, 0);
		emit(out, 306, sys3(SYS_lseek, fd, -1, 0), 0, 0, 0, 0, 0);
		emit(out, 307, sys3(SYS_read, fd, (long)buffer, 64), 0, 0, 0, 0, 0);
		emit(out, 308, sys3(SYS_close, fd, 0, 0), 0, 0, 0, 0, 0);
		emit(out, 309, sys3(SYS_close, fd, 0, 0), 0, 0, 0, 0, 0);
	}

	/* Opening things that are not ordinary readable files. */
	join(path, prefix, "/etc");
	emit(out, 320, sys3(SYS_open, (long)path, O_RDONLY | O_DIRECTORY, 0) >= 0, 0, 0, 0, 0, 0);
	join(path, prefix, "/etc/hosts");
	emit(out, 321, sys3(SYS_open, (long)path, O_RDONLY | O_DIRECTORY, 0), 0, 0, 0, 0, 0);
	join(path, prefix, "/etc/nothing");
	emit(out, 322, sys3(SYS_open, (long)path, O_RDONLY, 0), 0, 0, 0, 0, 0);

	/* Symlink targets, read without following. */
	static const char *const links[] = {"/lib", "/hosts-link", "/absolute-link",
	                                    "/etc/hosts", 0};
	for (long i = 0; links[i] != 0; i++) {
		volatile char target[128];
		join(path, prefix, links[i]);
		long length = sys3(SYS_readlink, (long)path, (long)target, 128);
		long packed = 0;
		if (length > 0) {
			for (long j = 0; j < 8 && j < length; j++) {
				packed = (packed << 8) | (unsigned char)target[j];
			}
		}
		emit(out, 400 + i, length, packed, 0, 0, 0, 0);
	}

	/* Traversal that has to arrive at the same place both ways. */
	static const char *const equivalent[] = {
	    "/./etc/hosts", "/etc/./hosts", "/usr/../etc/hosts",
	    "/usr/lib/../../etc/hosts", "//etc//hosts", "/lib/libthing.so",
	    "/usr/etc-link/hosts", 0,
	};
	for (long i = 0; equivalent[i] != 0; i++) {
		join(path, prefix, equivalent[i]);
		report_stat(out, 500 + i, SYS_stat, path);
	}

	/* Existence, executability and readability, and a garbage mode — which
	 * is EINVAL on Linux rather than a cheerful yes.
	 *
	 * W_OK is deliberately absent: the oracle's tree is an ordinary
	 * writable directory under /tmp, so the real kernel answers 0 where a
	 * read-only filesystem answers EROFS. That is a difference between the
	 * two *mounts*, not between the two kernels, and comparing it here
	 * would be comparing the fixture. The EROFS answer is asserted in
	 * `kisal/tests/filesystem.rs` against ground truth taken from a real
	 * read-only mount. */
	static const char *const probed[] = {"/script", "/etc/hosts", "/etc",
	                                     "/nothing", 0};
	for (long i = 0; probed[i] != 0; i++) {
		join(path, prefix, probed[i]);
		emit(out, 600 + i, sys3(SYS_access, (long)path, 0, 0),
		     sys3(SYS_access, (long)path, 1, 0),
		     sys3(SYS_access, (long)path, 4, 0),
		     sys3(SYS_access, (long)path, 8, 0), 0, 0);
	}

	/* Symlink chains, at and over the limit. Linux counts forty total
	 * traversals per resolution, not nesting depth, so a flat chain is
	 * what tells the two counts apart. */
	static const char *const chains[] = {
	    "/chain/link0",  "/chain/link38", "/chain/link39",
	    "/chain/link40", "/chain/link44", "/loop-a",
	    "/loop-b",       0,
	};
	for (long i = 0; chains[i] != 0; i++) {
		join(path, prefix, chains[i]);
		report_stat(out, 620 + i, SYS_stat, path);
	}

	/* Directory entries: names and types, in the order the kernel gives
	 * them. Sorted on both sides by the harness, because a real filesystem
	 * has no ordering guarantee and the image does.
	 *
	 * Two directories, not one. `/etc` holds a directory, a symlink and a
	 * fifo beside its regular files, and the root holds directories and
	 * symlinks — a listing of regular files only cannot tell a `d_type`
	 * read out of the index from one hardcoded to DT_REG. */
	static const char *const listed[] = {"/etc", "/", 0};
	for (long which = 0; listed[which] != 0; which++) {
		join(path, prefix, listed[which]);
		fd = sys3(SYS_open, (long)path, O_RDONLY | O_DIRECTORY, 0);
		if (fd < 0) {
			continue;
		}
		volatile char entries[4096];
		long written = sys3(SYS_getdents64, fd, (long)entries, 4096);
		emit(out, 700 + which * 10, written > 0, 0, 0, 0, 0, 0);
		long at = 0;
		long index = 0;
		while (at < written) {
			long length = (unsigned char)entries[at + 16] |
			              ((unsigned char)entries[at + 17] << 8);
			long type = (unsigned char)entries[at + 18];
			long packed = 0;
			for (long j = 0; j < 8; j++) {
				unsigned char byte = (unsigned char)entries[at + 19 + j];
				packed = (packed << 8) | byte;
				if (byte == 0) {
					break;
				}
			}
			emit(out, 701 + which * 10, index, type, packed, 0, 0, 0);
			at += length;
			index++;
		}
		emit(out, 702 + which * 10,
		     sys3(SYS_getdents64, fd, (long)entries, 4096), 0, 0, 0, 0, 0);
		sys3(SYS_close, fd, 0, 0);
	}

	/* `statx`, field by field. The mask is deliberately not compared: the
	 * real kernel advertises STATX_BTIME and STATX_MNT_ID, and an image
	 * built from a tar archive has neither a birth time nor a second
	 * mount to identify. Everything it does answer must agree. */
	static const char *const measured[] = {"/etc/hosts", "/script", "/etc",
	                                       "/lib", 0};
	for (long i = 0; measured[i] != 0; i++) {
		volatile char raw[256];
		join(path, prefix, measured[i]);
		long result = sys5(SYS_statx, AT_FDCWD, (long)path, 0, 0x7ff,
		                   (long)raw);
		if (result < 0) {
			emit(out, 800 + i, result, 0, 0, 0, 0, 0);
			continue;
		}
		const long *stx = (const long *)raw;
		const int *word = (const int *)raw;
		emit(out, 800 + i, 0,
		     word[4],                       /* stx_nlink @16 */
		     ((long)word[7] & 0xffff) |     /* stx_mode  @28 */
		         ((long)word[5] << 32),     /* stx_uid   @20 */
		     stx[5],                        /* stx_size  @40 */
		     word[6],                       /* stx_gid   @24 */
		     stx[14]);                      /* stx_mtime sec @112 */
	}

	return 0;
}
