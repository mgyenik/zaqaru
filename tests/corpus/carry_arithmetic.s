/* `adc` and `sbb` at every width, with the carry flag both set and clear.
 *
 * These arrived the way the loud-error design intends: clang compiles the
 * mutual recursion in signatures.c into an `adc` against a flag a comparison
 * had already set, and the transpile sweep said so. Compilers reach for them
 * well outside the multi-word arithmetic they are named for.
 *
 * The incoming carry comes from rdx, through `neg`, which sets the carry flag
 * exactly when its operand is nonzero — so a caller chooses the carry by
 * passing zero or not. `mov` does not disturb flags, so the value can be
 * staged between setting the carry and consuming it.
 *
 * Each width appears twice: the result, and all five flags the operation
 * defines. The flags need their own function because the carry *out* of an
 * `adc` is not the ordinary rule — a sum landing exactly on its left operand
 * has wrapped if something was carried in and has not if nothing was — and a
 * test that only compared results would not see the difference.
 */

    .text

/* Sets the carry flag from rdx and leaves every other input alone. */
.macro CARRY_FROM_RDX
    movl    %edx, %r8d
    negl    %r8d
.endm

/* Packs the five defined flags into one integer, in a fixed order. */
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

/* ---------------------------------------------------------------- byte --- */

    .globl  adc_byte
    .type   adc_byte, @function
adc_byte:
    CARRY_FROM_RDX
    adcb    %sil, %dil
    movzbl  %dil, %eax
    ret
    .size   adc_byte, .-adc_byte

    .globl  adc_byte_flags
    .type   adc_byte_flags, @function
adc_byte_flags:
    CARRY_FROM_RDX
    adcb    %sil, %dil
    PACK_FLAGS
    ret
    .size   adc_byte_flags, .-adc_byte_flags

    .globl  sbb_byte
    .type   sbb_byte, @function
sbb_byte:
    CARRY_FROM_RDX
    sbbb    %sil, %dil
    movzbl  %dil, %eax
    ret
    .size   sbb_byte, .-sbb_byte

    .globl  sbb_byte_flags
    .type   sbb_byte_flags, @function
sbb_byte_flags:
    CARRY_FROM_RDX
    sbbb    %sil, %dil
    PACK_FLAGS
    ret
    .size   sbb_byte_flags, .-sbb_byte_flags

/* ---------------------------------------------------------------- word --- */

    .globl  adc_word
    .type   adc_word, @function
adc_word:
    CARRY_FROM_RDX
    adcw    %si, %di
    movzwl  %di, %eax
    ret
    .size   adc_word, .-adc_word

    .globl  adc_word_flags
    .type   adc_word_flags, @function
adc_word_flags:
    CARRY_FROM_RDX
    adcw    %si, %di
    PACK_FLAGS
    ret
    .size   adc_word_flags, .-adc_word_flags

    .globl  sbb_word
    .type   sbb_word, @function
sbb_word:
    CARRY_FROM_RDX
    sbbw    %si, %di
    movzwl  %di, %eax
    ret
    .size   sbb_word, .-sbb_word

    .globl  sbb_word_flags
    .type   sbb_word_flags, @function
sbb_word_flags:
    CARRY_FROM_RDX
    sbbw    %si, %di
    PACK_FLAGS
    ret
    .size   sbb_word_flags, .-sbb_word_flags

/* --------------------------------------------------------- double word --- */

    .globl  adc_dword
    .type   adc_dword, @function
adc_dword:
    CARRY_FROM_RDX
    adcl    %esi, %edi
    movl    %edi, %eax
    ret
    .size   adc_dword, .-adc_dword

    .globl  adc_dword_flags
    .type   adc_dword_flags, @function
adc_dword_flags:
    CARRY_FROM_RDX
    adcl    %esi, %edi
    PACK_FLAGS
    ret
    .size   adc_dword_flags, .-adc_dword_flags

    .globl  sbb_dword
    .type   sbb_dword, @function
sbb_dword:
    CARRY_FROM_RDX
    sbbl    %esi, %edi
    movl    %edi, %eax
    ret
    .size   sbb_dword, .-sbb_dword

    .globl  sbb_dword_flags
    .type   sbb_dword_flags, @function
sbb_dword_flags:
    CARRY_FROM_RDX
    sbbl    %esi, %edi
    PACK_FLAGS
    ret
    .size   sbb_dword_flags, .-sbb_dword_flags

/* ----------------------------------------------------------- quad word --- */

    .globl  adc_qword
    .type   adc_qword, @function
adc_qword:
    CARRY_FROM_RDX
    adcq    %rsi, %rdi
    movq    %rdi, %rax
    ret
    .size   adc_qword, .-adc_qword

    .globl  adc_qword_flags
    .type   adc_qword_flags, @function
adc_qword_flags:
    CARRY_FROM_RDX
    adcq    %rsi, %rdi
    PACK_FLAGS
    ret
    .size   adc_qword_flags, .-adc_qword_flags

    .globl  sbb_qword
    .type   sbb_qword, @function
sbb_qword:
    CARRY_FROM_RDX
    sbbq    %rsi, %rdi
    movq    %rdi, %rax
    ret
    .size   sbb_qword, .-sbb_qword

    .globl  sbb_qword_flags
    .type   sbb_qword_flags, @function
sbb_qword_flags:
    CARRY_FROM_RDX
    sbbq    %rsi, %rdi
    PACK_FLAGS
    ret
    .size   sbb_qword_flags, .-sbb_qword_flags

    .section .note.GNU-stack,"",@progbits
