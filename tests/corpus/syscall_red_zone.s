/* The red zone across a syscall.
 *
 * The 128 bytes below `%rsp` belong to a leaf function, and the Linux kernel
 * preserves them across `syscall` — unlike a `call`, where the SysV ABI lets
 * the callee destroy them. Compilers rely on this: gcc at -O2 puts a leaf's
 * locals there and reads them back after an inline `syscall`, without ever
 * moving `%rsp`.
 *
 * This guest fills all sixteen quadwords with distinct sentinels, issues a
 * write, and returns how many survived. The real kernel returns 16. Anything
 * less is silent corruption of whatever the guest had there.
 *
 * long guest_red_zone(long descriptor, long length);
 */

	.text
	.globl	guest_red_zone
	.type	guest_red_zone, @function
guest_red_zone:
	movq	$1, %rcx                        /* sentinel, 1..16 */
	movq	$-8, %rdx                       /* offset from %rsp */
.Lfill:
	movq	%rcx, (%rsp,%rdx,1)
	subq	$8, %rdx
	addq	$1, %rcx
	cmpq	$17, %rcx
	jne	.Lfill

	movq	%rsi, %rdx                      /* length */
	leaq	red_zone_message(%rip), %rsi
	movq	$1, %rax                        /* SYS_write */
	syscall

	xorq	%rax, %rax                      /* survivors */
	movq	$1, %rcx
	movq	$-8, %rdx
.Lcheck:
	cmpq	%rcx, (%rsp,%rdx,1)
	jne	.Lskip
	addq	$1, %rax
.Lskip:
	subq	$8, %rdx
	addq	$1, %rcx
	cmpq	$17, %rcx
	jne	.Lcheck
	ret
	.size	guest_red_zone, .-guest_red_zone

	.section .rodata
	.type	red_zone_message, @object
	.size	red_zone_message, 17
red_zone_message:
	.ascii	"hello, courtyard\n"

	.section .note.GNU-stack,"",@progbits
