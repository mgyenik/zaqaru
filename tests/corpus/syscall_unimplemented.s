/* A syscall kisal has no row for. Natively it is an ordinary `getpid`; under
 * kisal it is the loud-error policy, which exists to be tested rather than
 * merely stated.
 *
 * long guest_getpid(void);
 */

	.text
	.globl	guest_getpid
	.type	guest_getpid, @function
guest_getpid:
	movq	$39, %rax
	syscall
	ret
	.size	guest_getpid, .-guest_getpid

/* Six distinct arguments through an unimplemented syscall, so that the
 * kernel's diagnostic reports what the seam actually marshalled.
 *
 * This is the only way the register wiring is observable from outside, and
 * it is the wiring most likely to be silently wrong: Linux takes its fourth
 * argument in `%r10`, not the `%rcx` the C convention would use, because the
 * `syscall` instruction overwrites `%rcx` with the return address before the
 * kernel ever sees it. A guest that only ever passes three arguments cannot
 * tell a correct seam from one that transposed them.
 *
 * long guest_six_arguments(void);
 */
	.globl	guest_six_arguments
	.type	guest_six_arguments, @function
guest_six_arguments:
	movq	$39, %rax
	movq	$11, %rdi
	movq	$22, %rsi
	movq	$33, %rdx
	movq	$44, %r10
	movq	$55, %r8
	movq	$66, %r9
	movq	$-1, %rcx
	syscall
	ret
	.size	guest_six_arguments, .-guest_six_arguments

	/* An assembly source states this for itself: without it the linker
	 * assumes an executable stack and says so, and a warning from a host
	 * tool is a failure here. */
	.section .note.GNU-stack,"",@progbits
