/* What a process is told about itself, and the small syscalls a libc asks
 * before it does anything else.
 *
 * These are exactly what a static glibc `hello` issues between `execve` and
 * its first `write`, in this order: `set_tid_address`, `set_robust_list`,
 * `rseq`, `prlimit64`, `getrandom`. None of them does any work; all of them
 * have to answer, because glibc treats a surprise here as fatal.
 *
 * Several answers are deliberately *not* the host's. The process id is one,
 * because a container's entry process is the first in its own namespace; the
 * stack limit is another, because it has to be the stack the kernel actually
 * handed over. So the differential compares only what must agree, and the
 * container test beside it checks the rest.
 *
 * No libc: raw `syscall` instructions, so the same source runs natively and
 * transpiled.
 */

typedef long ssize;

#define SYS_set_tid_address 218
#define SYS_set_robust_list 273
#define SYS_prlimit64 302
#define SYS_getrandom 318
#define SYS_rseq 334

#define RLIMIT_STACK 3
#define ROBUST_LIST_HEAD_SIZE 24

static ssize sys3(long number, long a, long b, long c) {
	ssize result;
	__asm__ volatile("syscall"
	                 : "=a"(result)
	                 : "a"(number), "D"(a), "S"(b), "d"(c)
	                 : "rcx", "r11", "memory");
	return result;
}

static ssize sys4(long number, long a, long b, long c, long d) {
	register long r10 __asm__("r10") = d;
	ssize result;
	__asm__ volatile("syscall"
	                 : "=a"(result)
	                 : "a"(number), "D"(a), "S"(b), "d"(c), "r"(r10)
	                 : "rcx", "r11", "memory");
	return result;
}

/* `set_robust_list` accepts its own structure's size and refuses any other,
 * because a caller passing a different one was built against a different
 * kernel. Returns the two answers packed. */
long guest_robust_list(void) {
	char head[64];
	long good = sys3(SYS_set_robust_list, (long)head, ROBUST_LIST_HEAD_SIZE, 0);
	long bad = sys3(SYS_set_robust_list, (long)head, ROBUST_LIST_HEAD_SIZE + 1, 0);
	return (good & 0xffff) | ((bad & 0xffff) << 16);
}

/* Restartable sequences are refused for real, and glibc takes the refusal by
 * never using the feature. A registration that appeared to succeed would
 * leave it expecting the kernel to restart its critical sections. */
long guest_rseq(void) {
	char area[32];
	return sys4(SYS_rseq, (long)area, 32, 0, 0x53053053);
}

/* Reading a limit with nowhere to put it is how a caller asks whether the
 * resource exists at all. */
long guest_limit_probe(long resource) {
	return sys4(SYS_prlimit64, 0, resource, 0, 0);
}

/* Setting one is refused rather than ignored. */
long guest_limit_set(void) {
	unsigned long limit[2] = {1024, 1024};
	return sys4(SYS_prlimit64, 0, RLIMIT_STACK, (long)limit, 0);
}

/* The argument checks, which are the part that must match Linux exactly. */
long guest_random_refusals(void) {
	char buffer[8];
	long empty = sys3(SYS_getrandom, (long)buffer, 0, 0);
	long faulted = sys3(SYS_getrandom, 0, 8, 0);
	long flagged = sys3(SYS_getrandom, (long)buffer, 8, 0xff);
	return (empty & 0xff) | ((faulted & 0xff) << 8) | ((flagged & 0xff) << 16);
}

/* And the count, which is always everything asked for: the generator is
 * always ready, which is the only reason a real kernel would give less. */
long guest_random_count(long length) {
	char buffer[512];
	if (length > (long)sizeof buffer) {
		length = sizeof buffer;
	}
	return sys3(SYS_getrandom, (long)buffer, length, 1 /* GRND_NONBLOCK */);
}

/* Eight bytes of the stream itself, so that two runs can be compared on what
 * they drew rather than on how much. */
long guest_random_word(void) {
	long word = 0;
	long got = sys3(SYS_getrandom, (long)&word, sizeof word, 0);
	if (got != (long)sizeof word) {
		return got;
	}
	return word;
}
