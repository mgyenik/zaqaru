/* A program that calls a function nobody could translate.
 *
 * The point of a trapping body is what happens when something reaches it,
 * and only a program that reaches one can check that. This one writes a
 * line first, so a failure that stops it earlier is distinguishable from
 * the failure it is supposed to produce.
 */
	.text
	.globl _start
	.type _start,@function
_start:
	mov $1, %eax            /* write */
	mov $1, %edi
	mov $message, %esi
	mov $8, %edx
	syscall
	call halted
	mov $231, %eax          /* exit_group, never reached */
	xor %edi, %edi
	syscall
	ret
	.size _start, .-_start

	.globl halted
	.type halted,@function
halted:
	wrmsr
	ret
	.size halted, .-halted

	.section .rodata
message:
	.ascii "running\n"
