/* What the machine says it is.
 *
 * `cpuid` is the one instruction whose answer must *not* match native: a
 * libc picks its `memcpy` from what this reports, so reporting the host's
 * processor would make a container behave differently on different machines
 * and would select code paths written in instructions nothing translates.
 * The container reports one fixed processor instead, and these functions
 * read the fields that decide those choices.
 *
 * rdi selects the leaf where the function takes one.
 */

    .text

.macro CPUID_FIELD name, out
    .globl  \name
    .type   \name, @function
\name:
    pushq   %rbx
    movl    %edi, %eax
    xorl    %ecx, %ecx
    cpuid
    \out
    popq    %rbx
    ret
    .size   \name, .-\name
.endm

CPUID_FIELD cpuid_eax, "movl %eax, %eax"
CPUID_FIELD cpuid_ebx, "movl %ebx, %eax"
CPUID_FIELD cpuid_ecx, "movl %ecx, %eax"
CPUID_FIELD cpuid_edx, "movl %edx, %eax"

/* The extended control register, which says whether the AVX state is live. */
    .globl  extended_control
    .type   extended_control, @function
extended_control:
    xorl    %ecx, %ecx
    xgetbv
    movl    %eax, %eax
    ret
    .size   extended_control, .-extended_control

    .section .note.GNU-stack,"",@progbits
