/* `hlt` where control does not reach, which is where compilers put it.
 *
 * It halts the processor and is privileged, so reaching one from user code
 * faults. A compiler emits one after a call that never returns, as a marker
 * that the path past the call does not exist — glibc's `_start` ends in one,
 * straight after the call to `__libc_start_main`.
 *
 * Six of the seven in a static glibc are *mid*-function rather than at the
 * end: `abort`, `_Exit` and `__libc_check_standard_fds` all carry one with
 * more code after it. So it has to end its block wherever it stands, not
 * only where the function does — otherwise it falls through into whatever
 * follows, which is a path that cannot be taken and should not be described.
 */

    .text

/* At the end of the function, which is `_start`'s shape. */
    .globl  halts_at_the_end
    .type   halts_at_the_end, @function
halts_at_the_end:
    call    never_returns
    hlt
    .size   halts_at_the_end, .-halts_at_the_end

/* In the middle, with reachable code after it — `abort`'s shape. The code
   below the `hlt` is reached by the branch above it and by nothing else. */
    .globl  halts_in_the_middle
    .type   halts_in_the_middle, @function
halts_in_the_middle:
    testq   %rdi, %rdi
    jne     .Lother
    call    never_returns
    hlt
.Lother:
    movq    $99, %rax
    ret
    .size   halts_in_the_middle, .-halts_in_the_middle

    .globl  never_returns
    .type   never_returns, @function
never_returns:
    jmp     never_returns
    .size   never_returns, .-never_returns

    .section .note.GNU-stack,"",@progbits
