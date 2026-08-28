/* `xchg`, `xadd` and `cmpxchg` — the read-modify-write family every libc
 * locks with, and the reason half of a static glibc could not be translated.
 *
 * They are grouped because they are one shape: read the destination, compute
 * something, write one or both operands back. What separates them is which
 * operand receives what, and which flags survive — `xchg` writes none at
 * all, `xadd` writes an addition's, and `cmpxchg` writes a comparison's
 * against the accumulator, which is what every compare-and-swap loop
 * branches on.
 *
 * With a memory operand all three are atomic on hardware whether or not a
 * `lock` prefix is present. Nothing here reproduces that and nothing has to
 * while threads switch only at syscalls; the memory forms are here because
 * the *addressing* differs from the register forms, not because the
 * atomicity does.
 *
 * Arguments arrive in rdi, rsi, rdx, and each function returns the one thing
 * it is about, so a disagreement names which half is wrong.
 */

    .text

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

/* ------------------------------------------------------------- xchg --- */

/* The swap happened: return the value that ended up in the first operand. */
    .globl  xchg_qword
    .type   xchg_qword, @function
xchg_qword:
    xchgq   %rsi, %rdi
    movq    %rdi, %rax
    ret
    .size   xchg_qword, .-xchg_qword

/* And the value that ended up in the second, which a swap that wrote one
   operand twice would get wrong. */
    .globl  xchg_qword_other
    .type   xchg_qword_other, @function
xchg_qword_other:
    xchgq   %rsi, %rdi
    movq    %rsi, %rax
    ret
    .size   xchg_qword_other, .-xchg_qword_other

    .globl  xchg_dword
    .type   xchg_dword, @function
xchg_dword:
    xchgl   %esi, %edi
    movl    %edi, %eax
    ret
    .size   xchg_dword, .-xchg_dword

/* A 32-bit write clears the upper half of the destination register, which a
   swap implemented as two 64-bit moves would preserve. */
    .globl  xchg_dword_upper
    .type   xchg_dword_upper, @function
xchg_dword_upper:
    xchgl   %esi, %edi
    movq    %rdi, %rax
    ret
    .size   xchg_dword_upper, .-xchg_dword_upper

    .globl  xchg_byte
    .type   xchg_byte, @function
xchg_byte:
    xchgb   %sil, %dil
    movzbl  %dil, %eax
    ret
    .size   xchg_byte, .-xchg_byte

/* Through memory, which is the form a mutex release actually uses. */
    .globl  xchg_memory
    .type   xchg_memory, @function
xchg_memory:
    movq    %rdi, -8(%rsp)
    xchgq   -8(%rsp), %rsi
    movq    -8(%rsp), %rax
    addq    %rsi, %rax
    ret
    .size   xchg_memory, .-xchg_memory

/* Both operands naming the same register: the swap must leave it alone. */
    .globl  xchg_same
    .type   xchg_same, @function
xchg_same:
    xchgq   %rdi, %rdi
    movq    %rdi, %rax
    ret
    .size   xchg_same, .-xchg_same

/* ------------------------------------------------------------- xadd --- */

    .globl  xadd_qword_sum
    .type   xadd_qword_sum, @function
xadd_qword_sum:
    xaddq   %rsi, %rdi
    movq    %rdi, %rax
    ret
    .size   xadd_qword_sum, .-xadd_qword_sum

/* The source gets the destination's *old* value, which is the half that
   makes this an exchange rather than an add. */
    .globl  xadd_qword_old
    .type   xadd_qword_old, @function
xadd_qword_old:
    xaddq   %rsi, %rdi
    movq    %rsi, %rax
    ret
    .size   xadd_qword_old, .-xadd_qword_old

    .globl  xadd_qword_flags
    .type   xadd_qword_flags, @function
xadd_qword_flags:
    xaddq   %rsi, %rdi
    PACK_FLAGS
    ret
    .size   xadd_qword_flags, .-xadd_qword_flags

    .globl  xadd_dword_sum
    .type   xadd_dword_sum, @function
xadd_dword_sum:
    xaddl   %esi, %edi
    movq    %rdi, %rax
    ret
    .size   xadd_dword_sum, .-xadd_dword_sum

    .globl  xadd_byte_flags
    .type   xadd_byte_flags, @function
xadd_byte_flags:
    xaddb   %sil, %dil
    PACK_FLAGS
    ret
    .size   xadd_byte_flags, .-xadd_byte_flags

    .globl  xadd_memory
    .type   xadd_memory, @function
xadd_memory:
    movq    %rdi, -8(%rsp)
    xaddq   %rsi, -8(%rsp)
    movq    -8(%rsp), %rax
    addq    %rsi, %rax
    ret
    .size   xadd_memory, .-xadd_memory

/* --------------------------------------------------------- cmpxchg --- */

/* rdi is what the accumulator expects, rsi the destination's current value,
   rdx the replacement. Returns the destination afterwards. */
    .globl  cmpxchg_qword_destination
    .type   cmpxchg_qword_destination, @function
cmpxchg_qword_destination:
    movq    %rdi, %rax
    cmpxchgq %rdx, %rsi
    movq    %rsi, %rax
    ret
    .size   cmpxchg_qword_destination, .-cmpxchg_qword_destination

/* The accumulator afterwards, which is the failure path's whole output: a
   compare-and-swap that missed reports what it found. */
    .globl  cmpxchg_qword_accumulator
    .type   cmpxchg_qword_accumulator, @function
cmpxchg_qword_accumulator:
    movq    %rdi, %rax
    cmpxchgq %rdx, %rsi
    ret
    .size   cmpxchg_qword_accumulator, .-cmpxchg_qword_accumulator

/* And the flags, since `ZF` is what the loop branches on. */
    .globl  cmpxchg_qword_flags
    .type   cmpxchg_qword_flags, @function
cmpxchg_qword_flags:
    movq    %rdi, %rax
    cmpxchgq %rdx, %rsi
    PACK_FLAGS
    ret
    .size   cmpxchg_qword_flags, .-cmpxchg_qword_flags

    .globl  cmpxchg_dword_destination
    .type   cmpxchg_dword_destination, @function
cmpxchg_dword_destination:
    movl    %edi, %eax
    cmpxchgl %edx, %esi
    movq    %rsi, %rax
    ret
    .size   cmpxchg_dword_destination, .-cmpxchg_dword_destination

    .globl  cmpxchg_byte_flags
    .type   cmpxchg_byte_flags, @function
cmpxchg_byte_flags:
    movl    %edi, %eax
    cmpxchgb %dl, %sil
    PACK_FLAGS
    ret
    .size   cmpxchg_byte_flags, .-cmpxchg_byte_flags

/* Through memory: the form a real compare-and-swap loop uses. */
    .globl  cmpxchg_memory
    .type   cmpxchg_memory, @function
cmpxchg_memory:
    movq    %rsi, -8(%rsp)
    movq    %rdi, %rax
    cmpxchgq %rdx, -8(%rsp)
    movq    -8(%rsp), %rsi
    addq    %rsi, %rax
    ret
    .size   cmpxchg_memory, .-cmpxchg_memory

    .section .note.GNU-stack,"",@progbits
