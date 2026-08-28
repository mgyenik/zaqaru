/* The timestamp counter, which counts reads rather than cycles.
 *
 * `rdtsc` answers from machine state, not from the world: a host counter
 * read straight through would differ every run, and the design commits to
 * two runs from one seed producing identical output. Time reaches the guest
 * as syscalls through `/iso/time` — which is why the auxiliary vector omits
 * `AT_SYSINFO_EHDR` — and `rdtsc` is an instruction that bypasses that
 * whether or not we would like it to.
 *
 * The counter is sixty-four bits delivered as two halves, `%edx` high and
 * `%eax` low, each a thirty-two bit write that clears the register above it.
 */

    .text

/* The whole counter, reassembled. */
    .globl  timestamp
    .type   timestamp, @function
timestamp:
    rdtsc
    shlq    $32, %rdx
    orq     %rdx, %rax
    ret
    .size   timestamp, .-timestamp

/* How far it moves between two reads. */
    .globl  timestamp_step
    .type   timestamp_step, @function
timestamp_step:
    rdtsc
    shlq    $32, %rdx
    orq     %rdx, %rax
    movq    %rax, %rcx
    rdtsc
    shlq    $32, %rdx
    orq     %rdx, %rax
    subq    %rcx, %rax
    ret
    .size   timestamp_step, .-timestamp_step

/* The high half alone, which must be the counter's top thirty-two bits and
   not a copy of the low half. */
    .globl  timestamp_high
    .type   timestamp_high, @function
timestamp_high:
    rdtsc
    movl    %edx, %eax
    ret
    .size   timestamp_high, .-timestamp_high

/* glibc's adaptive-mutex jitter takes the low bits under a mask. Successive
   reads must not hand it the same value every time. */
    .globl  timestamp_jitter
    .type   timestamp_jitter, @function
timestamp_jitter:
    rdtsc
    andl    $15, %eax
    movl    %eax, %ecx
    rdtsc
    andl    $15, %eax
    shll    $4, %eax
    orl     %ecx, %eax
    ret
    .size   timestamp_jitter, .-timestamp_jitter

/* `rdtscp` also reports which processor answered. There is one. */
    .globl  timestamp_processor
    .type   timestamp_processor, @function
timestamp_processor:
    rdtscp
    movl    %ecx, %eax
    ret
    .size   timestamp_processor, .-timestamp_processor

    .section .note.GNU-stack,"",@progbits
