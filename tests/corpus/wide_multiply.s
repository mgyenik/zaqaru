/* The one-operand `mul` and `imul`, at every width, in both signednesses.
 *
 * Compilers reach for exactly one member of this family: the 64-bit signed
 * form, emitted as the high-half multiply behind a division by a constant.
 * Covering the family therefore has to be deliberate, which is the lesson the
 * packed-operation corpus learned the expensive way — a translation written
 * for a family but exercised on one member is an untested translation.
 *
 * Each width appears three times: the low half of the product, the high half,
 * and the flags. Splitting them is what makes a failure say *which* half is
 * wrong, and the flags need their own function because carry and overflow are
 * the only two the instruction defines — every other flag is architecturally
 * undefined here, so comparing one would be testing the processor rather than
 * the translation.
 *
 * Arguments arrive in rdi and rsi; the accumulator is loaded from rdi so that
 * the operand register is never also the destination.
 */

    .text

/* ---------------------------------------------------------------- byte ---
 * The width that breaks the pattern: the whole 16-bit product lands in ax,
 * with no data register involved, so the high half is ah rather than dl.
 */

    .globl  mul_byte_low
    .type   mul_byte_low, @function
mul_byte_low:
    movl    $0, %eax
    movb    %dil, %al
    mulb    %sil
    movzbl  %al, %eax
    ret
    .size   mul_byte_low, .-mul_byte_low

    .globl  mul_byte_high
    .type   mul_byte_high, @function
mul_byte_high:
    movl    $0, %eax
    movb    %dil, %al
    mulb    %sil
    movzbl  %ah, %eax
    ret
    .size   mul_byte_high, .-mul_byte_high

    .globl  mul_byte_flags
    .type   mul_byte_flags, @function
mul_byte_flags:
    movl    $0, %eax
    movb    %dil, %al
    mulb    %sil
    setc    %al
    seto    %cl
    movzbl  %al, %eax
    movzbl  %cl, %ecx
    addl    %ecx, %ecx
    orl     %ecx, %eax
    ret
    .size   mul_byte_flags, .-mul_byte_flags

    .globl  imul_byte_low
    .type   imul_byte_low, @function
imul_byte_low:
    movl    $0, %eax
    movb    %dil, %al
    imulb   %sil
    movzbl  %al, %eax
    ret
    .size   imul_byte_low, .-imul_byte_low

    .globl  imul_byte_high
    .type   imul_byte_high, @function
imul_byte_high:
    movl    $0, %eax
    movb    %dil, %al
    imulb   %sil
    movzbl  %ah, %eax
    ret
    .size   imul_byte_high, .-imul_byte_high

    .globl  imul_byte_flags
    .type   imul_byte_flags, @function
imul_byte_flags:
    movl    $0, %eax
    movb    %dil, %al
    imulb   %sil
    setc    %al
    seto    %cl
    movzbl  %al, %eax
    movzbl  %cl, %ecx
    addl    %ecx, %ecx
    orl     %ecx, %eax
    ret
    .size   imul_byte_flags, .-imul_byte_flags

/* ---------------------------------------------------------------- word --- */

    .globl  mul_word_low
    .type   mul_word_low, @function
mul_word_low:
    movl    $0, %eax
    movw    %di, %ax
    mulw    %si
    movzwl  %ax, %eax
    ret
    .size   mul_word_low, .-mul_word_low

    .globl  mul_word_high
    .type   mul_word_high, @function
mul_word_high:
    movl    $0, %eax
    movw    %di, %ax
    mulw    %si
    movzwl  %dx, %eax
    ret
    .size   mul_word_high, .-mul_word_high

    .globl  mul_word_flags
    .type   mul_word_flags, @function
mul_word_flags:
    movl    $0, %eax
    movw    %di, %ax
    mulw    %si
    setc    %al
    seto    %cl
    movzbl  %al, %eax
    movzbl  %cl, %ecx
    addl    %ecx, %ecx
    orl     %ecx, %eax
    ret
    .size   mul_word_flags, .-mul_word_flags

    .globl  imul_word_low
    .type   imul_word_low, @function
imul_word_low:
    movl    $0, %eax
    movw    %di, %ax
    imulw   %si
    movzwl  %ax, %eax
    ret
    .size   imul_word_low, .-imul_word_low

    .globl  imul_word_high
    .type   imul_word_high, @function
imul_word_high:
    movl    $0, %eax
    movw    %di, %ax
    imulw   %si
    movzwl  %dx, %eax
    ret
    .size   imul_word_high, .-imul_word_high

    .globl  imul_word_flags
    .type   imul_word_flags, @function
imul_word_flags:
    movl    $0, %eax
    movw    %di, %ax
    imulw   %si
    setc    %al
    seto    %cl
    movzbl  %al, %eax
    movzbl  %cl, %ecx
    addl    %ecx, %ecx
    orl     %ecx, %eax
    ret
    .size   imul_word_flags, .-imul_word_flags

/* --------------------------------------------------------- double word ---
 * Writing eax and edx here also has to zero the upper halves of rax and rdx,
 * which is the ordinary 32-bit-destination rule and easy to lose in a
 * translation that writes register pairs.
 */

    .globl  mul_dword_low
    .type   mul_dword_low, @function
mul_dword_low:
    movl    %edi, %eax
    mull    %esi
    movl    %eax, %eax
    ret
    .size   mul_dword_low, .-mul_dword_low

    .globl  mul_dword_high
    .type   mul_dword_high, @function
mul_dword_high:
    movl    %edi, %eax
    mull    %esi
    movl    %edx, %eax
    ret
    .size   mul_dword_high, .-mul_dword_high

    .globl  mul_dword_flags
    .type   mul_dword_flags, @function
mul_dword_flags:
    movl    %edi, %eax
    mull    %esi
    setc    %al
    seto    %cl
    movzbl  %al, %eax
    movzbl  %cl, %ecx
    addl    %ecx, %ecx
    orl     %ecx, %eax
    ret
    .size   mul_dword_flags, .-mul_dword_flags

    .globl  imul_dword_low
    .type   imul_dword_low, @function
imul_dword_low:
    movl    %edi, %eax
    imull   %esi
    movl    %eax, %eax
    ret
    .size   imul_dword_low, .-imul_dword_low

    .globl  imul_dword_high
    .type   imul_dword_high, @function
imul_dword_high:
    movl    %edi, %eax
    imull   %esi
    movl    %edx, %eax
    ret
    .size   imul_dword_high, .-imul_dword_high

    .globl  imul_dword_flags
    .type   imul_dword_flags, @function
imul_dword_flags:
    movl    %edi, %eax
    imull   %esi
    setc    %al
    seto    %cl
    movzbl  %al, %eax
    movzbl  %cl, %ecx
    addl    %ecx, %ecx
    orl     %ecx, %eax
    ret
    .size   imul_dword_flags, .-imul_dword_flags

/* ----------------------------------------------------------- quad word ---
 * The only width whose product does not fit in an i64, so the only one whose
 * high half comes from partial-product arithmetic rather than a shift.
 */

    .globl  mul_qword_low
    .type   mul_qword_low, @function
mul_qword_low:
    movq    %rdi, %rax
    mulq    %rsi
    ret
    .size   mul_qword_low, .-mul_qword_low

    .globl  mul_qword_high
    .type   mul_qword_high, @function
mul_qword_high:
    movq    %rdi, %rax
    mulq    %rsi
    movq    %rdx, %rax
    ret
    .size   mul_qword_high, .-mul_qword_high

    .globl  mul_qword_flags
    .type   mul_qword_flags, @function
mul_qword_flags:
    movq    %rdi, %rax
    mulq    %rsi
    setc    %al
    seto    %cl
    movzbl  %al, %eax
    movzbl  %cl, %ecx
    addl    %ecx, %ecx
    orl     %ecx, %eax
    ret
    .size   mul_qword_flags, .-mul_qword_flags

    .globl  imul_qword_low
    .type   imul_qword_low, @function
imul_qword_low:
    movq    %rdi, %rax
    imulq   %rsi
    ret
    .size   imul_qword_low, .-imul_qword_low

    .globl  imul_qword_high
    .type   imul_qword_high, @function
imul_qword_high:
    movq    %rdi, %rax
    imulq   %rsi
    movq    %rdx, %rax
    ret
    .size   imul_qword_high, .-imul_qword_high

    .globl  imul_qword_flags
    .type   imul_qword_flags, @function
imul_qword_flags:
    movq    %rdi, %rax
    imulq   %rsi
    setc    %al
    seto    %cl
    movzbl  %al, %eax
    movzbl  %cl, %ecx
    addl    %ecx, %ecx
    orl     %ecx, %eax
    ret
    .size   imul_qword_flags, .-imul_qword_flags

    .section .note.GNU-stack,"",@progbits
