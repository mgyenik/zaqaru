/* A function whose last instruction is a call that never comes back.
 *
 * A compiler that knows a callee does not return emits nothing after the
 * call — no `ret`, no jump, nothing. The function's last byte is the last
 * byte of the call, and control simply does not continue. gcc does this for
 * every wrapper around `abort`, `exit` or `__stack_chk_fail`, and for every
 * cold fragment it splits out of a larger function; a static glibc is full
 * of five-byte functions that are one `call abort` and nothing else.
 *
 * Reading that as a fall-through asks where control goes next, and the
 * answer is nowhere.
 */

__attribute__((noreturn)) void stop(int status);

/* One call and nothing after it. */
__attribute__((noreturn)) void give_up(void) { stop(1); }

/* The same, reached conditionally, so the function has a real body and only
 * one of its paths ends this way. */
int checked(int value) {
	if (value < 0) {
		stop(2);
	}
	return value * 2;
}
