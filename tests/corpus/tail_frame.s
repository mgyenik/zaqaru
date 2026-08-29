/* A tail jump into a function that reads the jumper's frame.
 *
 * This is the hot/cold split as gcc writes it: the hot half allocates the
 * frame and stores its locals, jumps to the cold fragment with the frame
 * still standing, and the fragment reads those locals and tears the frame
 * down itself. So `%rsp` at the callee's first instruction has to be
 * exactly what the jumper left — not eight bytes below it.
 *
 * A translation that models a tail jump as a call has to put a
 * return-address slot somewhere, and the machine has no room for one: on
 * the hardware a `jmp` moves no stack pointer at all. Reserving one anyway
 * shifts every stack-relative operand the callee has by eight bytes, which
 * is invisible in any fixture whose callee only touches registers — and
 * every fixture here did, until this one.
 *
 * Found in a static glibc `main`: split at its canary epilogue, because the
 * cold path rejoins it. The second piece read `0x48(%rsp)` expecting the
 * canary and got the eight bytes below it, so `__stack_chk_fail` fired on a
 * program that had overflowed nothing.
 */
	.text

	.globl	reads_the_jumpers_frame
	.type	reads_the_jumpers_frame, @function
reads_the_jumpers_frame:
	movq	0x10(%rsp), %rax
	addq	$0x28, %rsp
	ret
	.size	reads_the_jumpers_frame, .-reads_the_jumpers_frame

	.globl	jumps_with_a_frame
	.type	jumps_with_a_frame, @function
jumps_with_a_frame:
	subq	$0x28, %rsp
	movq	%rdi, 0x10(%rsp)
	jmp	reads_the_jumpers_frame
	.size	jumps_with_a_frame, .-jumps_with_a_frame

	.section	.note.GNU-stack,"",@progbits
