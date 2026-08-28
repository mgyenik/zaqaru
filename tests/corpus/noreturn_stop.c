/* The callee that does not return, in a translation unit of its own so the
 * compiler cannot see through it. */
__attribute__((noreturn)) void stop(int status);

__attribute__((noreturn)) void stop(int status) {
	for (;;) {
		(void)status;
	}
}
