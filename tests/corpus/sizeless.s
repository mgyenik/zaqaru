/* Functions a compiler did not describe.
 *
 * Hand-written assembly routinely omits `.size`, and a linker does not add
 * one. Nothing here has a stated extent and nothing has an unwind entry
 * either, so where one function ends is knowable only from where the next
 * one starts — which is the last witness the reader has, and the one every
 * real binary needs for `crtbegin.o`'s stubs.
 */
	.text
	.globl entry
	.type entry,@function
entry:
	call helper
	mov $231, %eax          /* exit_group */
	mov %rbx, %rdi
	syscall
	/* `exit_group` does not return, but nothing in the instruction
	   stream says so — a function still has to end somewhere. */
	ret

	.globl helper
	.type helper,@function
helper:
	xor %ebx, %ebx
	ret
