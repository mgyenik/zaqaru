/* The synthetic `/dev`, exercised from inside a container.
 *
 * The mount is built by the kernel at boot, in the module's own memory, and
 * attached over the directory the image provides. Natively that is a `Vec`
 * and a leak; inside the module it is the guest's allocator and the guest's
 * linear memory. This is what says the two agree.
 *
 * No libc: raw `syscall` instructions.
 */

typedef long ssize;

#define SYS_read 0
#define SYS_write 1
#define SYS_open 2
#define SYS_close 3
#define SYS_stat 4
#define SYS_lseek 8
#define SYS_ioctl 16

#define O_RDONLY 0
#define O_RDWR 2

static ssize sys3(long number, long a, long b, long c) {
	ssize result;
	__asm__ volatile("syscall"
	                 : "=a"(result)
	                 : "a"(number), "D"(a), "S"(b), "d"(c)
	                 : "rcx", "r11", "memory");
	return result;
}

static void emit(long out, long tag, long result, long a, long b) {
	long record[4];
	record[0] = tag;
	record[1] = result;
	record[2] = a;
	record[3] = b;
	sys3(SYS_write, out, (long)record, sizeof(record));
}

long guest_devices(long out) {
	volatile char buffer[128];
	long stat[18];

	/* The nodes are there, with the device numbers Linux gives them. */
	static const char *const nodes[] = {"/dev/null", "/dev/zero", "/dev/full",
	                                    "/dev/urandom", 0};
	for (long i = 0; nodes[i] != 0; i++) {
		long result = sys3(SYS_stat, (long)nodes[i], (long)stat, 0);
		emit(out, 100 + i, result, stat[3] & 0xffffffff /* st_mode */,
		     stat[5] /* st_rdev */);
	}

	/* `/dev/null` reads nothing and accepts everything. */
	long null = sys3(SYS_open, (long)"/dev/null", O_RDWR, 0);
	emit(out, 200, null >= 0, 0, 0);
	emit(out, 201, sys3(SYS_read, null, (long)buffer, 16), 0, 0);
	emit(out, 202, sys3(SYS_write, null, (long)buffer, 16), 0, 0);

	/* `/dev/zero` fills with zeros — across the kernel's own chunk, so a
	 * read larger than it is one call rather than a short one. */
	for (long i = 0; i < 128; i++) {
		buffer[i] = (char)0xaa;
	}
	long zero = sys3(SYS_open, (long)"/dev/zero", O_RDONLY, 0);
	long read = sys3(SYS_read, zero, (long)buffer, 128);
	long nonzero = 0;
	for (long i = 0; i < 128; i++) {
		if (buffer[i] != 0) {
			nonzero++;
		}
	}
	emit(out, 300, read, nonzero, 0);
	/* A device has no position: the seek answers zero and reads carry on. */
	emit(out, 301, sys3(SYS_lseek, zero, 100, 0), 0, 0);
	emit(out, 302, sys3(SYS_read, zero, (long)buffer, 8), 0, 0);

	/* `/dev/full` refuses every write. */
	long full = sys3(SYS_open, (long)"/dev/full", O_RDWR, 0);
	emit(out, 400, sys3(SYS_read, full, (long)buffer, 8), 0, 0);
	emit(out, 401, sys3(SYS_write, full, (long)buffer, 8), 0, 0);

	/* `/dev/urandom` gives bytes that are not all the same, and the stream
	 * advances rather than repeating. */
	long urandom = sys3(SYS_open, (long)"/dev/urandom", O_RDONLY, 0);
	long first = 0;
	long second = 0;
	sys3(SYS_read, urandom, (long)buffer, 8);
	for (long i = 0; i < 8; i++) {
		first = (first << 8) | (unsigned char)buffer[i];
	}
	sys3(SYS_read, urandom, (long)buffer, 8);
	for (long i = 0; i < 8; i++) {
		second = (second << 8) | (unsigned char)buffer[i];
	}
	emit(out, 500, first != 0, second != 0, first != second);
	/* The bytes themselves, so the host can check they are the seed's. */
	emit(out, 501, first, second, 0);

	/* Stdio is not a terminal. */
	emit(out, 600, sys3(SYS_ioctl, 1, 0x5401, (long)buffer), 0, 0);

	sys3(SYS_close, null, 0, 0);
	sys3(SYS_close, zero, 0, 0);
	sys3(SYS_close, full, 0, 0);
	sys3(SYS_close, urandom, 0, 0);
	return 0;
}
