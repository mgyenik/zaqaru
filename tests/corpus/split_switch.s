/* A `switch` whose arms end up in a sibling piece.
 *
 * This is the shape that blocked CPython, reduced to something a test can
 * reason about. `_Py_HashBytes` is siphash and its tail is
 * `switch (len & 7)`; the body is cut at a direct branch into its middle,
 * and two of the eight arms then lie past the cut, in a piece that begins
 * somewhere else. A `br_table` cannot branch into another function, so the
 * structurer emits such an arm as a tail call — and a tail call needs a
 * function to begin exactly at the arm, which nothing does, because arms are
 * *indirect* targets and splitting cuts on direct branches. The interpreter
 * trapped on the first string it hashed.
 *
 * The order of the passes is the whole difficulty: recovery needs decoded
 * instructions, decoding needs extents, extents come from discovery, and
 * discovery is what would have to know. `crate::frontend::settle` closes
 * that loop; this is the fixture that proves it, and the fixture that fails
 * without it.
 *
 * Everything here is hand-written because a compiler will not oblige: it
 * has no reason to emit a jump into the interior of a function it also gave
 * a jump table, and the two have to coincide in one body.
 *
 * `dispatch` states its own size, so the extent an arm cuts is a **stated**
 * one. That is deliberate: it is the claim that a recovered arm carries the
 * standing of a proven transfer rather than of a weak witness, made into
 * something that fails if the claim is retracted.
 *
 * Each answer is a distinct bit, so the sum says which arms were reached
 * and the six bytes written say in which order.
 */

	.text

/* long dispatch(long selector)
 *
 * Arms 0 and 1 sit before `.Lside`; arms 2 and 3 sit after it. `side_door`
 * jumps straight to `.Lside`, which is a branch into this function's
 * interior from outside — so discovery cuts there, and arms 2 and 3 land in
 * the piece below the cut with nothing beginning at them.
 */
	.globl	dispatch
	.type	dispatch, @function
dispatch:
	cmpq	$3, %rdi
	ja	.Ldefault
	jmp	*jump_table(,%rdi,8)
.Larm0:
	movq	$1, %rax
	ret
.Larm1:
	movq	$2, %rax
	ret
/* The side door, and therefore the cut. */
.Lside:
	movq	$32, %rax
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

/* What cuts `dispatch`: an ordinary direct jump into its middle, which is
 * how a compiler reaches a shared tail or a cold fragment. */
	.globl	side_door
	.type	side_door, @function
side_door:
	jmp	.Lside
	.size	side_door, .-side_door

/* Runs every arm, the default, and the side door; writes what each returned
 * and leaves with their sum. Both halves are compared against the same
 * program run natively. */
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

	call	side_door
	movb	%al, results(,%r12,1)
	addq	%rax, %rbx

	movl	$1, %eax		/* write(1, results, 6) */
	movl	$1, %edi
	movl	$results, %esi
	movl	$6, %edx
	syscall

	movl	$231, %eax		/* exit_group(sum) */
	movq	%rbx, %rdi
	syscall
	.size	_start, .-_start

	.section .rodata
	.align	8
jump_table:
	.quad	.Larm0
	.quad	.Larm1
	.quad	.Larm2
	.quad	.Larm3

	.bss
	.align	8
results:
	.zero	8

	.section .note.GNU-stack,"",@progbits
