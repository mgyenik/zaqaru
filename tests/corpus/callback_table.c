/* A callback registered by taking its address, and reached no other way.
 *
 * `handler` is static, so nothing outside names it; nothing calls it
 * directly; and at `-O1` and above the compiler does not inline it through
 * the pointer. What the binary contains is one instruction that *computes*
 * its address — a program-counter-relative `lea`, or a `mov` of an
 * immediate where the code is not position-independent — and an indirect
 * call through the register.
 *
 * Stripped and built without unwind tables, that instruction is the only
 * thing in the file that says `handler` exists. It is the shape a callback,
 * a vtable slot and a dispatch table all have, and the reason discovery
 * harvests operands and not only branch targets.
 */

static int handler(int value) { return value * 7 + 3; }

static int (*chosen)(int) = 0;

int through_a_pointer(int value) {
	/* The compiler cannot fold this: `chosen` is written here and read
	   through a pointer it cannot prove constant across the store. */
	chosen = handler;
	int (*volatile call)(int) = chosen;
	return call(value);
}
