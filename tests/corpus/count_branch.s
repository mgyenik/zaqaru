/* `jrcxz` and `jecxz` — conditional branches that test no flag.
 *
 * Every other conditional jump asks about a flag, and iced answers with a
 * condition code. These two ask whether the count register is zero, so they
 * have no condition code at all and cannot go through the same table. They
 * arrive from real code the way everything here does: glibc's string
 * routines use `jrcxz` to skip a `rep` whose count is zero.
 *
 * rdi is the count. Each function returns a different value on each side of
 * the branch, so taking the wrong one is a wrong answer rather than a
 * plausible one.
 */

    .text

    .globl  branch_if_count_zero
    .type   branch_if_count_zero, @function
branch_if_count_zero:
    movq    %rdi, %rcx
    jrcxz   .Lzero
    movq    $100, %rax
    ret
.Lzero:
    movq    $200, %rax
    ret
    .size   branch_if_count_zero, .-branch_if_count_zero

/* The 32-bit form, which tests `%ecx` — so a count whose low half is zero
   branches even when the whole register is not. */
    .globl  branch_if_low_count_zero
    .type   branch_if_low_count_zero, @function
branch_if_low_count_zero:
    movq    %rdi, %rcx
    jecxz   .Lzero32
    movq    $100, %rax
    ret
.Lzero32:
    movq    $200, %rax
    ret
    .size   branch_if_low_count_zero, .-branch_if_low_count_zero

/* In the shape it actually appears in: guarding a `rep` whose count may be
   zero, where taking the branch the wrong way either skips a real copy or
   runs one that should not happen. */
    .globl  guarded_fill
    .type   guarded_fill, @function
guarded_fill:
    subq    $64, %rsp
    movq    $0, 0(%rsp)
    movq    $0, 8(%rsp)
    movq    $0, 16(%rsp)
    movq    $0, 24(%rsp)
    movq    %rdi, %rcx
    movq    $0x1111111111111111, %rax
    jrcxz   .Ldone
    movq    %rsp, %rdi
    cld
    rep stosq
.Ldone:
    movq    0(%rsp), %rax
    addq    8(%rsp), %rax
    addq    16(%rsp), %rax
    addq    24(%rsp), %rax
    addq    $64, %rsp
    ret
    .size   guarded_fill, .-guarded_fill

    .section .note.GNU-stack,"",@progbits
