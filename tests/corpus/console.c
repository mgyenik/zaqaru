/* Standard input, read through the seam.
 *
 * This is the only path in the system that crosses the `ll-store` boundary
 * *downward* — every filesystem row answers from the baked image, and the
 * console's writes go the other way. Without a guest that reads, the runner's
 * `ll_read` closure, its return-area decoding and its `place` into guest
 * memory are code no test executes.
 *
 * No libc: raw `syscall` instructions, so the same source runs natively and
 * transpiled.
 */

typedef long ssize;

#define SYS_read 0
#define SYS_write 1
#define SYS_lseek 8

static ssize sys3(long number, long a, long b, long c) {
	ssize result;
	__asm__ volatile("syscall"
	                 : "=a"(result)
	                 : "a"(number), "D"(a), "S"(b), "d"(c)
	                 : "rcx", "r11", "memory");
	return result;
}

long guest_console(long out) {
	volatile char buffer[64];
	for (long i = 0; i < 64; i++) {
		buffer[i] = 0;
	}

	long first = sys3(SYS_read, 0, (long)buffer, 8);
	sys3(SYS_write, out, (long)buffer, first < 0 ? 0 : first);

	/* The offset advanced, so the rest arrives on the next read rather
	 * than the same bytes again — a stream that never ends is one every
	 * reader loops on forever. */
	long second = sys3(SYS_read, 0, (long)buffer, 64);
	sys3(SYS_write, out, (long)buffer, second < 0 ? 0 : second);

	/* And then end of input, reported as zero. */
	long third = sys3(SYS_read, 0, (long)buffer, 64);

	long report[4];
	report[0] = first;
	report[1] = second;
	report[2] = third;
	/* A console stream is not seekable: ESPIPE, all the way through the
	 * seam rather than only in a native unit test. */
	report[3] = sys3(SYS_lseek, 0, 0, 1);
	sys3(SYS_write, out, (long)report, sizeof(report));
	return 0;
}
