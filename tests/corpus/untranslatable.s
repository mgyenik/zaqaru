/* A program with one function nobody can translate.
 *
 * `hlt` halts the processor and is privileged; nothing a container runs will
 * ever legitimately execute one, which makes it a stable stand-in here for
 * whatever the gap list happens to hold this week. The point is not the
 * instruction — it is that one function is untranslatable and the rest of
 * the program is not, so what a translation does with the first can be
 * checked without the second going missing.
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
	hlt
	ret
	.size unreachable_path, .-unreachable_path
