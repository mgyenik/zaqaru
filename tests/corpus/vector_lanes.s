/* Float-plan milestone 5: the packed operations one lane width at a time.
 *
 * Compiler output reaches a handful of these — whichever the vectoriser
 * happened to want — and leaves the rest of each family untested, which is
 * exactly where a wrong lane width or a wrong shift direction hides: a
 * translation that added sixty-four-bit lanes where the instruction says
 * thirty-two still passes every test that never uses it. So every member of
 * every family implemented has a case here, on purpose, in the way the
 * write-mask and irreducible-graph cases are written by hand.
 *
 * Two shapes, because two kinds of answer need comparing differently:
 *
 *   `long name(long low, long high)` folds all 128 bits of the result into
 *   one number, which is exact — used for the integer families, the compare
 *   masks and the sign-mask gathers, none of which can produce a value whose
 *   bits are the engine's choice.
 *
 *   `double name(long low, long high, int lane)` hands back one lane, because
 *   packed floating-point arithmetic can *generate* a NaN and the wasm
 *   specification lets an engine pick its payload — so those are compared as
 *   a class, which folding into an integer would make impossible.
 *
 * In both, `xmm0` starts as (low, high) and `xmm1` as (high, low), so the two
 * registers differ in both halves.
 */

	.text

/* The same red-zone layout as the write-mask cases, for the same reason: at
 * function entry `rsp - 8` is the first sixteen-byte-aligned address below
 * the stack pointer, and the aligned movers fault on the machine otherwise. */
	.set	PATTERN, -56
	.set	SWAPPED, -40
	.set	RESULT, -24

	.macro	lane_prologue
	movq	%rdi, PATTERN(%rsp)
	movq	%rsi, PATTERN+8(%rsp)
	movq	%rsi, SWAPPED(%rsp)
	movq	%rdi, SWAPPED+8(%rsp)
	movdqu	PATTERN(%rsp), %xmm0
	movdqu	SWAPPED(%rsp), %xmm1
	.endm

/* An exact case: the whole result folded into one integer. */
	.macro	lane_case name, body:vararg
	.globl	\name
	.type	\name, @function
\name:
	lane_prologue
	\body
	movdqu	%xmm0, RESULT(%rsp)
	movq	RESULT(%rsp), %rax
	imulq	$31, %rax, %rax
	addq	RESULT+8(%rsp), %rax
	ret
	.size	\name, .-\name
	.endm

/* A double-precision case, returning the lane the third argument selects. */
	.macro	double_lane_case name, body:vararg
	.globl	\name
	.type	\name, @function
\name:
	lane_prologue
	\body
	movupd	%xmm0, RESULT(%rsp)
	movl	%edx, %eax
	andl	$1, %eax
	cltq
	movsd	RESULT(%rsp,%rax,8), %xmm0
	ret
	.size	\name, .-\name
	.endm

/* The single-precision counterpart, with four lanes to choose from. */
	.macro	single_lane_case name, body:vararg
	.globl	\name
	.type	\name, @function
\name:
	lane_prologue
	\body
	movups	%xmm0, RESULT(%rsp)
	movl	%edx, %eax
	andl	$3, %eax
	cltq
	movss	RESULT(%rsp,%rax,4), %xmm0
	ret
	.size	\name, .-\name
	.endm

/* ---- packed integer arithmetic, every lane width ------------------------- */

	lane_case	lane_paddb, paddb %xmm1, %xmm0
	lane_case	lane_paddw, paddw %xmm1, %xmm0
	lane_case	lane_paddd, paddd %xmm1, %xmm0
	lane_case	lane_paddq, paddq %xmm1, %xmm0
	lane_case	lane_paddd_memory, paddd SWAPPED(%rsp), %xmm0
	lane_case	lane_psubb, psubb %xmm1, %xmm0
	lane_case	lane_psubw, psubw %xmm1, %xmm0
	lane_case	lane_psubd, psubd %xmm1, %xmm0
	lane_case	lane_psubq, psubq %xmm1, %xmm0
	lane_case	lane_pmullw, pmullw %xmm1, %xmm0
	lane_case	lane_pmulld, pmulld %xmm1, %xmm0
	lane_case	lane_pmuludq, pmuludq %xmm1, %xmm0
	lane_case	lane_pmuludq_memory, pmuludq SWAPPED(%rsp), %xmm0

/* ---- packed comparisons -------------------------------------------------- */

	lane_case	lane_pcmpeqb, pcmpeqb %xmm1, %xmm0
	lane_case	lane_pcmpeqw, pcmpeqw %xmm1, %xmm0
	lane_case	lane_pcmpeqd, pcmpeqd %xmm1, %xmm0
	lane_case	lane_pcmpeqq, pcmpeqq %xmm1, %xmm0
	lane_case	lane_pcmpgtb, pcmpgtb %xmm1, %xmm0
	lane_case	lane_pcmpgtw, pcmpgtw %xmm1, %xmm0
	lane_case	lane_pcmpgtd, pcmpgtd %xmm1, %xmm0
	lane_case	lane_pcmpgtq, pcmpgtq %xmm1, %xmm0

/* ---- lane shifts, including counts that reach and pass the lane width ---- */

	lane_case	lane_psllw_3, psllw $3, %xmm0
	lane_case	lane_psllw_15, psllw $15, %xmm0
	lane_case	lane_psllw_16, psllw $16, %xmm0
	lane_case	lane_pslld_5, pslld $5, %xmm0
	lane_case	lane_pslld_31, pslld $31, %xmm0
	lane_case	lane_pslld_32, pslld $32, %xmm0
	lane_case	lane_psllq_9, psllq $9, %xmm0
	lane_case	lane_psllq_63, psllq $63, %xmm0
	lane_case	lane_psllq_64, psllq $64, %xmm0
	lane_case	lane_psrlw_3, psrlw $3, %xmm0
	lane_case	lane_psrlw_16, psrlw $16, %xmm0
	lane_case	lane_psrld_5, psrld $5, %xmm0
	lane_case	lane_psrld_32, psrld $32, %xmm0
	lane_case	lane_psrlq_9, psrlq $9, %xmm0
	lane_case	lane_psrlq_100, psrlq $100, %xmm0
	lane_case	lane_psraw_3, psraw $3, %xmm0
	lane_case	lane_psraw_20, psraw $20, %xmm0
	lane_case	lane_psrad_5, psrad $5, %xmm0
	lane_case	lane_psrad_31, psrad $31, %xmm0
	lane_case	lane_psrad_40, psrad $40, %xmm0

/* Whole-register shifts by bytes, where bits cross between the two halves of
 * the pair — the one place the representation costs something. */
	lane_case	lane_psrldq_0, psrldq $0, %xmm0
	lane_case	lane_psrldq_1, psrldq $1, %xmm0
	lane_case	lane_psrldq_4, psrldq $4, %xmm0
	lane_case	lane_psrldq_7, psrldq $7, %xmm0
	lane_case	lane_psrldq_8, psrldq $8, %xmm0
	lane_case	lane_psrldq_9, psrldq $9, %xmm0
	lane_case	lane_psrldq_15, psrldq $15, %xmm0
	lane_case	lane_psrldq_16, psrldq $16, %xmm0
	lane_case	lane_pslldq_1, pslldq $1, %xmm0
	lane_case	lane_pslldq_4, pslldq $4, %xmm0
	lane_case	lane_pslldq_7, pslldq $7, %xmm0
	lane_case	lane_pslldq_8, pslldq $8, %xmm0
	lane_case	lane_pslldq_9, pslldq $9, %xmm0
	lane_case	lane_pslldq_15, pslldq $15, %xmm0
	lane_case	lane_pslldq_16, pslldq $16, %xmm0

/* ---- compare masks, every predicate the legacy encoding can name --------- */

	.irp	predicate, 0, 1, 2, 3, 4, 5, 6, 7
	lane_case	lane_cmpsd_\predicate, cmpsd $\predicate, %xmm1, %xmm0
	lane_case	lane_cmpss_\predicate, cmpss $\predicate, %xmm1, %xmm0
	lane_case	lane_cmppd_\predicate, cmppd $\predicate, %xmm1, %xmm0
	lane_case	lane_cmpps_\predicate, cmpps $\predicate, %xmm1, %xmm0
	.endr

/* ---- sign masks ---------------------------------------------------------- */

	.globl	lane_movmskpd
	.type	lane_movmskpd, @function
lane_movmskpd:
	lane_prologue
	movmskpd	%xmm0, %eax
	ret
	.size	lane_movmskpd, .-lane_movmskpd

	.globl	lane_movmskps
	.type	lane_movmskps, @function
lane_movmskps:
	lane_prologue
	movmskps	%xmm0, %eax
	ret
	.size	lane_movmskps, .-lane_movmskps

/* The byte-grain gather, which is the one that matters: every SSE2 string
   function turns sixteen lanes of comparison into an integer this way, and
   it is by far the most-used vector instruction in a static libc. */
	.globl	lane_pmovmskb
	.type	lane_pmovmskb, @function
lane_pmovmskb:
	lane_prologue
	pmovmskb	%xmm0, %eax
	ret
	.size	lane_pmovmskb, .-lane_pmovmskb

/* And after a comparison, which is how it actually appears. */
	.globl	lane_pmovmskb_after_compare
	.type	lane_pmovmskb_after_compare, @function
lane_pmovmskb_after_compare:
	lane_prologue
	pcmpeqb		%xmm1, %xmm0
	pmovmskb	%xmm0, %eax
	ret
	.size	lane_pmovmskb_after_compare, .-lane_pmovmskb_after_compare

/* ---- lane-wise extrema --------------------------------------------------- */

/* x86 has these only where it happened to need them: unsigned bytes and
   signed words. An SSE2 `strlen` reaches for `pminub` every iteration. */
	lane_case	lane_pminub, pminub %xmm1, %xmm0
	lane_case	lane_pmaxub, pmaxub %xmm1, %xmm0
	lane_case	lane_pminsw, pminsw %xmm1, %xmm0
	lane_case	lane_pmaxsw, pmaxsw %xmm1, %xmm0

/* ---- interleaves at byte and word grain ---------------------------------- */

	lane_case	lane_punpcklbw, punpcklbw %xmm1, %xmm0
	lane_case	lane_punpckhbw, punpckhbw %xmm1, %xmm0
	lane_case	lane_punpcklwd, punpcklwd %xmm1, %xmm0
	lane_case	lane_punpckhwd, punpckhwd %xmm1, %xmm0

/* Interleaving a register with itself, which is how an SSE2 `memset`
   broadcasts one byte across a whole vector — the shape that actually
   appears in a libc. */
	.globl	lane_broadcast_byte
	.type	lane_broadcast_byte, @function
lane_broadcast_byte:
	lane_prologue
	punpcklbw	%xmm0, %xmm0
	punpcklwd	%xmm0, %xmm0
	movq		%xmm0, %rax
	ret
	.size	lane_broadcast_byte, .-lane_broadcast_byte

/* ---- packed floating-point arithmetic ------------------------------------ */

	double_lane_case	lane_addpd, addpd %xmm1, %xmm0
	double_lane_case	lane_addpd_memory, addpd SWAPPED(%rsp), %xmm0
	double_lane_case	lane_subpd, subpd %xmm1, %xmm0
	double_lane_case	lane_mulpd, mulpd %xmm1, %xmm0
	double_lane_case	lane_divpd, divpd %xmm1, %xmm0
	double_lane_case	lane_sqrtpd, sqrtpd %xmm1, %xmm0
	double_lane_case	lane_cvtdq2pd, cvtdq2pd %xmm1, %xmm0

	single_lane_case	lane_addps, addps %xmm1, %xmm0
	single_lane_case	lane_subps, subps %xmm1, %xmm0
	single_lane_case	lane_mulps, mulps %xmm1, %xmm0
	single_lane_case	lane_divps, divps %xmm1, %xmm0
	single_lane_case	lane_sqrtps, sqrtps %xmm1, %xmm0
	single_lane_case	lane_cvtdq2ps, cvtdq2ps %xmm1, %xmm0

	.section	.note.GNU-stack,"",@progbits
