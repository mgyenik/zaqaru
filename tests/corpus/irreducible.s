/* Milestone 6: a control-flow graph no `block`/`loop` nesting can express.
 *
 * The loop made of A and B has two entry points — the selector decides which
 * one control arrives at — and neither block dominates the other, so the
 * graph is irreducible. Compilers essentially never emit this from C, which
 * is why it is written by hand: it is the case the structured translation
 * must detect and hand to the dispatcher rather than mistranslate.
 *
 *        entry
 *        /   \
 *       A <-> B
 *        \   /
 *         done
 *
 * int irreducible(int selector, int count);
 */

	.text
	.globl	irreducible
	.type	irreducible, @function
irreducible:
	movl	$0, %eax
	testl	%edi, %edi
	je	.Lblock_b
.Lblock_a:
	addl	$1, %eax
	subl	$1, %esi
	cmpl	$0, %esi
	jle	.Ldone
.Lblock_b:
	addl	$10, %eax
	subl	$1, %esi
	cmpl	$0, %esi
	jg	.Lblock_a
.Ldone:
	ret
	.size	irreducible, .-irreducible
	.section	.note.GNU-stack,"",@progbits
