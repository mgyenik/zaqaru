/* Call sites to functions nothing here defines, in the shapes caller-side
 * inference has to have opinions about.
 *
 * Hand-written because C cannot express two of these: calling one symbol with
 * two different argument counts is exactly what a header exists to prevent,
 * and it is exactly the disagreement inference must refuse rather than
 * average. The compiler would also be within its rights to reorder the
 * argument setup, and these cases are about what the setup *is*.
 */

    .text

/* ---- two call sites that cannot both be right ----
 *
 * One passes one argument, the other two. Inference must name both callers
 * rather than pick the popular reading: a disagreement means the evidence is
 * being misread somewhere, and a signature that looks authoritative would
 * bury that.
 */

    .globl  passes_one
    .type   passes_one, @function
passes_one:
    subq    $8, %rsp
    movl    $1, %edi
    call    disputed
    addq    $8, %rsp
    ret
    .size   passes_one, .-passes_one

    .globl  passes_two
    .type   passes_two, @function
passes_two:
    subq    $8, %rsp
    movl    $1, %edi
    movl    $2, %esi
    call    disputed
    addq    $8, %rsp
    ret
    .size   passes_two, .-passes_two

/* ---- the variadic protocol ----
 *
 * SysV puts the number of vector registers used into `al` before a call to a
 * variadic function. The value would be dead otherwise, which is what makes
 * it a signal rather than a coincidence — and it means the arguments do not
 * all travel in registers, so no thunk can carry them.
 */

    .globl  calls_a_variadic_function
    .type   calls_a_variadic_function, @function
calls_a_variadic_function:
    subq    $8, %rsp
    movl    $1, %edi
    movl    $0, %eax
    call    formatted
    addq    $8, %rsp
    ret
    .size   calls_a_variadic_function, .-calls_a_variadic_function

/* ---- an ordinary agreed call, so the test has a positive case ---- */

    .globl  passes_a_pair
    .type   passes_a_pair, @function
passes_a_pair:
    subq    $8, %rsp
    movl    $7, %edi
    movl    $9, %esi
    call    agreed
    addq    $8, %rsp
    ret
    .size   passes_a_pair, .-passes_a_pair

    .globl  passes_a_pair_again
    .type   passes_a_pair_again, @function
passes_a_pair_again:
    subq    $8, %rsp
    movl    $3, %edi
    movl    $4, %esi
    call    agreed
    addq    $8, %rsp
    ret
    .size   passes_a_pair_again, .-passes_a_pair_again

    .section .note.GNU-stack,"",@progbits
