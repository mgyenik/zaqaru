/* The x87 stack's own shapes, hand-written because no compiler emits them.
 *
 * Compiler output reaches whichever forms the optimiser happened to want and
 * leaves the rest untested — which is exactly where a wrong stack index or a
 * reversed operand hides. Every function here builds its operands from
 * integer bit patterns so a caller can name an exact value, and returns
 * something that differs if the shape is wrong.
 *
 * `long name(long, long)`, operands arriving through memory the way `fild`
 * needs them.
 */

	.text

/* Packs the three flags an `fcomi` defines. */
.macro PACK_COMPARE
	setc	%r8b
	setp	%r9b
	setz	%r10b
	movzbl	%r8b, %eax
	movzbl	%r9b, %r9d
	shll	$1, %r9d
	orl	%r9d, %eax
	movzbl	%r10b, %r10d
	shll	$2, %r10d
	orl	%r10d, %eax
.endm

/* Two integers loaded, then compared: ST(0) is the second pushed. */
	.globl	compare_integers
	.type	compare_integers, @function
compare_integers:
	movq	%rdi, -8(%rsp)
	movq	%rsi, -16(%rsp)
	fildll	-8(%rsp)		/* ST0 = a */
	fildll	-16(%rsp)		/* ST0 = b, ST1 = a */
	fcomi	%st(1), %st		/* b against a */
	PACK_COMPARE
	fstp	%st(0)
	fstp	%st(0)
	ret
	.size	compare_integers, .-compare_integers

/* The same after an exchange, which must reverse the answer. A no-op `fxch`
   gives the previous function's answer instead. */
	.globl	compare_after_exchange
	.type	compare_after_exchange, @function
compare_after_exchange:
	movq	%rdi, -8(%rsp)
	movq	%rsi, -16(%rsp)
	fildll	-8(%rsp)
	fildll	-16(%rsp)
	fxch	%st(1)			/* ST0 = a, ST1 = b */
	fcomi	%st(1), %st		/* a against b */
	PACK_COMPARE
	fstp	%st(0)
	fstp	%st(0)
	ret
	.size	compare_after_exchange, .-compare_after_exchange

/* `fld %st(0)` duplicates the top. Three values go on, the middle one is
   compared, and a duplicate that failed to push leaves the wrong one there. */
	.globl	compare_after_duplicate
	.type	compare_after_duplicate, @function
compare_after_duplicate:
	movq	%rdi, -8(%rsp)
	movq	%rsi, -16(%rsp)
	fildll	-8(%rsp)		/* ST0 = a */
	fld	%st(0)			/* ST0 = a, ST1 = a */
	fildll	-16(%rsp)		/* ST0 = b, ST1 = a, ST2 = a */
	fxch	%st(2)			/* ST0 = a, ST1 = a, ST2 = b */
	fcomi	%st(2), %st		/* a against b */
	PACK_COMPARE
	fstp	%st(0)
	fstp	%st(0)
	fstp	%st(0)
	ret
	.size	compare_after_duplicate, .-compare_after_duplicate

/* The arithmetic direction battery. Each returns a value that differs if the
   destination, the source or the reversal is wrong. */
.macro DIRECTION name, op
	.globl	\name
	.type	\name, @function
\name:
	movq	%rdi, -8(%rsp)
	movq	%rsi, -16(%rsp)
	fildll	-8(%rsp)		/* ST0 = a */
	fildll	-16(%rsp)		/* ST0 = b, ST1 = a */
	\op
	fistpll	-24(%rsp)
	movq	-24(%rsp), %rax
	ret
	.size	\name, .-\name
.endm

/* `st(1) = st(1) - st(0)` and its reversal, in the encodings that carry the
   two directions. The AT&T spelling of these is famously the other way
   round from the opcode, which is the reason for testing rather than
   reading. */
DIRECTION subtract_into_second, "fsubp %st, %st(1)"
DIRECTION subtract_reversed_into_second, "fsubrp %st, %st(1)"
DIRECTION divide_into_second, "fdivp %st, %st(1)"
DIRECTION divide_reversed_into_second, "fdivrp %st, %st(1)"

/* The non-popping register forms, which name their destination explicitly. */
	.globl	subtract_into_top
	.type	subtract_into_top, @function
subtract_into_top:
	movq	%rdi, -8(%rsp)
	movq	%rsi, -16(%rsp)
	fildll	-8(%rsp)
	fildll	-16(%rsp)
	fsub	%st(1), %st		/* ST0 = ST0 - ST1 */
	fistpll	-24(%rsp)
	fstp	%st(0)
	movq	-24(%rsp), %rax
	ret
	.size	subtract_into_top, .-subtract_into_top

	.globl	subtract_reversed_into_top
	.type	subtract_reversed_into_top, @function
subtract_reversed_into_top:
	movq	%rdi, -8(%rsp)
	movq	%rsi, -16(%rsp)
	fildll	-8(%rsp)
	fildll	-16(%rsp)
	fsubr	%st(1), %st		/* ST0 = ST1 - ST0 */
	fistpll	-24(%rsp)
	fstp	%st(0)
	movq	-24(%rsp), %rax
	ret
	.size	subtract_reversed_into_top, .-subtract_reversed_into_top

/* The status word's top-of-stack field, which says how deep the stack got.
   A push that did not push, or a pop that did not pop, shows up here and
   nowhere else. */
	.globl	stack_depth
	.type	stack_depth, @function
stack_depth:
	fildll	-8(%rsp)
	fld	%st(0)
	fld	%st(0)
	fnstsw	%ax
	movzwl	%ax, %eax
	shrl	$11, %eax
	andl	$7, %eax
	fstp	%st(0)
	fstp	%st(0)
	fstp	%st(0)
	ret
	.size	stack_depth, .-stack_depth

/* The red zone, across a helper call.
 *
 * A translated x87 instruction is a call into the softfloat crate, and a
 * compiler puts values in the 128 bytes below `%rsp` without moving it —
 * the SysV red zone — expecting nothing to disturb them. `compare_branch`
 * in the long-double corpus does exactly this: two `movsd` into the red
 * zone, then two x87 instructions, then a load back. If a helper writes
 * there, the value that comes back is not the one that went in.
 */
	.globl	red_zone_across_helpers
	.type	red_zone_across_helpers, @function
red_zone_across_helpers:
	movq	%rdi, -8(%rsp)
	movq	%rsi, -72(%rsp)
	fldz
	fld1
	fadd	%st(1), %st
	fildll	-8(%rsp)
	fistpll	-16(%rsp)
	fildll	-72(%rsp)
	fistpll	-80(%rsp)
	fstp	%st(0)
	fstp	%st(0)
	movq	-16(%rsp), %rax
	addq	-80(%rsp), %rax
	ret
	.size	red_zone_across_helpers, .-red_zone_across_helpers

	.section .note.GNU-stack,"",@progbits
