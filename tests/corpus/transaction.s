/* Hardware transactional memory, which this machine does not have.
 *
 * `xbegin` starts a transaction and names the address to jump to if it
 * aborts; `xend` commits one. The architecture is explicit that a
 * transaction may abort spuriously, for no reason at all, and that software
 * must never rely on one committing — which is why every user of `xbegin`
 * carries a non-transactional fallback. So a transaction that always aborts
 * is a conformant implementation rather than a stand-in for one.
 *
 * glibc's `__lll_lock_elision` is the real caller: it tries a transaction,
 * and on abort inspects the status for the retry bit before giving up and
 * taking the lock. A status with that bit set would spin its loop forever,
 * so zero — aborted, do not retry — is the only answer that works.
 *
 * These cannot be compared against a native run: on a machine without TSX
 * `xbegin` is an invalid opcode, and on one with it the transaction may
 * genuinely commit. Either way the answer would depend on the host, which is
 * the thing a container must not do.
 */

    .text

/* Returns the abort status, and 1000 if the transaction ever commits. */
    .globl  transaction_status
    .type   transaction_status, @function
transaction_status:
    xbegin  .Laborted
    /* The transaction body, which is never entered. */
    xend
    movq    $1000, %rax
    ret
.Laborted:
    /* `xbegin` delivers the status in eax on the abort path. */
    movl    %eax, %eax
    ret
    .size   transaction_status, .-transaction_status

/* The shape `__lll_lock_elision` has: retry while the status says it is
   worth retrying, then fall back. Returns how many attempts were made,
   which must be one. */
    .globl  elision_attempts
    .type   elision_attempts, @function
elision_attempts:
    xorq    %rcx, %rcx
.Lretry:
    incq    %rcx
    xbegin  .Lfailed
    xend
    movq    $-1, %rax           /* committed: not reachable */
    ret
.Lfailed:
    testl   $2, %eax            /* the retry bit */
    jz      .Lgive_up
    cmpq    $100, %rcx          /* a bound, so a wrong status is a hang no test survives */
    jl      .Lretry
.Lgive_up:
    movq    %rcx, %rax
    ret
    .size   elision_attempts, .-elision_attempts

/* `xtest` asks whether a transaction is active. It never is. */
    .globl  inside_transaction
    .type   inside_transaction, @function
inside_transaction:
    xtest
    setnz   %al
    movzbl  %al, %eax
    ret
    .size   inside_transaction, .-inside_transaction

    .section .note.GNU-stack,"",@progbits
