/* `rol`/`ror` and the `bt` family — bit motion that is not a shift.
 *
 * Both arrived from a real binary: a static glibc reaches 13 functions that
 * rotate and 9 that test a bit, hashing and bitmap work respectively.
 *
 * The flags are the whole difficulty. A shift writes five; a rotate writes
 * two and leaves the sign, zero, parity and adjust flags exactly as it found
 * them, and `bt` writes only the carry. Anything that wrote more would
 * diverge on the *next* instruction rather than this one, so each family has
 * functions that read the flags back after setting them to a known state
 * first — which is what catches a flag written that should not have been.
 *
 * Arguments arrive in rdi (value) and rsi (count or bit offset).
 */

    .text

/* Carry and overflow only, in that order: the two a rotate defines. */
.macro PACK_CARRY_OVERFLOW
    setc    %r8b
    seto    %r9b
    movzbl  %r8b, %eax
    movzbl  %r9b, %r9d
    shll    $1, %r9d
    orl     %r9d, %eax
.endm

/* All five, for checking that the ones a rotate must not touch survive. */
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

/* ------------------------------------------------------ rotate results --- */

.macro ROTATE name, op, suffix, dst, out
    .globl  \name
    .type   \name, @function
\name:
    movl    %esi, %ecx
    \op\suffix %cl, \dst
    \out
    ret
    .size   \name, .-\name
.endm

ROTATE rol_qword, rol, q, %rdi, "movq %rdi, %rax"
ROTATE ror_qword, ror, q, %rdi, "movq %rdi, %rax"
ROTATE rol_dword, rol, l, %edi, "movq %rdi, %rax"
ROTATE ror_dword, ror, l, %edi, "movq %rdi, %rax"
ROTATE rol_word,  rol, w, %di,  "movzwl %di, %eax"
ROTATE ror_word,  ror, w, %di,  "movzwl %di, %eax"
ROTATE rol_byte,  rol, b, %dil, "movzbl %dil, %eax"
ROTATE ror_byte,  ror, b, %dil, "movzbl %dil, %eax"

/* ------------------------------------------------------ rotate flags ----- */

/* Carry and overflow after rotating by a variable count. */
.macro ROTATE_FLAGS name, op, suffix, dst
    .globl  \name
    .type   \name, @function
\name:
    movl    %esi, %ecx
    \op\suffix %cl, \dst
    PACK_CARRY_OVERFLOW
    ret
    .size   \name, .-\name
.endm

ROTATE_FLAGS rol_qword_flags, rol, q, %rdi
ROTATE_FLAGS ror_qword_flags, ror, q, %rdi
ROTATE_FLAGS rol_word_flags,  rol, w, %di
ROTATE_FLAGS ror_word_flags,  ror, w, %di
ROTATE_FLAGS rol_byte_flags,  rol, b, %dil

/* The four flags a rotate must leave alone. A comparison sets them all to a
   known state first; a rotate that wrote any of them would show up here and
   nowhere else. */
    .globl  rol_preserves_flags
    .type   rol_preserves_flags, @function
rol_preserves_flags:
    movl    %esi, %ecx
    cmpq    $0, %rdi
    rolq    %cl, %rdi
    PACK_FLAGS
    /* Mask out carry and overflow, which a rotate does define. */
    andl    $0x1c, %eax
    ret
    .size   rol_preserves_flags, .-rol_preserves_flags

    .globl  ror_preserves_flags
    .type   ror_preserves_flags, @function
ror_preserves_flags:
    movl    %esi, %ecx
    cmpq    $0, %rdi
    rorq    %cl, %rdi
    PACK_FLAGS
    andl    $0x1c, %eax
    ret
    .size   ror_preserves_flags, .-ror_preserves_flags

/* The immediate-count encoding, which is the only one glibc actually uses. */
    .globl  ror_qword_immediate
    .type   ror_qword_immediate, @function
ror_qword_immediate:
    rorq    $13, %rdi
    movq    %rdi, %rax
    ret
    .size   ror_qword_immediate, .-ror_qword_immediate

    .globl  rol_word_immediate
    .type   rol_word_immediate, @function
rol_word_immediate:
    rolw    $9, %di
    movzwl  %di, %eax
    ret
    .size   rol_word_immediate, .-rol_word_immediate

/* ---------------------------------------------------------- bit tests --- */

/* `bt` writes only the carry, so the answer is the carry. */
    .globl  bt_qword
    .type   bt_qword, @function
bt_qword:
    btq     %rsi, %rdi
    setc    %al
    movzbl  %al, %eax
    ret
    .size   bt_qword, .-bt_qword

    .globl  bt_dword
    .type   bt_dword, @function
bt_dword:
    btl     %esi, %edi
    setc    %al
    movzbl  %al, %eax
    ret
    .size   bt_dword, .-bt_dword

/* The offset wraps to the operand's width — bit 64 of a quadword is bit 0. */
    .globl  bt_qword_immediate
    .type   bt_qword_immediate, @function
bt_qword_immediate:
    btq     $70, %rdi
    setc    %al
    movzbl  %al, %eax
    ret
    .size   bt_qword_immediate, .-bt_qword_immediate

/* And `bt` must leave the other four alone. */
    .globl  bt_preserves_flags
    .type   bt_preserves_flags, @function
bt_preserves_flags:
    cmpq    $0, %rdi
    btq     %rsi, %rdi
    PACK_FLAGS
    andl    $0x1c, %eax
    ret
    .size   bt_preserves_flags, .-bt_preserves_flags

/* The three that write the bit back: the result, then the carry, which must
   still be the bit as it was *before* the write. */
.macro BIT_WRITE name, op, dst, out
    .globl  \name
    .type   \name, @function
\name:
    \op     %rsi, \dst
    \out
    ret
    .size   \name, .-\name
.endm

BIT_WRITE bts_qword, btsq, %rdi, "movq %rdi, %rax"
BIT_WRITE btr_qword, btrq, %rdi, "movq %rdi, %rax"
BIT_WRITE btc_qword, btcq, %rdi, "movq %rdi, %rax"

.macro BIT_WRITE_CARRY name, op, dst
    .globl  \name
    .type   \name, @function
\name:
    \op     %rsi, \dst
    setc    %al
    movzbl  %al, %eax
    ret
    .size   \name, .-\name
.endm

BIT_WRITE_CARRY bts_qword_carry, btsq, %rdi
BIT_WRITE_CARRY btr_qword_carry, btrq, %rdi
BIT_WRITE_CARRY btc_qword_carry, btcq, %rdi

    .globl  bts_dword
    .type   bts_dword, @function
bts_dword:
    btsl    %esi, %edi
    movq    %rdi, %rax
    ret
    .size   bts_dword, .-bts_dword

    .section .note.GNU-stack,"",@progbits
