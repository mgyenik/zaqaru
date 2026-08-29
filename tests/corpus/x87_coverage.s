/* The x87 mnemonics no compiler emits, one case each.
 *
 * `long_double.c` covers what gcc and clang actually produce, which is a
 * little over half of what the lowering implements. The rest — the
 * conditional moves, the non-`i` compares, the integer-operand arithmetic,
 * four of the seven constants — appear in hand-written library assembly and
 * nowhere else, so they are written by hand here. This is the file that
 * would have caught the `fxch`/`fcom` operand-index bug on its own: every
 * case names a stack slot that is not ST(0), because a lowering that reads
 * the wrong operand is right about `st(1)` by accident.
 *
 * What is deliberately absent: `f2xm1`, `fyl2x`, `fyl2xp1` and `fpatan`.
 * The crate backs those with f64 and measures its divergence from the
 * hardware rather than matching it — thousands of ulps of extended, by
 * design and recorded in the tier table — so a bit-exact differential
 * against native is the one thing they must not be put in. Their oracle is
 * in the crate. `fisttp` is absent for a different reason: it is SSE3, and
 * the corpus is built for a baseline that does not have it.
 *
 * Convention: `long name(long, long)`, operands arriving through memory,
 * the answer in `%rax`, and the stack left clean.
 */

	.text

/* ---- the constants ------------------------------------------------------
 *
 * Each is a different index into the same helper, so a lowering that
 * transposed two of them would still pass a test that only loaded one.
 * Returned as `double` bits: the extended constants are wider than that,
 * but the rounding is the hardware's and the comparison is exact.
 */
.macro CONSTANT name, insn
	.globl	\name
	.type	\name, @function
\name:
	\insn
	fstpl	-8(%rsp)
	movq	-8(%rsp), %rax
	ret
	.size	\name, .-\name
.endm

	CONSTANT	constant_one, fld1
	CONSTANT	constant_log2_ten, fldl2t
	CONSTANT	constant_log2_e, fldl2e
	CONSTANT	constant_pi, fldpi
	CONSTANT	constant_log10_two, fldlg2
	CONSTANT	constant_ln_two, fldln2
	CONSTANT	constant_zero, fldz

/* ---- the unary operations ----------------------------------------------- */

.macro UNARY name, insn
	.globl	\name
	.type	\name, @function
\name:
	movq	%rdi, -8(%rsp)
	fldl	-8(%rsp)
	\insn
	fstpl	-16(%rsp)
	movq	-16(%rsp), %rax
	ret
	.size	\name, .-\name
.endm

	UNARY	absolute_value, fabs
	UNARY	negate, fchs
	UNARY	square_root, fsqrt
	UNARY	round_to_integer, frndint

/* `ftst` compares against zero and answers in the condition codes, which is
   the one thing `fcomi` cannot be substituted for: it predates the flag
   forms and writes C3/C2/C0 instead. */
	.globl	test_against_zero
	.type	test_against_zero, @function
test_against_zero:
	movq	%rdi, -8(%rsp)
	fldl	-8(%rsp)
	ftst
	fnstsw	%ax
	movzwl	%ax, %eax
	andl	$0x4500, %eax
	fstp	%st(0)
	ret
	.size	test_against_zero, .-test_against_zero

/* ---- the compares that answer in the condition codes --------------------
 *
 * `fcom` and `fucom` differ only in what they do with a signalling NaN,
 * which this cannot see with the exceptions masked; what it can see is the
 * popping. Each form leaves the stack at a different depth, so the depth is
 * returned alongside the answer and a lowering that popped the wrong number
 * of times fails even where the comparison itself was right.
 */
.macro CONDITION_COMPARE name, insn, pops
	.globl	\name
	.type	\name, @function
\name:
	movq	%rsi, -16(%rsp)
	movq	%rdi, -8(%rsp)
	fldl	-16(%rsp)		/* ST1 = b */
	fldl	-8(%rsp)		/* ST0 = a */
	\insn
	fnstsw	%ax
	movzwl	%ax, %eax
	andl	$0x4700, %eax		/* the codes, and the stack pointer */
	.rept	\pops
	fstp	%st(0)
	.endr
	ret
	.size	\name, .-\name
.endm

	CONDITION_COMPARE	compare_registers, "fcom %st(1)", 2
	CONDITION_COMPARE	compare_and_pop, "fcomp %st(1)", 1
	CONDITION_COMPARE	compare_and_pop_both, fcompp, 0
	CONDITION_COMPARE	compare_unordered_registers, "fucom %st(1)", 2
	CONDITION_COMPARE	compare_unordered_pop, "fucomp %st(1)", 1
	CONDITION_COMPARE	compare_unordered_pop_both, fucompp, 0

/* The memory forms, which take their operand by value rather than from the
   stack — a different helper and a different argument. */
.macro MEMORY_COMPARE name, insn, pops
	.globl	\name
	.type	\name, @function
\name:
	movq	%rsi, -16(%rsp)
	movq	%rdi, -8(%rsp)
	fldl	-8(%rsp)
	\insn	-16(%rsp)
	fnstsw	%ax
	movzwl	%ax, %eax
	andl	$0x4700, %eax
	.rept	\pops
	fstp	%st(0)
	.endr
	ret
	.size	\name, .-\name
.endm

	MEMORY_COMPARE	compare_memory_double, fcoml, 1
	MEMORY_COMPARE	compare_memory_double_pop, fcompl, 0
	MEMORY_COMPARE	compare_memory_integer, ficoml, 1
	MEMORY_COMPARE	compare_memory_integer_pop, ficompl, 0

/* ---- arithmetic against an integer in memory ---------------------------- */

.macro INTEGER_ARITHMETIC name, insn
	.globl	\name
	.type	\name, @function
\name:
	movq	%rdi, -8(%rsp)
	movl	%esi, -16(%rsp)
	fldl	-8(%rsp)
	\insn	-16(%rsp)
	fstpl	-24(%rsp)
	movq	-24(%rsp), %rax
	ret
	.size	\name, .-\name
.endm

	INTEGER_ARITHMETIC	integer_add, fiaddl
	INTEGER_ARITHMETIC	integer_subtract, fisubl
	INTEGER_ARITHMETIC	integer_subtract_reversed, fisubrl
	INTEGER_ARITHMETIC	integer_multiply, fimull
	INTEGER_ARITHMETIC	integer_divide, fidivl
	INTEGER_ARITHMETIC	integer_divide_reversed, fidivrl

/* The sixteen-bit forms, whose operand is a different width and a different
   helper. */
.macro SHORT_ARITHMETIC name, insn
	.globl	\name
	.type	\name, @function
\name:
	movq	%rdi, -8(%rsp)
	movw	%si, -16(%rsp)
	fldl	-8(%rsp)
	\insn	-16(%rsp)
	fstpl	-24(%rsp)
	movq	-24(%rsp), %rax
	ret
	.size	\name, .-\name
.endm

	SHORT_ARITHMETIC	short_add, fiadds
	SHORT_ARITHMETIC	short_divide_reversed, fidivrs

/* ---- storing to integers ------------------------------------------------ */

	.globl	store_short
	.type	store_short, @function
store_short:
	movq	%rdi, -8(%rsp)
	fldl	-8(%rsp)
	fistps	-16(%rsp)
	movswq	-16(%rsp), %rax
	ret
	.size	store_short, .-store_short

/* `fist` without the pop, which leaves the value in place — so the stack
   has to be cleaned by hand, and a lowering that popped anyway leaves it
   at a depth this notices. */
	.globl	store_integer_keeping
	.type	store_integer_keeping, @function
store_integer_keeping:
	movq	%rdi, -8(%rsp)
	fldl	-8(%rsp)
	fistl	-16(%rsp)
	fnstsw	%ax
	movzwl	%ax, %eax
	andl	$0x3800, %eax		/* the depth, which must be unchanged */
	movslq	-16(%rsp), %rcx
	fstp	%st(0)
	addq	%rcx, %rax
	ret
	.size	store_integer_keeping, .-store_integer_keeping

/* ---- the conditional moves ---------------------------------------------
 *
 * The condition comes from an integer `cmp`, so the flags are the ordinary
 * ones and the eight forms differ only in which of them they read. ST(2) is
 * the source in every case, because a lowering that read ST(1) — the
 * default for the forms that name no register — would pass otherwise.
 */
.macro CONDITIONAL_MOVE name, insn
	.globl	\name
	.type	\name, @function
\name:
	movq	%rdi, -8(%rsp)
	movq	%rsi, -16(%rsp)
	fildll	-8(%rsp)		/* ST2, once the others are pushed */
	fldz
	fld1				/* ST0 = 1, ST1 = 0, ST2 = a */
	cmpq	%rsi, %rdi
	\insn	%st(2), %st
	fistpll	-24(%rsp)
	fstp	%st(0)
	fstp	%st(0)
	movq	-24(%rsp), %rax
	ret
	.size	\name, .-\name
.endm

	CONDITIONAL_MOVE	move_if_below, fcmovb
	CONDITIONAL_MOVE	move_if_equal, fcmove
	CONDITIONAL_MOVE	move_if_below_or_equal, fcmovbe
	CONDITIONAL_MOVE	move_if_unordered, fcmovu
	CONDITIONAL_MOVE	move_if_not_below, fcmovnb
	CONDITIONAL_MOVE	move_if_not_equal, fcmovne
	CONDITIONAL_MOVE	move_if_not_below_or_equal, fcmovnbe
	CONDITIONAL_MOVE	move_if_not_unordered, fcmovnu

/* ---- the housekeeping ---------------------------------------------------
 *
 * `fnclex` clears the exception flags and `fninit` resets the whole unit;
 * both are visible only in the status word, so the status word is what
 * comes back. `ffreep` frees and pops in one instruction — undocumented,
 * real, and emitted by hand-written library code. `fnop` does nothing,
 * which still has to translate to nothing rather than to a refusal.
 */
	.globl	clear_exceptions
	.type	clear_exceptions, @function
clear_exceptions:
	movq	%rdi, -8(%rsp)
	fildll	-8(%rsp)
	fnop
	fnclex
	fnstsw	%ax
	movzwl	%ax, %eax
	andl	$0xbf00, %eax
	fstp	%st(0)
	ret
	.size	clear_exceptions, .-clear_exceptions

	.globl	initialise_unit
	.type	initialise_unit, @function
initialise_unit:
	fldz
	fldz
	fninit
	fnstsw	%ax
	movzwl	%ax, %eax
	fnstcw	-8(%rsp)
	movzwl	-8(%rsp), %ecx
	shlq	$16, %rcx
	orq	%rcx, %rax
	ret
	.size	initialise_unit, .-initialise_unit

	.globl	free_and_pop
	.type	free_and_pop, @function
free_and_pop:
	movq	%rdi, -8(%rsp)
	fildll	-8(%rsp)
	fldz
	ffreep	%st(0)			/* the zero goes, `a` becomes ST0 */
	fistpll	-16(%rsp)
	movq	-16(%rsp), %rax
	ret
	.size	free_and_pop, .-free_and_pop

	.section	.note.GNU-stack,"",@progbits
