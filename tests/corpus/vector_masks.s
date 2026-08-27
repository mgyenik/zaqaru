/* Float-plan milestone 1: the write mask of every form of the SSE move
 * family, one exported function each.
 *
 * The merge/zero asymmetry is the trap the plan names, because getting it
 * wrong is silent: `movsd xmm, m64` zeroes bits 64..127 while
 * `movsd xmm1, xmm2` merges and preserves them, and `movss` does the same at
 * 32 bits. Compilers reach for whichever form suits them, so a corpus that
 * only runs compiler output covers whichever masks that day's compiler
 * happened to need. These cases cover every form on purpose, and each is set
 * up so that a wrong mask changes the returned number rather than hiding.
 *
 * Every function is `long name(long low, long high)` and works the same way:
 *
 *   xmm0 = (low, high)      the register under test
 *   xmm1 = (high, low)      a second one differing in both halves
 *   Q                       the same swapped pair, in memory, for the forms
 *                           whose source is a memory operand
 *   R                       a sixteen-byte result area pre-filled with ones,
 *                           so that a store which writes too few bytes leaves
 *                           marker bits behind and a store which writes too
 *                           many wipes them
 *
 * and returns all 128 bits of the result folded into one number.
 */

	.text

/* Scratch layout in the red zone, which the machine model reaches through
 * `rsp` exactly as it reaches any other memory.
 *
 * At function entry SysV leaves `rsp` eight past a sixteen-byte boundary, as
 * if a return address had just been pushed, so `rsp - 8` is the first aligned
 * address below it and every slot here is a multiple of sixteen from there.
 * The aligned movers fault on the machine otherwise — wasm loads never do,
 * which is precisely why the alignment has to be right in the oracle. */
	.set	PATTERN, -56
	.set	SWAPPED, -40
	.set	RESULT, -24

	.macro	vector_prologue
	movq	%rdi, PATTERN(%rsp)
	movq	%rsi, PATTERN+8(%rsp)
	movq	%rsi, SWAPPED(%rsp)
	movq	%rdi, SWAPPED+8(%rsp)
	movdqu	PATTERN(%rsp), %xmm0
	movdqu	SWAPPED(%rsp), %xmm1
	movq	$-1, %rax
	movq	%rax, RESULT(%rsp)
	movq	%rax, RESULT+8(%rsp)
	.endm

	.macro	vector_epilogue
	movq	RESULT(%rsp), %rax
	imulq	$31, %rax, %rax
	addq	RESULT+8(%rsp), %rax
	ret
	.endm

/* A case whose result is left in xmm0. */
	.macro	register_case name, body:vararg
	.globl	\name
	.type	\name, @function
\name:
	vector_prologue
	\body
	movdqu	%xmm0, RESULT(%rsp)
	vector_epilogue
	.size	\name, .-\name
	.endm

/* A case whose result is stored straight into the result area, so that what
 * is checked is which bytes of it the store actually touched. */
	.macro	store_case name, body:vararg
	.globl	\name
	.type	\name, @function
\name:
	vector_prologue
	\body
	vector_epilogue
	.size	\name, .-\name
	.endm

/* ---- the scalar moves, where merge and zero part company ---------------- */

	register_case	mask_movsd_register, movsd %xmm1, %xmm0
	register_case	mask_movsd_memory, movsd SWAPPED(%rsp), %xmm0
	register_case	mask_movss_register, movss %xmm1, %xmm0
	register_case	mask_movss_memory, movss SWAPPED(%rsp), %xmm0

	store_case	mask_movsd_store, movsd %xmm0, RESULT(%rsp)
	store_case	mask_movss_store, movss %xmm0, RESULT(%rsp)

/* ---- `movq` and `movd`, which always zero what they do not write -------- */

	register_case	mask_movq_register, movq %xmm1, %xmm0
	register_case	mask_movq_memory, movq SWAPPED(%rsp), %xmm0
	register_case	mask_movq_from_general, movq %rsi, %xmm0
	register_case	mask_movd_from_general, movd %esi, %xmm0
	register_case	mask_movd_memory, movd SWAPPED(%rsp), %xmm0

	store_case	mask_movq_store, movq %xmm0, RESULT(%rsp)
	store_case	mask_movd_store, movd %xmm0, RESULT(%rsp)

/* Out of a vector register and into a general-purpose one, where the
 * narrow form must zero the upper half of the destination register as any
 * 32-bit write does. */
	.globl	mask_movq_to_general
	.type	mask_movq_to_general, @function
mask_movq_to_general:
	vector_prologue
	movq	%xmm0, %rax
	ret
	.size	mask_movq_to_general, .-mask_movq_to_general

	.globl	mask_movd_to_general
	.type	mask_movd_to_general, @function
mask_movd_to_general:
	vector_prologue
	movq	$-1, %rax
	movd	%xmm0, %eax
	ret
	.size	mask_movd_to_general, .-mask_movd_to_general

/* ---- whole-register moves ------------------------------------------------ */

	register_case	mask_movaps_register, movaps %xmm1, %xmm0
	register_case	mask_movaps_memory, movaps SWAPPED(%rsp), %xmm0
	register_case	mask_movups_memory, movups SWAPPED+4(%rsp), %xmm0
	register_case	mask_movdqa_register, movdqa %xmm1, %xmm0
	register_case	mask_movdqu_memory, movdqu SWAPPED+4(%rsp), %xmm0
	register_case	mask_movupd_memory, movupd SWAPPED+4(%rsp), %xmm0

	store_case	mask_movaps_store, movaps %xmm0, RESULT(%rsp)
	store_case	mask_movdqu_store, movdqu %xmm0, RESULT(%rsp)

/* ---- one half at a time -------------------------------------------------- */

	register_case	mask_movlpd_load, movlpd SWAPPED(%rsp), %xmm0
	register_case	mask_movhpd_load, movhpd SWAPPED(%rsp), %xmm0
	register_case	mask_movlps_load, movlps SWAPPED(%rsp), %xmm0
	register_case	mask_movhps_load, movhps SWAPPED(%rsp), %xmm0
	register_case	mask_movhlps, movhlps %xmm1, %xmm0
	register_case	mask_movlhps, movlhps %xmm1, %xmm0

	store_case	mask_movlpd_store, movlpd %xmm0, RESULT(%rsp)
	store_case	mask_movhpd_store, movhpd %xmm0, RESULT(%rsp)

/* ---- the bitwise family, including the zeroing idiom --------------------- */

	register_case	mask_pxor_self, pxor %xmm0, %xmm0
	register_case	mask_xorps_self, xorps %xmm0, %xmm0
	register_case	mask_pxor_register, pxor %xmm1, %xmm0
	register_case	mask_pand_register, pand %xmm1, %xmm0
	register_case	mask_por_register, por %xmm1, %xmm0
	register_case	mask_pandn_register, pandn %xmm1, %xmm0
	register_case	mask_andpd_memory, andpd SWAPPED(%rsp), %xmm0
	register_case	mask_orps_memory, orps SWAPPED(%rsp), %xmm0

/* ---- lane rearrangement -------------------------------------------------- */

	register_case	mask_pshufd, pshufd $0x1b, %xmm1, %xmm0
	register_case	mask_pshufd_broadcast, pshufd $0xaa, %xmm0, %xmm0
	register_case	mask_pshufd_memory, pshufd $0x4e, SWAPPED(%rsp), %xmm0
	register_case	mask_punpckldq, punpckldq %xmm1, %xmm0
	register_case	mask_punpckhdq, punpckhdq %xmm1, %xmm0
	register_case	mask_punpcklqdq, punpcklqdq %xmm1, %xmm0
	register_case	mask_punpckhqdq, punpckhqdq %xmm1, %xmm0
	register_case	mask_unpcklps, unpcklps %xmm1, %xmm0
	register_case	mask_unpckhpd, unpckhpd %xmm1, %xmm0
	register_case	mask_shufps, shufps $0x93, %xmm1, %xmm0
	register_case	mask_shufpd, shufpd $0x3, %xmm1, %xmm0

	.section	.note.GNU-stack,"",@progbits
