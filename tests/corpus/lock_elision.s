/* Branching past a `lock` prefix — two instruction streams sharing bytes.
 *
 * glibc's allocator does this to avoid a locked read-modify-write when the
 * process has no second thread: it tests the thread count and, if there is
 * only one, jumps *one byte into* the next instruction, past the `f0` that
 * makes it atomic. The same `cmpxchg` then runs unlocked.
 *
 * A linear decode sees `f0 48 0f b1 37` as one instruction and has nothing
 * at the byte after the prefix, so the branch lands in the middle of an
 * instruction — which is normally out of scope and here is not, because the
 * two streams differ only in a prefix this translation does not model.
 *
 * rdi holds the expected value, rsi the memory's current value, rdx the
 * replacement, and rcx chooses the path so that both are exercised from one
 * caller.
 */

    .text

    .globl  elided_compare_and_swap
    .type   elided_compare_and_swap, @function
elided_compare_and_swap:
    movq    %rsi, -8(%rsp)          /* the memory operand */
    leaq    -8(%rsp), %r8
    movq    %rdi, %rax              /* the accumulator: what we expect */
    testq   %rcx, %rcx
    je      .Lunlocked
    .byte   0xf0                    /* lock */
.Lunlocked:
    cmpxchgq %rdx, (%r8)
    /* The memory afterwards, and the accumulator, and the flag the loop
       would branch on — everything the instruction defines. */
    setz    %r9b
    movzbl  %r9b, %r9d
    addq    (%r8), %rax
    addq    %r9, %rax
    ret
    .size   elided_compare_and_swap, .-elided_compare_and_swap

/* The same shape around `xchg`, which carries an implicit lock and so is
   written with an explicit one here to make the byte real. */
    .globl  elided_exchange
    .type   elided_exchange, @function
elided_exchange:
    movq    %rsi, -8(%rsp)
    leaq    -8(%rsp), %r8
    testq   %rcx, %rcx
    je      .Lunlocked_add
    .byte   0xf0
.Lunlocked_add:
    xaddq   %rdi, (%r8)
    movq    (%r8), %rax
    addq    %rdi, %rax
    ret
    .size   elided_exchange, .-elided_exchange

    .section .note.GNU-stack,"",@progbits
