/* A program with one function nobody can translate.
 *
 * `wrmsr` writes a model-specific register, which is a supervisor operation
 * a container can never meaningfully perform. The point is not the
 * instruction — it is that one function is untranslatable and the rest of
 * the program is not, so what a translation does with the first can be
 * checked without the second going missing.
 *
 * This was `hlt` until `hlt` was implemented, which is the hazard with a
 * stand-in: it stops standing in. The premise is asserted rather than
 * assumed, so the day `wrmsr` gains a translation the test says so instead
 * of passing quietly.
 */
	.text
	.globl entry
	.type entry,@function
entry:
	push %rbp
	call reachable
	call unreachable_path
	pop %rbp
	ret
	.size entry, .-entry

	.globl reachable
	.type reachable,@function
reachable:
	mov $42, %eax
	ret
	.size reachable, .-reachable

	.globl unreachable_path
	.type unreachable_path,@function
unreachable_path:
	wrmsr
	ret
	.size unreachable_path, .-unreachable_path
