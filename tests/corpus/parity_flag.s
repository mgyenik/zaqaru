/* Float-plan milestone 2: the parity flag, consumed after integer
 * arithmetic.
 *
 * The flag exists in the machine model because floating-point compares report
 * *unordered* through it, and compilers branch on `jp` immediately after a
 * `ucomisd`. Integer code essentially never reads it, so no compiled corpus
 * case will — which is why these are written by hand, in the way the
 * irreducible-graph case is.
 *
 * Parity is the flag with the least obvious rule: it is the parity of the
 * **low byte** of the result, whatever the operand's width, and it is set
 * when that byte has an *even* number of one bits. Both halves of that are
 * easy to get backwards, and each function here returns a value that changes
 * if either is.
 *
 * Every function is `long name(long left, long right)`.
 */

	.text

/* A case that performs some arithmetic and hands back the parity flag it
 * left behind. `eax` is cleared first so that the byte `setp` writes is the
 * whole answer. */
	.macro	parity_case name, body:vararg
	.globl	\name
	.type	\name, @function
\name:
	xorl	%eax, %eax
	\body
	setp	%al
	ret
	.size	\name, .-\name
	.endm

/* The same, reading the flag the other way round, so that a translation which
 * computed the complement would pass one of the pair and fail the other. */
	.macro	not_parity_case name, body:vararg
	.globl	\name
	.type	\name, @function
\name:
	xorl	%eax, %eax
	\body
	setnp	%al
	ret
	.size	\name, .-\name
	.endm

/* ---- the arithmetic and logic rules ------------------------------------- */

	parity_case	parity_after_add_long, addq %rsi, %rdi
	parity_case	parity_after_add_int, addl %esi, %edi
	parity_case	parity_after_add_short, addw %si, %di
	parity_case	parity_after_add_byte, addb %sil, %dil
	parity_case	parity_after_subtract, subl %esi, %edi
	parity_case	parity_after_and, andl %esi, %edi
	parity_case	parity_after_or, orl %esi, %edi
	parity_case	parity_after_exclusive_or, xorl %esi, %edi
	parity_case	parity_after_test, testl %esi, %edi
	parity_case	parity_after_test_byte, testb %sil, %dil
	parity_case	parity_after_compare, cmpl %esi, %edi
	parity_case	parity_after_compare_long, cmpq %rsi, %rdi
	parity_case	parity_after_compare_byte, cmpb %sil, %dil
	parity_case	parity_after_increment, incl %edi
	parity_case	parity_after_decrement, decl %edi
	parity_case	parity_after_negate, negl %edi

/* `imul` is deliberately absent: it leaves parity — like sign and zero —
   architecturally undefined, and the hardware this was written on preserves
   the previous value rather than computing one. Comparing that against a
   translation would be testing the processor, not the translation. The
   translator gives the flag the obvious value there, as it already does for
   sign and zero. */

	not_parity_case	not_parity_after_add_long, addq %rsi, %rdi
	not_parity_case	not_parity_after_and, andl %esi, %edi
	not_parity_case	not_parity_after_compare, cmpl %esi, %edi

/* ---- shifts, where a count of zero must leave every flag alone ----------- */

	parity_case	parity_after_shift_left, shll $3, %edi
	parity_case	parity_after_shift_right, sarl $2, %edi

	.globl	parity_after_variable_shift
	.type	parity_after_variable_shift, @function
parity_after_variable_shift:
	/* The shift count comes from the second argument, so the zero-count
	   case — where the architecture leaves every flag untouched, parity
	   included — is reachable from the test's inputs. `cmp` before the
	   shift puts a known parity in place for that case to preserve. */
	movl	%esi, %ecx
	cmpl	$0x0f, %edi
	shll	%cl, %edi
	/* `mov` rather than `xor`: clearing with `xor` would set the very
	   flag this is about to read. */
	movl	$0, %eax
	setp	%al
	ret
	.size	parity_after_variable_shift, .-parity_after_variable_shift

/* ---- the flag reaching a branch and a conditional move ------------------- */

	.globl	parity_branch
	.type	parity_branch, @function
parity_branch:
	addl	%esi, %edi
	jp	.Leven_parity
	movl	$-100, %eax
	ret
.Leven_parity:
	movl	$100, %eax
	ret
	.size	parity_branch, .-parity_branch

	.globl	parity_branch_complement
	.type	parity_branch_complement, @function
parity_branch_complement:
	andl	%esi, %edi
	jnp	.Lodd_parity
	movl	$7, %eax
	ret
.Lodd_parity:
	movl	$-7, %eax
	ret
	.size	parity_branch_complement, .-parity_branch_complement

	.globl	parity_conditional_move
	.type	parity_conditional_move, @function
parity_conditional_move:
	movl	$1234, %eax
	movl	$-4321, %ecx
	subl	%esi, %edi
	cmovp	%ecx, %eax
	ret
	.size	parity_conditional_move, .-parity_conditional_move

/* ---- the flag surviving a call ------------------------------------------- */

	.type	leave_flags_alone, @function
leave_flags_alone:
	/* Nothing here writes a flag, so the caller's parity must still be
	   there afterwards — which is why the machine model keeps flags in
	   globals rather than in locals. */
	movl	$0, %edx
	ret
	.size	leave_flags_alone, .-leave_flags_alone

	.globl	parity_survives_a_call
	.type	parity_survives_a_call, @function
parity_survives_a_call:
	addl	%esi, %edi
	call	leave_flags_alone
	movl	$0, %eax
	setp	%al
	ret
	.size	parity_survives_a_call, .-parity_survives_a_call

	.section	.note.GNU-stack,"",@progbits
