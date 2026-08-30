/* A `switch` one of whose arms gcc moved into the cold body.
 *
 * The other half of the same story `split_switch.s` tells. A dispatch table
 * is told apart from an array of function pointers by where its entries
 * land: a function pointer names a function's *start*, a table entry names a
 * block inside the body that dispatches. That test was written as "the first
 * two entries", and a real binary says the arms of one dispatch land in two
 * bodies — because `gcc` splits a function's cold blocks out into a body of
 * their own, and which arms end up there is a property of the numbering
 * rather than of the table.
 *
 * CPython is the case: `opcode_targets[]` is 256 entries, 181 inside
 * `_PyEval_EvalFrameDefault` and 75 inside its cold twin — and **entry zero
 * is one of the 75**. Asking about the first two rejected the hottest
 * dispatch in the milestone target, which then translated as an indirect
 * transfer and missed in the exec map the first time a bytecode ran.
 *
 * Here entry zero names a block inside `cold_fragment`, and entries one and
 * two name blocks inside `dispatch`. Recognising the table needs the scan to
 * look past the first entry; running the program then needs the front-end
 * fixpoint to cut `cold_fragment` at the arm, since nothing else begins
 * there.
 *
 * Each answer is a distinct bit, so the sum says which arms were reached.
 */

	.text

/* long dispatch(long selector) */
	.globl	dispatch
	.type	dispatch, @function
dispatch:
	cmpq	$3, %rdi
	ja	.Ldefault
	jmp	*cold_table(,%rdi,8)
.Larm1:
	movq	$2, %rax
	ret
.Larm2:
	movq	$4, %rax
	ret
.Larm3:
	movq	$8, %rax
	ret
.Ldefault:
	movq	$16, %rax
	ret
	.size	dispatch, .-dispatch

/* The cold body. Its own first instruction is reached by nothing in this
 * program, which is how a cold fragment sits: the hot code jumps to each of
 * its interior labels separately and to its start not at all. */
	.globl	cold_fragment
	.type	cold_fragment, @function
cold_fragment:
	movq	$64, %rax
	ret
.Larm0:
	movq	$1, %rax
	ret
	.size	cold_fragment, .-cold_fragment

	.globl	_start
	.type	_start, @function
_start:
	xorl	%ebx, %ebx		/* the running sum */
	xorl	%r12d, %r12d		/* the selector */
.Lloop:
	movq	%r12, %rdi
	call	dispatch
	movb	%al, results(,%r12,1)
	addq	%rax, %rbx
	incq	%r12
	cmpq	$5, %r12
	jb	.Lloop

	movl	$1, %eax		/* write(1, results, 5) */
	movl	$1, %edi
	movl	$results, %esi
	movl	$5, %edx
	syscall

	movl	$231, %eax		/* exit_group(sum) */
	movq	%rbx, %rdi
	syscall
	.size	_start, .-_start

	.section .rodata
	.align	8
cold_table:
	.quad	.Larm0
	.quad	.Larm1
	.quad	.Larm2
	.quad	.Larm3

	.bss
	.align	8
results:
	.zero	8

	.section .note.GNU-stack,"",@progbits
