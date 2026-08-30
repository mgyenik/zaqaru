/* Data inside `.text`, in the two shapes hand-written assembly puts it.
 *
 * A compiler does not do this — gcc and clang put constants in `.rodata` —
 * but assembly written by hand does it as a matter of course, and the
 * libraries a real image carries are full of it. Both cases here are taken
 * from `libcrypto.so.3`, which is what found them:
 *
 * - **A table the code takes the address of.** OpenSSL's AES keeps its Te0
 *   S-box in `.text`, sixteen-byte aligned, immediately after
 *   `AES_cbc_encrypt` — and something naturally takes its address, because
 *   it is a lookup table. The address-taken witness then mints a function
 *   out of `c6 63 63 a5 f8 7c 7c 84 …`, whose extent is a *guess* bounded by
 *   whatever begins next, and whose "code" stops decoding four bytes in.
 *
 * - **A constant pool inside a symbol's own stated size.** `RC4_options` is
 *   208 bytes by its `.size`, of which the last 64 are the strings it
 *   returns: `ret`, alignment padding, then `"rc4(8x,int)"`. perlasm writes
 *   `.size name,.-name` after the pool, so a *stated* extent deliberately
 *   spans data.
 *
 * Both used to refuse the whole bake with "undecodable bytes". Neither is a
 * decode that lost sync: the extent says where a function's *bytes* end and
 * the decode says where its *code* ends, and where they disagree the decode
 * is right. What is kept is everything that decoded; what is dropped never
 * decoded and so could never have run.
 *
 * The bytes below are Te0's own first entries, which is what makes the first
 * case a reproduction rather than an imitation — they are what stops the
 * decoder, at the offset they stop it at.
 */

	.text

/* long lookup(long index) — reads the table by the address it takes, which
 * is what makes the table address-taken and therefore a function candidate. */
	.globl	lookup
	.type	lookup, @function
lookup:
	leaq	sbox(%rip), %rdx
	movl	(%rdx,%rdi,4), %eax
	ret
	.size	lookup, .-lookup

	.align	16
/* Not a function, and nothing says so. Four bytes in, it stops decoding. */
sbox:
	/* `xor %rax,%rax`, then `0x06` — `push %es`, which does not exist in
	 * 64-bit mode. The decode continues out of the first into the second
	 * and stops three bytes in. Chosen rather than copied: the real Te0
	 * stops the decoder too, but *where* depends on which boundaries the
	 * fixpoint found in the surrounding garbage, and a fixture should stop
	 * it somewhere a reader can see. It must not begin with a `nop`
	 * either — padding is never a function, and that filter would refuse
	 * the candidate before this one is reached. */
	.byte	0x48, 0x31, 0xc0, 0x06
	.byte	0xf8, 0x7c, 0x7c, 0x84, 0xee, 0x77, 0x77, 0x99
	.byte	0xf6, 0x7b, 0x7b, 0x8d, 0xff, 0xf2, 0xf2, 0x0d
	.byte	0xd6, 0x6b, 0x6b, 0xbd, 0xde, 0x6f, 0x6f, 0xb1

/* long options(void) — returns the address of a string that lives inside
 * this function's own stated size, exactly as `RC4_options` does. */
	.globl	options
	.type	options, @function
options:
	leaq	.Ltext(%rip), %rax
	ret
	.align	16
	/* A binary constant first, which is what a pool holds as often as it
	 * holds strings — and the same stopping pair, so that the decode
	 * arrives here off the alignment padding and stops three bytes in.
	 * Before the string rather than after it, because where a decode
	 * *through* a string stops is a property of the string. */
	.byte	0x48, 0x31, 0xc0, 0x06, 0x11, 0x22, 0x33, 0x44
.Ltext:
	.asciz	"rc4(8x,int)"
	.size	options, .-options

	.globl	_start
	.type	_start, @function
_start:
	xorl	%ebx, %ebx
	xorl	%r12d, %r12d
.Lloop:
	movq	%r12, %rdi
	call	lookup
	addq	%rax, %rbx
	incq	%r12
	cmpq	$4, %r12
	jb	.Lloop

	/* The string the pool holds, written out — so the bytes the decode
	 * walked through are also bytes the program reads. */
	call	options
	movq	%rax, %rsi
	movl	$1, %eax
	movl	$1, %edi
	movl	$11, %edx
	syscall

	/* And a checksum of the table, so a truncation that lost the table's
	 * bytes rather than only its translation would show. */
	movl	$231, %eax
	movq	%rbx, %rdi
	andq	$0xff, %rdi
	syscall
	.size	_start, .-_start

	.section .note.GNU-stack,"",@progbits
