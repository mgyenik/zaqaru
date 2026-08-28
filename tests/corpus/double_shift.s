/* `shld` and `shrd` — shifting one register while filling from another.
 *
 * The vacated bits come from the *source* rather than from zero or the sign,
 * which is how a multi-word shift carries bits across a word boundary and
 * how a hash mixes one register into another. The source is not written.
 *
 * A count of zero must leave everything alone, flags included, exactly as a
 * plain shift does — and a count equal to the operand's width is where a
 * naive implementation puts the filler's bits where the destination's
 * belong, because a shift by the whole width is masked back to a shift by
 * nothing.
 *
 * rdi is the destination's value, rsi the filler's, rdx the count.
 */

    .text

.macro PACK_FLAGS
    setc    %r8b
    seto    %r9b
    sets    %r10b
    setz    %r11b
    setp    %cl
    movzbl  %r8b, %eax
    movzbl  %r9b, %r9d
    shll    $1, %r9d
    orl     %r9d, %eax
    movzbl  %r10b, %r10d
    shll    $2, %r10d
    orl     %r10d, %eax
    movzbl  %r11b, %r11d
    shll    $3, %r11d
    orl     %r11d, %eax
    movzbl  %cl, %ecx
    shll    $4, %ecx
    orl     %ecx, %eax
.endm

.macro DOUBLE_SHIFT name, op, suffix, dst, src, out
    .globl  \name
    .type   \name, @function
\name:
    movl    %edx, %ecx
    \op\suffix %cl, \src, \dst
    \out
    ret
    .size   \name, .-\name
.endm

DOUBLE_SHIFT shld_qword, shld, q, %rdi, %rsi, "movq %rdi, %rax"
DOUBLE_SHIFT shrd_qword, shrd, q, %rdi, %rsi, "movq %rdi, %rax"
DOUBLE_SHIFT shld_dword, shld, l, %edi, %esi, "movq %rdi, %rax"
DOUBLE_SHIFT shrd_dword, shrd, l, %edi, %esi, "movq %rdi, %rax"
DOUBLE_SHIFT shld_word,  shld, w, %di,  %si,  "movzwl %di, %eax"
DOUBLE_SHIFT shrd_word,  shrd, w, %di,  %si,  "movzwl %di, %eax"

/* The source must come back unchanged. */
    .globl  shld_leaves_source
    .type   shld_leaves_source, @function
shld_leaves_source:
    movl    %edx, %ecx
    shldq   %cl, %rsi, %rdi
    movq    %rsi, %rax
    ret
    .size   shld_leaves_source, .-shld_leaves_source

/* And the flags, which a count of zero must leave exactly as they were —
   here set by a comparison first, so an implementation that wrote them
   anyway shows up. */
.macro DOUBLE_SHIFT_FLAGS name, op, dst, src
    .globl  \name
    .type   \name, @function
\name:
    movl    %edx, %ecx
    cmpq    $0, %rdi
    \op     %cl, \src, \dst
    PACK_FLAGS
    ret
    .size   \name, .-\name
.endm

DOUBLE_SHIFT_FLAGS shld_qword_flags, shldq, %rdi, %rsi
DOUBLE_SHIFT_FLAGS shrd_qword_flags, shrdq, %rdi, %rsi

/* The immediate-count encoding. */
    .globl  shld_qword_immediate
    .type   shld_qword_immediate, @function
shld_qword_immediate:
    shldq   $17, %rsi, %rdi
    movq    %rdi, %rax
    ret
    .size   shld_qword_immediate, .-shld_qword_immediate

    .section .note.GNU-stack,"",@progbits
