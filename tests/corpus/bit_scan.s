/* `bsf`, `bsr` and `bswap` — finding a set bit, and reversing bytes.
 *
 * The scans matter because of what they do with nothing to find: a source of
 * zero has no lowest or highest set bit, and x86 says so by setting the zero
 * flag and leaving the destination *unchanged*. Not zero — unchanged, which
 * is a real distinction because the destination usually still holds
 * something. Every function here therefore preloads the destination with a
 * recognisable value, so a translation that wrote zero instead would show.
 *
 * rdi is the value to scan or swap; rsi is what the destination held first.
 */

    .text

.macro SCAN name, op, src, dst, out
    .globl  \name
    .type   \name, @function
\name:
    movq    %rsi, %rax
    \op     \src, \dst
    \out
    ret
    .size   \name, .-\name
.endm

/* The destination is the accumulator, preloaded from rsi. */
SCAN bsf_qword, bsfq, %rdi, %rax, ""
SCAN bsr_qword, bsrq, %rdi, %rax, ""
SCAN bsf_dword, bsfl, %edi, %eax, ""
SCAN bsr_dword, bsrl, %edi, %eax, ""
SCAN bsf_word,  bsfw, %di,  %ax,  "movzwl %ax, %eax"
SCAN bsr_word,  bsrw, %di,  %ax,  "movzwl %ax, %eax"

/* And the zero flag, which is the answer to "was there a bit at all". */
.macro SCAN_FLAG name, op, src, dst
    .globl  \name
    .type   \name, @function
\name:
    movq    %rsi, %rax
    \op     \src, \dst
    setz    %al
    movzbl  %al, %eax
    ret
    .size   \name, .-\name
.endm

SCAN_FLAG bsf_qword_zero, bsfq, %rdi, %rdx
SCAN_FLAG bsr_qword_zero, bsrq, %rdi, %rdx
SCAN_FLAG bsf_dword_zero, bsfl, %edi, %edx

/* --------------------------------------------------------- bswap ------- */

    .globl  bswap_qword
    .type   bswap_qword, @function
bswap_qword:
    movq    %rdi, %rax
    bswapq  %rax
    ret
    .size   bswap_qword, .-bswap_qword

    .globl  bswap_dword
    .type   bswap_dword, @function
bswap_dword:
    movl    %edi, %eax
    bswapl  %eax
    ret
    .size   bswap_dword, .-bswap_dword

/* A 32-bit swap clears the register's upper half, which a 64-bit
   implementation of it would preserve. */
    .globl  bswap_dword_upper
    .type   bswap_dword_upper, @function
bswap_dword_upper:
    movq    %rdi, %rax
    bswapl  %eax
    ret
    .size   bswap_dword_upper, .-bswap_dword_upper

/* --------------------------------------------------------- rdsspq ------ */

/* The idiom a libc uses to ask for the shadow-stack pointer. With the
   shadow stack off — which it is, since nothing here implements control-flow
   enforcement — the instruction does nothing and the answer is the zero the
   `xor` just wrote. */
    .globl  shadow_stack_pointer
    .type   shadow_stack_pointer, @function
shadow_stack_pointer:
    xorq    %rax, %rax
    rdsspq  %rax
    ret
    .size   shadow_stack_pointer, .-shadow_stack_pointer

/* --------------------------------------------------------- stmxcsr ----- */

    .globl  sse_control_register
    .type   sse_control_register, @function
sse_control_register:
    stmxcsr -8(%rsp)
    movl    -8(%rsp), %eax
    ret
    .size   sse_control_register, .-sse_control_register

    .section .note.GNU-stack,"",@progbits
