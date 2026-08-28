/* A leaf that issues a syscall and leaves by tail jump.
 *
 * The shape exists because of what it breaks. The translator reserves stack
 * for a call site's return-address slot and for a `syscall`'s red zone, but
 * those reservations are the translation's own — `iced` reports `syscall` as
 * writing `%rcx` and `%r11` and nothing else, and a tail `jmp` as writing
 * nothing. So a function that *reads* `%rsp` and never writes it through a
 * `push`, `pop`, `call` or `ret` is exactly the function whose promoted
 * `%rsp` would never be published, leaving the guest's stack pointer shifted
 * for the rest of its life with nothing to say so.
 *
 * `syscall_red_zone.s` cannot catch it: that guest ends in `ret`, which does
 * put `%rsp` in the written set.
 *
 * long guest_leaf_tail(long descriptor, long length);
 */

	.text
	.globl	guest_leaf_tail
	.type	guest_leaf_tail, @function
guest_leaf_tail:
	/* Stored to memory rather than kept in a register, and read back by
	 * the tail-call target, because *reading* `%rsp` is all this function
	 * may do: a `push` or a `ret` would put `%rsp` in the written set and
	 * hide the bug. A store writes memory, not `%rsp`. */
	movq	%rsp, leaf_before(%rip)
	movq	%rsi, %rdx
	leaq	leaf_message(%rip), %rsi
	movq	$1, %rax                    /* SYS_write */
	syscall
	movq	%rsp, leaf_after(%rip)
	jmp	guest_leaf_report            /* leave by tail jump, not `ret` */
	.size	guest_leaf_tail, .-guest_leaf_tail

/* How far `%rsp` moved across the syscall. Zero for a balanced translation.
 *
 * Measured across the syscall alone rather than across the whole function,
 * because a tail jump is modelled as a call and deliberately leaves the
 * target's `%rsp` eight bytes lower than a real `jmp` would — a separate,
 * self-consistent property of the translation that would otherwise be mixed
 * into this number. */
	.globl	guest_leaf_report
	.type	guest_leaf_report, @function
guest_leaf_report:
	movq	leaf_after(%rip), %rax
	subq	leaf_before(%rip), %rax
	ret
	.size	guest_leaf_report, .-guest_leaf_report

	.section .bss
	.type	leaf_before, @object
	.size	leaf_before, 8
	.align	8
leaf_before:
	.zero	8
	.type	leaf_after, @object
	.size	leaf_after, 8
	.align	8
leaf_after:
	.zero	8

	.section .rodata
	.type	leaf_message, @object
	.size	leaf_message, 6
leaf_message:
	.ascii	"leaf\n"

	.section .note.GNU-stack,"",@progbits
