/* The M4 differential guest: the writable layer, exercised from C.
 *
 * Same shape as the read differential — raw `syscall` instructions, a binary
 * record stream, a caller-supplied path prefix — so the same source runs
 * natively against a real directory and transpiled against the overlay.
 *
 * What is deliberately absent from the records: timestamps that nothing set
 * explicitly. A container has no clock of its own, so a file written here
 * carries a counter rather than a wall time, and comparing that against a
 * real filesystem's would be comparing the two clocks. Times that a caller
 * *sets* are compared exactly, because that is the case that matters — a
 * `.pyc` is stale or not by the number `utimensat` put there.
 */

typedef long ssize;

#define SYS_read 0
#define SYS_write 1
#define SYS_open 2
#define SYS_close 3
#define SYS_stat 4
#define SYS_lstat 6
#define SYS_lseek 8
#define SYS_pwrite64 18
#define SYS_rename 82
#define SYS_mkdir 83
#define SYS_rmdir 84
#define SYS_unlink 87
#define SYS_symlink 88
#define SYS_truncate 76
#define SYS_ftruncate 77
#define SYS_getdents64 217
#define SYS_utimensat 280
#define SYS_readlink 89

#define O_RDONLY 0
#define O_WRONLY 1
#define O_RDWR 2
#define O_CREAT 0100
#define O_EXCL 0200
#define O_TRUNC 01000
#define O_APPEND 02000
#define O_DIRECTORY 0200000
#define AT_FDCWD -100

static ssize sys3(long number, long a, long b, long c) {
	ssize result;
	__asm__ volatile("syscall"
	                 : "=a"(result)
	                 : "a"(number), "D"(a), "S"(b), "d"(c)
	                 : "rcx", "r11", "memory");
	return result;
}

static ssize sys4(long number, long a, long b, long c, long d) {
	ssize result;
	register long r10 __asm__("r10") = d;
	__asm__ volatile("syscall"
	                 : "=a"(result)
	                 : "a"(number), "D"(a), "S"(b), "d"(c), "r"(r10)
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

/* Mode, link count and size — everything about a file that both sides can
 * agree on. */
static void report_stat(long out, long tag, long number, volatile char *path) {
	long stat[18];
	long result = sys3(number, (long)path, (long)stat, 0);
	if (result < 0) {
		emit(out, tag, result, 0, 0, 0, 0, 0);
		return;
	}
	emit(out, tag, 0, stat[2] /* nlink */, stat[3] & 0xffffffff /* mode */,
	     stat[6] /* size */, 0, 0);
}

/* The first eight bytes of a file, and its length. */
static void report_contents(long out, long tag, volatile char *path) {
	long fd = sys3(SYS_open, (long)path, O_RDONLY, 0);
	if (fd < 0) {
		emit(out, tag, fd, 0, 0, 0, 0, 0);
		return;
	}
	volatile char buffer[64];
	for (long i = 0; i < 64; i++) {
		buffer[i] = 0;
	}
	long read = sys3(SYS_read, fd, (long)buffer, 64);
	long packed = 0;
	for (long i = 0; i < 8; i++) {
		packed = (packed << 8) | (unsigned char)buffer[i];
	}
	long tail = 0;
	for (long i = 8; i < 16; i++) {
		tail = (tail << 8) | (unsigned char)buffer[i];
	}
	emit(out, tag, read, packed, tail, 0, 0, 0);
	sys3(SYS_close, fd, 0, 0);
}

long guest_write(long out, const char *prefix) {
	volatile char path[512];
	volatile char other[512];
	static const char none[] = "";
	if (prefix == 0) {
		prefix = none;
	}

	/* Creating: a new file, then the same name again with O_EXCL. */
	join(path, prefix, "/made");
	long fd = sys3(SYS_open, (long)path, O_RDWR | O_CREAT, 0644);
	emit(out, 100, fd >= 0, 0, 0, 0, 0, 0);
	emit(out, 101, sys3(SYS_open, (long)path, O_RDWR | O_CREAT | O_EXCL, 0644), 0, 0, 0, 0, 0);
	report_stat(out, 102, SYS_stat, path);

	/* Writing, and reading back through a fresh open. */
	static const char message[] = "the writable layer\n";
	emit(out, 110, sys3(SYS_write, fd, (long)message, 19), 0, 0, 0, 0, 0);
	report_contents(out, 111, path);
	report_stat(out, 112, SYS_stat, path);

	/* The description's own offset advanced with the write. */
	emit(out, 113, sys3(SYS_lseek, fd, 0, 1 /* SEEK_CUR */), 0, 0, 0, 0, 0);

	/* Writing at an offset past the end fills the gap with zeros. */
	emit(out, 120, sys4(SYS_pwrite64, fd, (long)"X", 1, 24), 0, 0, 0, 0, 0);
	report_stat(out, 121, SYS_stat, path);
	report_contents(out, 122, path);

	/* Truncating, both ways. */
	emit(out, 130, sys3(SYS_ftruncate, fd, 4, 0), 0, 0, 0, 0, 0);
	report_stat(out, 131, SYS_stat, path);
	emit(out, 132, sys3(SYS_truncate, (long)path, 8, 0), 0, 0, 0, 0, 0);
	report_contents(out, 133, path);
	emit(out, 134, sys3(SYS_ftruncate, fd, -1, 0), 0, 0, 0, 0, 0);
	sys3(SYS_close, fd, 0, 0);

	/* Appending: a second descriptor writes at the end whatever its own
	 * offset says. */
	join(path, prefix, "/log");
	long first = sys3(SYS_open, (long)path, O_WRONLY | O_CREAT, 0644);
	sys3(SYS_write, first, (long)"one\n", 4);
	long second = sys3(SYS_open, (long)path, O_WRONLY | O_APPEND, 0);
	emit(out, 140, sys3(SYS_write, second, (long)"two\n", 4), 0, 0, 0, 0, 0);
	sys3(SYS_lseek, second, 0, 0 /* SEEK_SET */);
	emit(out, 141, sys3(SYS_write, second, (long)"three\n", 6), 0, 0, 0, 0, 0);
	report_contents(out, 142, path);
	sys3(SYS_close, first, 0, 0);
	sys3(SYS_close, second, 0, 0);

	/* O_TRUNC empties a file before the descriptor exists. */
	long emptied = sys3(SYS_open, (long)path, O_WRONLY | O_TRUNC, 0);
	emit(out, 150, emptied >= 0, 0, 0, 0, 0, 0);
	report_stat(out, 151, SYS_stat, path);
	sys3(SYS_close, emptied, 0, 0);

	/* Changing a file that came from the image, which copies it up. */
	join(path, prefix, "/etc/hosts");
	long existing = sys3(SYS_open, (long)path, O_WRONLY, 0);
	emit(out, 200, existing >= 0, 0, 0, 0, 0, 0);
	emit(out, 201, sys3(SYS_write, existing, (long)"CHANGED", 7), 0, 0, 0, 0, 0);
	sys3(SYS_close, existing, 0, 0);
	report_contents(out, 202, path);
	report_stat(out, 203, SYS_stat, path);

	/* Deleting one. */
	join(path, prefix, "/etc/hostname");
	emit(out, 210, sys3(SYS_unlink, (long)path, 0, 0), 0, 0, 0, 0, 0);
	report_stat(out, 211, SYS_stat, path);
	emit(out, 212, sys3(SYS_unlink, (long)path, 0, 0), 0, 0, 0, 0, 0);
	emit(out, 213, sys3(SYS_open, (long)path, O_RDONLY, 0), 0, 0, 0, 0, 0);
	/* And creating it again, which is a new empty file rather than the old
	 * one coming back. */
	long recreated = sys3(SYS_open, (long)path, O_RDWR | O_CREAT, 0644);
	emit(out, 214, recreated >= 0, 0, 0, 0, 0, 0);
	report_stat(out, 215, SYS_stat, path);
	sys3(SYS_close, recreated, 0, 0);

	/* Directories. */
	join(path, prefix, "/directory");
	emit(out, 300, sys3(SYS_mkdir, (long)path, 0755, 0), 0, 0, 0, 0, 0);
	emit(out, 301, sys3(SYS_mkdir, (long)path, 0755, 0), 0, 0, 0, 0, 0);
	report_stat(out, 302, SYS_stat, path);
	join(other, prefix, "/directory/inside");
	long inside = sys3(SYS_open, (long)other, O_WRONLY | O_CREAT, 0644);
	emit(out, 303, inside >= 0, 0, 0, 0, 0, 0);
	sys3(SYS_close, inside, 0, 0);
	emit(out, 304, sys3(SYS_rmdir, (long)path, 0, 0), 0, 0, 0, 0, 0);
	emit(out, 305, sys3(SYS_unlink, (long)path, 0, 0), 0, 0, 0, 0, 0);
	emit(out, 306, sys3(SYS_rmdir, (long)other, 0, 0), 0, 0, 0, 0, 0);
	emit(out, 307, sys3(SYS_unlink, (long)other, 0, 0), 0, 0, 0, 0, 0);
	emit(out, 308, sys3(SYS_rmdir, (long)path, 0, 0), 0, 0, 0, 0, 0);
	report_stat(out, 309, SYS_stat, path);

	/* Symlinks. */
	join(path, prefix, "/made-link");
	emit(out, 400, sys3(SYS_symlink, (long)"made", (long)path, 0), 0, 0, 0, 0, 0);
	report_stat(out, 401, SYS_lstat, path);
	report_stat(out, 402, SYS_stat, path);
	{
		volatile char target[64];
		for (long i = 0; i < 64; i++) {
			target[i] = 0;
		}
		long length = sys3(SYS_readlink, (long)path, (long)target, 64);
		long packed = 0;
		for (long i = 0; i < 8; i++) {
			packed = (packed << 8) | (unsigned char)target[i];
		}
		emit(out, 403, length, packed, 0, 0, 0, 0);
	}
	emit(out, 404, sys3(SYS_symlink, (long)"made", (long)path, 0), 0, 0, 0, 0, 0);

	/* Renaming. */
	join(path, prefix, "/made");
	join(other, prefix, "/moved");
	emit(out, 500, sys3(SYS_rename, (long)path, (long)other, 0), 0, 0, 0, 0, 0);
	report_stat(out, 501, SYS_stat, path);
	report_contents(out, 502, other);
	/* Onto an existing name, which it replaces. */
	join(path, prefix, "/log");
	emit(out, 503, sys3(SYS_rename, (long)other, (long)path, 0), 0, 0, 0, 0, 0);
	report_stat(out, 504, SYS_stat, path);
	/* A directory over a file, and a file over a directory. */
	join(path, prefix, "/box");
	sys3(SYS_mkdir, (long)path, 0755, 0);
	join(other, prefix, "/log");
	emit(out, 505, sys3(SYS_rename, (long)path, (long)other, 0), 0, 0, 0, 0, 0);
	emit(out, 506, sys3(SYS_rename, (long)other, (long)path, 0), 0, 0, 0, 0, 0);
	/* And a name that is not there. */
	join(other, prefix, "/absent");
	emit(out, 507, sys3(SYS_rename, (long)other, (long)path, 0), 0, 0, 0, 0, 0);

	/* Timestamps, set explicitly and read back: the `.pyc` question. */
	join(path, prefix, "/source.py");
	long source = sys3(SYS_open, (long)path, O_WRONLY | O_CREAT, 0644);
	sys3(SYS_close, source, 0, 0);
	{
		long times[4];
		times[0] = 1700000000; /* atime seconds  */
		times[1] = 0;
		times[2] = 1600000000; /* mtime seconds  */
		times[3] = 123456789;  /* mtime nanoseconds */
		emit(out, 600, sys4(SYS_utimensat, AT_FDCWD, (long)path, (long)times, 0),
		     0, 0, 0, 0, 0);
		long stat[18];
		long result = sys3(SYS_stat, (long)path, (long)stat, 0);
		emit(out, 601, result, stat[11] /* mtime seconds */,
		     stat[12] /* mtime nanoseconds */, 0, 0, 0);

		/* UTIME_OMIT leaves it where it was. */
		times[3] = (1L << 30) - 2;
		times[1] = (1L << 30) - 2;
		emit(out, 602, sys4(SYS_utimensat, AT_FDCWD, (long)path, (long)times, 0),
		     0, 0, 0, 0, 0);
		result = sys3(SYS_stat, (long)path, (long)stat, 0);
		emit(out, 603, result, stat[11], stat[12], 0, 0, 0);

		/* A nanosecond field that is neither real nor special. */
		times[3] = 2000000000;
		emit(out, 604, sys4(SYS_utimensat, AT_FDCWD, (long)path, (long)times, 0),
		     0, 0, 0, 0, 0);
	}

	/* The merged listing: names from the image, names created here, and the
	 * ones deleted in between. */
	join(path, prefix, "/etc");
	long directory = sys3(SYS_open, (long)path, O_RDONLY | O_DIRECTORY, 0);
	if (directory >= 0) {
		volatile char entries[2048];
		long written = sys3(SYS_getdents64, directory, (long)entries, 2048);
		long at = 0;
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
			emit(out, 700, 0, type, packed, 0, 0, 0);
			at += length;
		}
		sys3(SYS_close, directory, 0, 0);
	}

	return 0;
}
