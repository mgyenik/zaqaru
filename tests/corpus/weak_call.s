/* A call to a weak symbol nothing defines.
 *
 * A static link resolves such a symbol to address zero and emits the call
 * anyway; the code around it guards on the pointer being non-null, so the
 * call is never taken. glibc does this around its locale and threading
 * hooks, and `__libc_start_main` — the function every program starts in —
 * is one of the ones that contains one.
 *
 * There is no function at address zero to name, and real hardware would
 * fault on reaching it. So the call translates to a trap, which is faithful,
 * and the rest of the function translates normally — which is the point,
 * because refusing the function would refuse the program's entry.
 *
 * rdi chooses whether the guard lets the call through, so a test can prove
 * the guarded path still computes.
 */

    .text
    .weak   absent_hook

    .globl  guarded_weak_call
    .type   guarded_weak_call, @function
guarded_weak_call:
    pushq   %rbx
    movq    %rdi, %rbx
    /* Through memory, so nothing can fold the test away. */
    movq    hook_pointer(%rip), %rax
    testq   %rax, %rax
    je      .Lskip
    call    absent_hook
.Lskip:
    movq    %rbx, %rax
    addq    $7, %rax
    popq    %rbx
    ret
    .size   guarded_weak_call, .-guarded_weak_call

    .data
    .align  8
hook_pointer:
    .quad   0

    .section .note.GNU-stack,"",@progbits
