/* `rep stos` and `rep movs` — memset and memcpy as single instructions.
 *
 * Both walk `%rdi` (and `%rsi`) forward or backward according to the
 * direction flag, which is the only reason that flag is modelled at all: a
 * libc's `memmove` sets it to copy an overlapping range backwards and clears
 * it again immediately after.
 *
 * The count is what makes these easy to get subtly wrong. `rep` with `%rcx`
 * of zero stores nothing *and* leaves the pointers where they were, so the
 * test has to come before the first element rather than after it.
 *
 * Each function works in a buffer on the stack and returns a checksum of it
 * plus where the pointers ended, so a translation that moved a pointer the
 * wrong way or ran one element too many is a different number.
 */

    .text

/* Sums 8 quadwords at %rsi into %rax. */
.macro CHECKSUM base
    xorq    %rax, %rax
    movq    0(\base), %r10
    addq    %r10, %rax
    movq    8(\base), %r10
    addq    %r10, %rax
    movq    16(\base), %r10
    addq    %r10, %rax
    movq    24(\base), %r10
    addq    %r10, %rax
    movq    32(\base), %r10
    addq    %r10, %rax
    movq    40(\base), %r10
    addq    %r10, %rax
    movq    48(\base), %r10
    addq    %r10, %rax
    movq    56(\base), %r10
    addq    %r10, %rax
.endm

/* rdi = fill value, rsi = count. Fills quadwords forward. */
    .globl  stos_quad_forward
    .type   stos_quad_forward, @function
stos_quad_forward:
    subq    $64, %rsp
    /* Zero the buffer so an under-run is visible. */
    movq    $0, 0(%rsp)
    movq    $0, 8(%rsp)
    movq    $0, 16(%rsp)
    movq    $0, 24(%rsp)
    movq    $0, 32(%rsp)
    movq    $0, 40(%rsp)
    movq    $0, 48(%rsp)
    movq    $0, 56(%rsp)
    movq    %rdi, %rax
    movq    %rsi, %rcx
    movq    %rsp, %rdi
    cld
    rep stosq
    /* Where the pointer ended, relative to the buffer. */
    subq    %rsp, %rdi
    movq    %rdi, %r11
    CHECKSUM %rsp
    addq    %r11, %rax
    addq    %rcx, %rax
    addq    $64, %rsp
    ret
    .size   stos_quad_forward, .-stos_quad_forward

/* The same backwards, from the buffer's last quadword down. */
    .globl  stos_quad_backward
    .type   stos_quad_backward, @function
stos_quad_backward:
    subq    $64, %rsp
    movq    $0, 0(%rsp)
    movq    $0, 8(%rsp)
    movq    $0, 16(%rsp)
    movq    $0, 24(%rsp)
    movq    $0, 32(%rsp)
    movq    $0, 40(%rsp)
    movq    $0, 48(%rsp)
    movq    $0, 56(%rsp)
    movq    %rdi, %rax
    movq    %rsi, %rcx
    leaq    56(%rsp), %rdi
    std
    rep stosq
    cld
    leaq    56(%rsp), %r11
    subq    %rdi, %r11
    CHECKSUM %rsp
    addq    %r11, %rax
    addq    %rcx, %rax
    addq    $64, %rsp
    ret
    .size   stos_quad_backward, .-stos_quad_backward

/* Byte fill, which is what a small `memset` compiles to. */
    .globl  stos_byte_forward
    .type   stos_byte_forward, @function
stos_byte_forward:
    subq    $64, %rsp
    movq    $0, 0(%rsp)
    movq    $0, 8(%rsp)
    movq    $0, 16(%rsp)
    movq    $0, 24(%rsp)
    movq    $0, 32(%rsp)
    movq    $0, 40(%rsp)
    movq    $0, 48(%rsp)
    movq    $0, 56(%rsp)
    movq    %rdi, %rax
    movq    %rsi, %rcx
    movq    %rsp, %rdi
    cld
    rep stosb
    subq    %rsp, %rdi
    movq    %rdi, %r11
    CHECKSUM %rsp
    addq    %r11, %rax
    addq    %rcx, %rax
    addq    $64, %rsp
    ret
    .size   stos_byte_forward, .-stos_byte_forward

/* A single store with no `rep` at all, which must move the pointer once. */
    .globl  stos_quad_once
    .type   stos_quad_once, @function
stos_quad_once:
    subq    $64, %rsp
    movq    $0, 0(%rsp)
    movq    $0, 8(%rsp)
    movq    %rdi, %rax
    movq    %rsp, %rdi
    cld
    stosq
    subq    %rsp, %rdi
    movq    %rdi, %r11
    movq    0(%rsp), %rax
    addq    %r11, %rax
    addq    $64, %rsp
    ret
    .size   stos_quad_once, .-stos_quad_once

/* rdi = a value to seed the source with, rsi = element count. Copies
   quadwords forward from the top half of the buffer to the bottom. */
    .globl  movs_quad_forward
    .type   movs_quad_forward, @function
movs_quad_forward:
    subq    $128, %rsp
    /* Seed the whole source with distinguishable values — all eight, so a
       copy of the full buffer never reads stack nobody wrote. */
    movq    %rdi, 64(%rsp)
    leaq    1(%rdi), %r10
    movq    %r10, 72(%rsp)
    leaq    2(%rdi), %r10
    movq    %r10, 80(%rsp)
    leaq    3(%rdi), %r10
    movq    %r10, 88(%rsp)
    leaq    4(%rdi), %r10
    movq    %r10, 96(%rsp)
    leaq    5(%rdi), %r10
    movq    %r10, 104(%rsp)
    leaq    6(%rdi), %r10
    movq    %r10, 112(%rsp)
    leaq    7(%rdi), %r10
    movq    %r10, 120(%rsp)
    movq    $0, 0(%rsp)
    movq    $0, 8(%rsp)
    movq    $0, 16(%rsp)
    movq    $0, 24(%rsp)
    movq    $0, 32(%rsp)
    movq    $0, 40(%rsp)
    movq    $0, 48(%rsp)
    movq    $0, 56(%rsp)
    movq    %rsi, %rcx
    leaq    64(%rsp), %rsi
    movq    %rsp, %rdi
    cld
    rep movsq
    CHECKSUM %rsp
    addq    %rcx, %rax
    addq    $128, %rsp
    ret
    .size   movs_quad_forward, .-movs_quad_forward

/* Byte copy, which is what `rep movsb` in a libc actually is. */
    .globl  movs_byte_forward
    .type   movs_byte_forward, @function
movs_byte_forward:
    subq    $128, %rsp
    movq    %rdi, 64(%rsp)
    leaq    1(%rdi), %r10
    movq    %r10, 72(%rsp)
    movq    $0, 0(%rsp)
    movq    $0, 8(%rsp)
    movq    $0, 16(%rsp)
    movq    $0, 24(%rsp)
    movq    $0, 32(%rsp)
    movq    $0, 40(%rsp)
    movq    $0, 48(%rsp)
    movq    $0, 56(%rsp)
    movq    %rsi, %rcx
    leaq    64(%rsp), %rsi
    movq    %rsp, %rdi
    cld
    rep movsb  /* at most 16 bytes, all of them seeded above */
    CHECKSUM %rsp
    addq    %rcx, %rax
    addq    $128, %rsp
    ret
    .size   movs_byte_forward, .-movs_byte_forward

/* Backwards, which is the shape a `memmove` uses for an overlapping copy. */
    .globl  movs_byte_backward
    .type   movs_byte_backward, @function
movs_byte_backward:
    subq    $128, %rsp
    movq    %rdi, 64(%rsp)
    leaq    1(%rdi), %r10
    movq    %r10, 72(%rsp)
    movq    $0, 0(%rsp)
    movq    $0, 8(%rsp)
    movq    $0, 16(%rsp)
    movq    $0, 24(%rsp)
    movq    $0, 32(%rsp)
    movq    $0, 40(%rsp)
    movq    $0, 48(%rsp)
    movq    $0, 56(%rsp)
    movq    %rsi, %rcx
    leaq    79(%rsp), %rsi
    leaq    15(%rsp), %rdi
    std
    rep movsb
    cld
    CHECKSUM %rsp
    addq    %rcx, %rax
    addq    $128, %rsp
    ret
    .size   movs_byte_backward, .-movs_byte_backward

    .section .note.GNU-stack,"",@progbits
