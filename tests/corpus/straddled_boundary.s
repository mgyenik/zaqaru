/* A branch target inside what a linear sweep decodes as one instruction.
 *
 * The bytes after a call that never returns are not instructions — nothing
 * reaches them — but a decoder does not know that and decodes them anyway,
 * and what it produces can span the place a real instruction begins. glibc's
 * `____longjmp_chk` is the case this was found in: two branches target one
 * offset, and the padding after its `call __fortify_fail` decodes into an
 * instruction that runs straight through it. Swept linearly, that offset is
 * not an instruction boundary and the whole function is refused.
 *
 * Reproduced here with `.byte` because no assembler will emit it from
 * mnemonics: the branch target is a local label reached only by the branch, and the two bytes in
 * front of it are the tail of a five-byte `add $imm32, %eax` that a sweep
 * starting one byte earlier would swallow it into.
 */

	.text

	.globl	straddled_boundary
	.type	straddled_boundary, @function
straddled_boundary:
	testq	%rdi, %rdi
	jne	.Lchosen
	call	never_returns
	/* Unreachable padding. `05 05 05 05 05` is `add $0x5050505,%eax`,
	   whose last two bytes are where the branch target begins. */
	.byte	0x05, 0x05, 0x05
.Lchosen:
	movq	%rsi, %rax
	addq	$7, %rax
	ret
	.size	straddled_boundary, .-straddled_boundary

	.globl	never_returns
	.type	never_returns, @function
never_returns:
	/* Not actually noreturn — the corpus links no `abort`, and what is
	   under test is the decode rather than the call. Nothing reaches it. */
	movq	$0, %rax
	ret
	.size	never_returns, .-never_returns

	.section	.note.GNU-stack,"",@progbits
