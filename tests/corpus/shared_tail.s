/* Two functions sharing a body, entered at different points.
 *
 * This is not a corner case invented for a test: it is how glibc writes
 * `mempcpy`. The two differ only in what they return — `memcpy` gives back
 * the destination, `mempcpy` the destination plus the count — so `mempcpy`
 * computes its own return value and then jumps *into* `memcpy`, past the
 * instruction where `memcpy` computes a different one. One copy loop, two
 * entry points, and a symbol table that can only describe the first.
 *
 * The other producer is gcc's hot/cold splitting: several of a function's
 * cold exits are moved into one `.cold` fragment under a single symbol, and
 * the hot code jumps to each of them separately, so only the first lands on
 * the symbol's own address.
 *
 * rdi is a value, rsi a count.
 */

    .text

/* Enters `scaled` after its return-value setup, having done its own. */
    .globl  offset_by_count
    .type   offset_by_count, @function
offset_by_count:
    movq    %rdi, %rax
    addq    %rsi, %rax
    jmp     .Lshared
    .size   offset_by_count, .-offset_by_count

    .globl  scaled
    .type   scaled, @function
scaled:
    movq    %rdi, %rax
.Lshared:
    /* The shared body, reached at two different entry points. */
    imulq   $3, %rsi, %rdx
    addq    %rdx, %rax
    ret
    .size   scaled, .-scaled

/* The cold-fragment shape: one symbol over two independent stubs, each
   entered by its own jump from elsewhere. */
    .globl  hot
    .type   hot, @function
hot:
    testq   %rsi, %rsi
    je      .Lfirst_exit
    cmpq    $1, %rsi
    je      .Lsecond_exit
    movq    $7, %rax
    ret
    .size   hot, .-hot

    .globl  hot_cold
    .type   hot_cold, @function
hot_cold:
.Lfirst_exit:
    movq    $100, %rax
    ret
.Lsecond_exit:
    movq    $200, %rax
    ret
    .size   hot_cold, .-hot_cold

/* The shape splitting makes at a piece's last instruction: a conditional
 * branch out of the function, whose *untaken* path is the first byte of the
 * function below.
 *
 * Both edges leave. The taken one is a tail call; not taking it runs off the
 * end into the next function, which is a tail call too. A translation that
 * assumed the untaken side stays inside refuses the whole function, and it
 * refuses it in glibc's `memcpy`: the size check against the non-temporal
 * threshold is the last instruction of one split piece and its fall-through
 * is the first byte of the next. A 41-byte `puts` reaches it; a 5-byte one
 * does not, which is why nothing here noticed for so long.
 *
 * `rdi` selects the path, `rsi` is carried through so the two answers differ.
 */
    .globl  leaves_or_falls_out
    .type   leaves_or_falls_out, @function
leaves_or_falls_out:
    testq   %rdi, %rdi
    jne     taken_side
    .size   leaves_or_falls_out, .-leaves_or_falls_out

/* Falling out of the function above lands exactly here. */
    .globl  untaken_side
    .type   untaken_side, @function
untaken_side:
    movq    %rsi, %rax
    addq    $11, %rax
    ret
    .size   untaken_side, .-untaken_side

    .globl  taken_side
    .type   taken_side, @function
taken_side:
    movq    %rsi, %rax
    imulq   $3, %rax, %rax
    ret
    .size   taken_side, .-taken_side

    .section .note.GNU-stack,"",@progbits
