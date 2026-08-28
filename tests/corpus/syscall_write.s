/* The M1 seam guest: `write(2)` and nothing else.
 *
 * Hand-written because it must run *natively* as well as transpiled — the
 * same `.s`, assembled for x86-64, writing the same bytes to the same
 * descriptor, is the oracle the transpiled run is compared against. A C
 * program could not be the oracle here: reaching `syscall` from C means a
 * libc, and a libc means `%fs` before M2 exists.
 *
 * The descriptor is a parameter rather than a hard-coded 1 so that the
 * native side can be handed a file to write into and read back, while the
 * wasm side is handed 1 and reaches the console mount. What is compared is
 * what the syscall did: the bytes delivered, and the value returned.
 *
 * long guest_write(long descriptor, long length);
 */

	.text
	.globl	guest_write
	.type	guest_write, @function
guest_write:
	movq	%rsi, %rdx
	leaq	guest_message(%rip), %rsi
	movq	$1, %rax
	/* Sentinels in the two registers the `syscall` instruction destroys, so
	 * that finding zero in them afterwards means the seam cleared them
	 * rather than that nobody ever wrote them. */
	movq	$-1, %rcx
	movq	$-1, %r11
	syscall
	ret
	.size	guest_write, .-guest_write

/* Flags set before a syscall must be visible to the kernel.
 *
 * `syscall` is not a `call`: Linux restores `RFLAGS` from `r11`, so a guest
 * may branch on a flag it set beforehand — and more decisively, a thread that
 * blocks here has its register file snapshotted out of the globals, so a flag
 * left behind in a local would be saved stale.
 *
 * Sets SF=1, CF=1, ZF=0, OF=0 — a pattern no zeroed register file produces —
 * and then issues the write.
 *
 * long guest_flags_before_syscall(long descriptor, long length);
 */
	.text
	.globl	guest_flags_before_syscall
	.type	guest_flags_before_syscall, @function
guest_flags_before_syscall:
	movq	%rsi, %rdx
	leaq	guest_message(%rip), %rsi
	movq	$1, %rax
	xorq	%r8, %r8
	cmpq	$1, %r8
	syscall
	ret
	.size	guest_flags_before_syscall, .-guest_flags_before_syscall

	.section .rodata
	.type	guest_message, @object
	.size	guest_message, 17
guest_message:
	.ascii	"hello, courtyard\n"

	/* An assembly source states this for itself: without it the linker
	 * assumes an executable stack and says so, and a warning from a host
	 * tool is a failure here. */
	.section .note.GNU-stack,"",@progbits
