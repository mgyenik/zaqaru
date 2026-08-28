/* The M2 guest: thread-local storage, exercised through the mechanism
 * rather than through a compiler's TLS relocations.
 *
 * A C program with `__thread` variables would be the obvious corpus source
 * and would be the wrong one: it drags in `.tdata` and `TPOFF` relocations
 * that the relocatable-object pipeline has no reason to learn, because the
 * container pipeline never sees them — a linked static binary's TLS setup is
 * musl's own code operating on concrete addresses. So this exercises what
 * actually has to work: `arch_prctl` moving the base, and `%fs`-prefixed
 * operands reading and writing through it.
 *
 * It is *self-restoring*, and that is not tidiness. The native oracle runs
 * in-process, so leaving `%fs` pointing at a scratch buffer would destroy the
 * test harness's own thread-local storage out from under it. Nothing between
 * the two `ARCH_SET_FS` calls touches TLS, which is why this has to be
 * assembly and not C.
 *
 * long guest_segment_base(long value);
 *
 * Sets the base to `scratch`, writes `value` through `%fs:8`, reads it back,
 * writes the successor through `%fs:16` with a scaled index, reads *that*
 * back, checks `ARCH_GET_FS` reports the base it was given, restores the
 * incumbent base and returns the value read — or -1 if the base read back
 * did not match, which is the one failure the return value alone could not
 * distinguish from a correct small answer.
 */

	.text
	.globl	guest_segment_base
	.type	guest_segment_base, @function
guest_segment_base:
	pushq	%rbx
	pushq	%r12
	subq	$16, %rsp

	leaq	segment_scratch(%rip), %r12 /* its own block, not the caller's */
	movq	%rdi, %rbx                  /* value   */

	/* Save the incumbent base into the frame. */
	movq	$158, %rax                  /* SYS_arch_prctl */
	movq	$0x1003, %rdi               /* ARCH_GET_FS */
	movq	%rsp, %rsi
	syscall

	/* Point %fs at the scratch buffer. */
	movq	$158, %rax
	movq	$0x1002, %rdi               /* ARCH_SET_FS */
	movq	%r12, %rsi
	syscall

	/* `lea` ignores a segment override: there is no access for a segment
	 * to apply to, so the hardware computes the address without the base.
	 * Hand-encoded as `lea %fs:0x10, %r11` because gas warns about the
	 * prefix ("ineffectual") while still emitting it, and a warning from a
	 * host tool fails the build here. */
	.byte	0x64, 0x4c, 0x8d, 0x1c, 0x25, 0x10, 0x00, 0x00, 0x00
	cmpq	$0x10, %r11
	jne	.Lsegment_applied_to_lea

	/* A store and a load through the base, at a displacement. */
	movq	%rbx, %fs:8
	movq	%fs:8, %rcx
	addq	$1, %rcx

	/* And again with a scaled index, so the base is added to a whole
	 * effective address rather than to a bare displacement. */
	movq	$2, %rdx
	movq	%rcx, %fs:(,%rdx,8)
	movq	%fs:(,%rdx,8), %r9

	/* Ask the kernel what the base is now; it must be what we set. */
	movq	$158, %rax
	movq	$0x1003, %rdi
	leaq	8(%rsp), %rsi
	syscall
	movq	8(%rsp), %r8

	/* Restore the incumbent base before anything else can notice. */
	movq	$158, %rax
	movq	$0x1002, %rdi
	movq	(%rsp), %rsi
	syscall

	movq	$-1, %rax
	cmpq	%r12, %r8
	jne	.Ldone
	movq	%r9, %rax
.Ldone:
	addq	$16, %rsp
	popq	%r12
	popq	%rbx
	ret

	/* Reached only if `lea` picked up the segment base. Distinct from the
	 * -1 above so a failure says which rule was broken. */
.Lsegment_applied_to_lea:
	movq	$158, %rax
	movq	$0x1002, %rdi
	movq	(%rsp), %rsi
	syscall
	movq	$-2, %rax
	addq	$16, %rsp
	popq	%r12
	popq	%rbx
	ret
	.size	guest_segment_base, .-guest_segment_base

	/* The thread-local block this points `%fs` at. Owned here rather than
	 * supplied by the caller: a buffer the *host* places would have to
	 * outlive the guest's syscalls, and nothing on this boundary does. */
	.section .bss
	.type	segment_scratch, @object
	.size	segment_scratch, 64
	.align	16
segment_scratch:
	.zero	64

	.section .note.GNU-stack,"",@progbits
