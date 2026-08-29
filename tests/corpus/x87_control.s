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

/* `fnstsw %ax` writes a *word*, so RAX's upper six bytes survive it — which
   is what the hardware does and what a translation writing the full
   register would silently break. `stack_depth` above reads the status word
   back and would pass either way, because it zero-extends; this carries a
   witness through in the bytes `fnstsw` must not touch.

   It is also the sequence the effects analysis has to see correctly: the ST
   registers are invisible to it, while `fnstsw`'s RAX write and the read
   after it are not, so promotion has to keep them in step. */
	.globl	status_word_keeps_upper_bytes
	.type	status_word_keeps_upper_bytes, @function
status_word_keeps_upper_bytes:
	movq	%rdi, -8(%rsp)
	movabsq	$0x5a5a5a5a5a5a0000, %rax
	fildll	-8(%rsp)
	fld	%st(0)
	fnstsw	%ax
	fstp	%st(0)
	fstp	%st(0)
	/* The witness in the upper bytes, and the top-of-stack field the status
	   word carries, folded together so that losing either one shows. */
	movq	%rax, %rcx
	shrq	$32, %rcx
	movzwl	%ax, %eax
	andl	$0x3800, %eax
	orq	%rcx, %rax
	ret
	.size	status_word_keeps_upper_bytes, .-status_word_keeps_upper_bytes

/* ---- `fprem`, and the protocol that makes it terminate ------------------
 *
 * `fprem` does not compute a remainder; it computes *some* of one, and sets
 * C2 to say it is not finished. The caller loops until C2 clears. This is
 * the shape musl's `fmodl` writes, and it is the reason a wrong step rule
 * shows up as a hang rather than a wrong answer — so the differential is
 * checking two things at once: that the answer matches, and that the
 * question terminates at all.
 *
 * Operands arrive as `double` bit patterns, which is the widest thing that
 * crosses this boundary; the arithmetic inside is extended.
 */
	.globl	partial_remainder
	.type	partial_remainder, @function
partial_remainder:
	movq	%rdi, -8(%rsp)
	movq	%rsi, -16(%rsp)
	fldl	-16(%rsp)		/* ST0 = divisor */
	fldl	-8(%rsp)		/* ST0 = dividend, ST1 = divisor */
1:	fprem
	fnstsw	%ax
	testb	$4, %ah			/* C2 is bit 10, so bit 2 of %ah */
	jnz	1b
	fstpl	-24(%rsp)
	fstp	%st(0)
	movq	-24(%rsp), %rax
	ret
	.size	partial_remainder, .-partial_remainder

/* `fprem1` is the IEEE remainder, and rounds the implied quotient to
   nearest rather than truncating it — the same C2 protocol, a different
   answer, and a family a translation can easily collapse into one. */
	.globl	ieee_remainder
	.type	ieee_remainder, @function
ieee_remainder:
	movq	%rdi, -8(%rsp)
	movq	%rsi, -16(%rsp)
	fldl	-16(%rsp)
	fldl	-8(%rsp)
1:	fprem1
	fnstsw	%ax
	testb	$4, %ah
	jnz	1b
	fstpl	-24(%rsp)
	fstp	%st(0)
	movq	-24(%rsp), %rax
	ret
	.size	ieee_remainder, .-ieee_remainder

/* ---- `fscale` and `fxtract`, which are each other's inverse -------------- */

/* `fscale` multiplies by a power of two whose exponent is ST(1) *truncated*,
   so a fractional scale is part of what it does rather than a misuse. */
	.globl	scale_by_power
	.type	scale_by_power, @function
scale_by_power:
	movq	%rsi, -16(%rsp)
	movq	%rdi, -8(%rsp)
	fldl	-16(%rsp)		/* ST0 = scale, as a double */
	fldl	-8(%rsp)		/* ST0 = value, ST1 = scale */
	fscale
	fstpl	-24(%rsp)
	fstp	%st(0)
	movq	-24(%rsp), %rax
	ret
	.size	scale_by_power, .-scale_by_power

/* `fxtract` splits one value into two, which is the shape a stack machine
   makes awkward: the significand replaces the operand and the exponent is
   pushed under it. Both halves are returned, one function each, so a
   translation that produced them in the wrong order fails one of them. */
	.globl	extract_significand
	.type	extract_significand, @function
extract_significand:
	movq	%rdi, -8(%rsp)
	fldl	-8(%rsp)
	fxtract				/* ST0 = significand, ST1 = exponent */
	fstpl	-16(%rsp)
	fstp	%st(0)
	movq	-16(%rsp), %rax
	ret
	.size	extract_significand, .-extract_significand

	.globl	extract_exponent
	.type	extract_exponent, @function
extract_exponent:
	movq	%rdi, -8(%rsp)
	fldl	-8(%rsp)
	fxtract
	fstp	%st(0)			/* drop the significand */
	fstpl	-16(%rsp)
	movq	-16(%rsp), %rax
	ret
	.size	extract_exponent, .-extract_exponent

/* ---- `fxam`, one operand class at a time --------------------------------
 *
 * The condition codes are masked out of the status word because the rest of
 * it is history: the exception flags are sticky and the top-of-stack field
 * depends on what ran before, neither of which this is asking about. C3,
 * C2, C1 and C0 are bits 14, 10, 9 and 8.
 */
	.globl	classify
	.type	classify, @function
classify:
	movq	%rdi, -8(%rsp)
	fldl	-8(%rsp)
	fxam
	fnstsw	%ax
	movzwl	%ax, %eax
	andl	$0x4700, %eax
	fstp	%st(0)
	ret
	.size	classify, .-classify

/* The denormal class, which needs an operand this boundary cannot carry: an
   extended denormal. A subnormal `double` is not one — loading it widens the
   exponent field and it arrives normalised — so the value is made in the
   register instead, by scaling a normal one below the extended format's
   range. */
	.globl	classify_denormal
	.type	classify_denormal, @function
classify_denormal:
	movq	%rdi, -8(%rsp)
	movq	$-16400, -16(%rsp)
	fildll	-16(%rsp)		/* ST0 = the scale */
	fldl	-8(%rsp)		/* ST0 = value, ST1 = scale */
	fscale
	fxam
	fnstsw	%ax
	movzwl	%ax, %eax
	andl	$0x4700, %eax
	fstp	%st(0)
	fstp	%st(0)
	ret
	.size	classify_denormal, .-classify_denormal

/* The empty class, which is the one `fxam` answer no operand can produce:
   the register has to be marked empty without the stack pointer moving,
   which is exactly what `ffree` does and nothing else does. */
	.globl	classify_empty
	.type	classify_empty, @function
classify_empty:
	fldz
	ffree	%st(0)
	fxam
	fnstsw	%ax
	movzwl	%ax, %eax
	andl	$0x4700, %eax
	fincstp				/* the slot is empty; step over it */
	ret
	.size	classify_empty, .-classify_empty

/* ---- the stack pointer itself -------------------------------------------
 *
 * `fincstp` and `fdecstp` rotate the register file without touching a tag
 * or a value, so the only way to see them is to leave a value somewhere and
 * come back for it. This pushes three, walks the pointer all the way round,
 * and reads back what should be untouched — a translation that moved a
 * value instead of the pointer, or that rotated the wrong way, returns
 * something else.
 */
	.globl	rotate_stack
	.type	rotate_stack, @function
rotate_stack:
	movq	%rdi, -8(%rsp)
	fildll	-8(%rsp)		/* ST0 = a */
	fld1				/* ST0 = 1, ST1 = a */
	fldz				/* ST0 = 0, ST1 = 1, ST2 = a */
	fincstp				/* ST0 = 1, ST1 = a, ST7 = 0 */
	fincstp				/* ST0 = a, ST7 = 1, ST6 = 0 */
	fdecstp
	fdecstp				/* back where it started */
	fstp	%st(0)
	fstp	%st(0)
	fistpll	-16(%rsp)		/* what is left must still be `a` */
	movq	-16(%rsp), %rax
	ret
	.size	rotate_stack, .-rotate_stack

/* ---- the control word, saved and put back -------------------------------
 *
 * The rounding-mode round trip a C library performs around a conversion:
 * read the control word, force round-toward-zero, do the work, put the old
 * word back. Written here rather than with `<fenv.h>` because the corpus
 * links against no libc — the instructions are the same ones `fesetround`
 * would have emitted.
 *
 * Both answers come back, the truncated one in the low half and a witness
 * that the control word was restored in the high, because a translation
 * that dropped the restore would still get the first one right.
 */
	.globl	rounding_round_trip
	.type	rounding_round_trip, @function
rounding_round_trip:
	movq	%rdi, -8(%rsp)
	fnstcw	-16(%rsp)
	movzwl	-16(%rsp), %eax
	movl	%eax, %ecx
	orl	$0xc00, %ecx		/* RC = 11, round toward zero */
	movw	%cx, -18(%rsp)
	fldcw	-18(%rsp)
	fldl	-8(%rsp)
	frndint
	fistpll	-32(%rsp)
	fldcw	-16(%rsp)		/* the original word back */
	fldl	-8(%rsp)
	frndint				/* now under the restored mode */
	fistpll	-40(%rsp)
	movq	-32(%rsp), %rax
	shlq	$32, %rax
	movl	-40(%rsp), %ecx
	orq	%rcx, %rax
	ret
	.size	rounding_round_trip, .-rounding_round_trip

/* ---- `fnsave` and `frstor`: the whole file, out and back -----------------
 *
 * `fnsave` writes 108 bytes and *reinitialises* the unit, which is what
 * makes this a real test rather than a copy: the value has to survive being
 * erased. The clobber between the two is there so that a `frstor` that
 * restored nothing would be visible.
 */
	.globl	save_and_restore
	.type	save_and_restore, @function
save_and_restore:
	subq	$136, %rsp
	movq	%rdi, 120(%rsp)
	fildll	120(%rsp)		/* ST0 = a */
	fnsave	(%rsp)			/* out, and the unit is now empty */
	fld1				/* the clobber */
	fldz
	frstor	(%rsp)			/* back, clobber and all gone */
	fistpll	120(%rsp)
	movq	120(%rsp), %rax
	addq	$136, %rsp
	ret
	.size	save_and_restore, .-save_and_restore

/* The environment alone — control, status and tags, no registers — which is
   the other pair and a different image size. `fnstenv` reinitialises too. */
	.globl	environment_round_trip
	.type	environment_round_trip, @function
environment_round_trip:
	subq	$56, %rsp
	fnstcw	48(%rsp)
	movzwl	48(%rsp), %eax
	orl	$0xc00, %eax
	movw	%ax, 50(%rsp)
	fldcw	50(%rsp)		/* a control word worth saving */
	fnstenv	(%rsp)
	fldcw	48(%rsp)		/* clobber it */
	fldenv	(%rsp)			/* and put the saved one back */
	fnstcw	52(%rsp)
	movzwl	52(%rsp), %eax
	addq	$56, %rsp
	ret
	.size	environment_round_trip, .-environment_round_trip

	.section .note.GNU-stack,"",@progbits
