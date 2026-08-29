/* A genuine 128-bit dividend, which no C expression produces.
 *
 * A compiler emits `div` and `idiv` only after `cqo` or a zeroed `rdx`, so
 * every dividend it builds fits in sixty-four bits — and a translation that
 * handles only that case passes every test written in C. The instruction
 * divides `rdx:rax` and a library's own extended-precision arithmetic uses
 * it that way: glibc's `strtold` scales a mantissa by dividing a value that
 * genuinely occupies both registers.
 *
 * So these set `rdx` themselves. Each takes the dividend's high half, its
 * low half, and the divisor, and returns one of the two results, because a
 * quotient and a remainder that were compared together could hide a wrong
 * answer in whichever one the caller looked at second.
 *
 * The caller is responsible for the inputs being divisible at all: a
 * divisor of zero, or a quotient too wide for `rax`, raises a divide error,
 * which on the native side of a differential means the test process dies.
 */

	.text

	.globl	wide_unsigned_quotient
	.type	wide_unsigned_quotient, @function
wide_unsigned_quotient:
	movq	%rdx, %rcx
	movq	%rdi, %rdx
	movq	%rsi, %rax
	divq	%rcx
	ret
	.size	wide_unsigned_quotient, .-wide_unsigned_quotient

	.globl	wide_unsigned_remainder
	.type	wide_unsigned_remainder, @function
wide_unsigned_remainder:
	movq	%rdx, %rcx
	movq	%rdi, %rdx
	movq	%rsi, %rax
	divq	%rcx
	movq	%rdx, %rax
	ret
	.size	wide_unsigned_remainder, .-wide_unsigned_remainder

	.globl	wide_signed_quotient
	.type	wide_signed_quotient, @function
wide_signed_quotient:
	movq	%rdx, %rcx
	movq	%rdi, %rdx
	movq	%rsi, %rax
	idivq	%rcx
	ret
	.size	wide_signed_quotient, .-wide_signed_quotient

	.globl	wide_signed_remainder
	.type	wide_signed_remainder, @function
wide_signed_remainder:
	movq	%rdx, %rcx
	movq	%rdi, %rdx
	movq	%rsi, %rax
	idivq	%rcx
	movq	%rdx, %rax
	ret
	.size	wide_signed_remainder, .-wide_signed_remainder

/* The divisor in memory rather than a register, which is the other operand
 * form and reaches the same helper by a different path. */
	.globl	wide_unsigned_quotient_memory
	.type	wide_unsigned_quotient_memory, @function
wide_unsigned_quotient_memory:
	movq	%rdx, -8(%rsp)
	movq	%rdi, %rdx
	movq	%rsi, %rax
	divq	-8(%rsp)
	ret
	.size	wide_unsigned_quotient_memory, .-wide_unsigned_quotient_memory

	.section	.note.GNU-stack,"",@progbits
