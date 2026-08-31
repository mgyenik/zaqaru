/* Nine workload shapes, one binary, so that one rootfs and one bake cover
 * all of them and the only thing differing between runs is the argument.
 *
 * Every kernel is deterministic and prints a checksum. The checksum is not
 * decoration: three execution paths that disagree about the answer are not
 * three timings of the same thing, and a benchmark that computes something
 * different under interpretation would post a wonderful number for it.
 *
 * No kernel times itself. Under the interpreter a guest's clock is
 * interpolated from a retired-instruction counter between timebase
 * refreshes, so in-guest timing measures something close to instructions
 * rather than seconds — the host holds the only clock worth reporting. What
 * the host cannot separate is process start-up, which is why `noop` exists:
 * it is the same start-up with no kernel after it, and subtracting it is how
 * the workload is isolated.
 *
 *   usage: bench <name> [scale]
 */

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>
#include <unistd.h>

/* A cheap deterministic generator, used to build inputs rather than to be
 * benchmarked. Identical on every path because it is plain integer work. */
static uint64_t next_random(uint64_t *state) {
    *state = *state * 6364136223846793005ULL + 1442695040888963407ULL;
    uint64_t x = *state;
    x ^= x >> 33;
    x *= 0xff51afd7ed558ccdULL;
    x ^= x >> 29;
    return x;
}

/* Dependent integer arithmetic: a chain where each operation needs the one
 * before it, so a native CPU cannot hide the latency and an interpreter
 * pays per instruction. The floor of what any engine can do. */
static uint64_t kernel_alu(uint64_t rounds) {
    uint64_t a = 1, b = 2, c = 3, d = 4;
    for (uint64_t i = 0; i < rounds; i++) {
        a = a * 6364136223846793005ULL + 1442695040888963407ULL;
        b ^= a >> 13;
        b += i;
        c = (c << 7) | (c >> 57);
        c ^= b;
        d += a ^ c;
    }
    return a ^ b ^ c ^ d;
}

/* Sequential memory: one pass over an array far larger than any cache, with
 * a stride of one. Native this is bounded by memory bandwidth; interpreted
 * it is bounded by the engine, which is the comparison worth having. */
static uint64_t kernel_memory_sequential(uint64_t rounds) {
    const size_t count = 4u << 20; /* 32 MB of uint64_t */
    uint64_t *data = malloc(count * sizeof *data);
    if (!data) return 0;
    for (size_t i = 0; i < count; i++) data[i] = i * 2654435761u;
    uint64_t sum = 0;
    for (uint64_t pass = 0; pass < rounds; pass++) {
        for (size_t i = 0; i < count; i++) sum += data[i];
    }
    free(data);
    return sum;
}

/* Random memory: a pointer chase around a shuffled cycle, so every access
 * depends on the one before it and no prefetcher can help. The shape that
 * separates "the engine is slow" from "the machine is waiting for RAM". */
static uint64_t kernel_memory_random(uint64_t rounds) {
    const size_t count = 1u << 21; /* 16 MB of indices */
    uint32_t *next = malloc(count * sizeof *next);
    if (!next) return 0;
    for (size_t i = 0; i < count; i++) next[i] = (uint32_t)i;
    uint64_t state = 12345;
    /* Fisher-Yates, which leaves one permutation; a permutation of indices
     * is a set of cycles, and walking it touches each slot once. */
    for (size_t i = count - 1; i > 0; i--) {
        size_t j = next_random(&state) % (i + 1);
        uint32_t held = next[i];
        next[i] = next[j];
        next[j] = held;
    }
    uint64_t at = 0, sum = 0;
    for (uint64_t step = 0; step < rounds; step++) {
        at = next[at];
        sum += at;
    }
    free(next);
    return sum;
}

/* Calls: a recursion deep enough that the work is prologue, epilogue and
 * stack traffic rather than arithmetic. */
static uint64_t fibonacci(uint32_t n) {
    if (n < 2) return n;
    return fibonacci(n - 1) + fibonacci(n - 2);
}

static uint64_t kernel_calls(uint64_t rounds) {
    uint64_t sum = 0;
    for (uint64_t i = 0; i < rounds; i++) sum += fibonacci(24);
    return sum;
}

/* Branches a predictor cannot learn: the condition is a fresh pseudorandom
 * bit each time. Native this costs mispredictions; interpreted a branch is
 * just another decoded instruction, so this is where the ratio should be at
 * its *smallest* — and a benchmark set with no such case would flatter the
 * engine by omission. */
static uint64_t kernel_branches(uint64_t rounds) {
    uint64_t state = 99, taken = 0, other = 0;
    for (uint64_t i = 0; i < rounds; i++) {
        uint64_t value = next_random(&state);
        if (value & 0x10) taken += value & 0xff;
        else if (value & 0x20) other ^= value >> 8;
        else taken -= 1;
    }
    return taken ^ other;
}

/* The string routines, which is glibc's hand-written SSE and not the
 * compiler's output — a different question from `memory_sequential`,
 * because what the engine has to interpret is vector code. */
static uint64_t kernel_string(uint64_t rounds) {
    const size_t span = 64 * 1024;
    char *from = malloc(span), *to = malloc(span);
    if (!from || !to) return 0;
    for (size_t i = 0; i < span - 1; i++) from[i] = (char)('a' + (i % 26));
    from[span - 1] = 0;
    uint64_t sum = 0;
    for (uint64_t i = 0; i < rounds; i++) {
        memcpy(to, from, span);
        sum += strlen(to);
        sum += (uint64_t)(memcmp(to, from, span) == 0);
    }
    free(from);
    free(to);
    return sum;
}

/* Double-precision floating point, including a transcendental, so the
 * measurement covers the x87/SSE paths rather than only the integer core. */
static uint64_t kernel_float(uint64_t rounds) {
    double x = 1.0, y = 0.5, total = 0.0;
    for (uint64_t i = 0; i < rounds; i++) {
        x = x * 1.0000001 + 0.000001;
        y = sqrt(y * y + x * 0.25);
        total += x / (y + 1.0);
    }
    /* Through the bits, so the checksum compares exactly rather than by an
     * epsilon somebody has to justify. */
    uint64_t bits;
    memcpy(&bits, &total, sizeof bits);
    return bits;
}

/* The kernel seam: a round trip through a pipe, which under the container is
 * a syscall into kisal and back. Nothing about the engine's speed at
 * instructions — this is the cost of leaving the guest. */
static uint64_t kernel_syscalls(uint64_t rounds) {
    int ends[2];
    if (pipe(ends) != 0) return 0;
    uint64_t sum = 0;
    char byte = 1;
    for (uint64_t i = 0; i < rounds; i++) {
        if (write(ends[1], &byte, 1) != 1) break;
        if (read(ends[0], &byte, 1) != 1) break;
        sum += (uint64_t)byte;
    }
    close(ends[0]);
    close(ends[1]);
    return sum;
}

/* Allocator churn: sizes that cross the bins, freed out of order, which is
 * what a real program does to malloc and what a loop over one size does
 * not. */
static uint64_t kernel_alloc(uint64_t rounds) {
    void *held[64] = {0};
    uint64_t state = 7, sum = 0;
    for (uint64_t i = 0; i < rounds; i++) {
        size_t slot = next_random(&state) % 64;
        if (held[slot]) {
            free(held[slot]);
            held[slot] = NULL;
        }
        size_t size = 16 + (next_random(&state) % 4096);
        held[slot] = malloc(size);
        if (held[slot]) {
            ((char *)held[slot])[0] = (char)i;
            ((char *)held[slot])[size - 1] = (char)size;
            sum += size;
        }
    }
    for (size_t i = 0; i < 64; i++) free(held[i]);
    return sum;
}

struct entry {
    const char *name;
    uint64_t (*run)(uint64_t);
    uint64_t scale;
};

/* Sized so that each runs for roughly the same short time natively, which
 * keeps one slow shape from dominating the wall clock of the whole set. */
static const struct entry entries[] = {
    {"alu", kernel_alu, 4000000},
    {"memory_sequential", kernel_memory_sequential, 2},
    {"memory_random", kernel_memory_random, 2000000},
    {"calls", kernel_calls, 40},
    {"branches", kernel_branches, 3000000},
    {"string", kernel_string, 400},
    {"float", kernel_float, 2000000},
    {"syscalls", kernel_syscalls, 100000},
    {"alloc", kernel_alloc, 1000000},
};

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: bench <name|noop> [scale]\n");
        for (size_t i = 0; i < sizeof entries / sizeof *entries; i++)
            fprintf(stderr, "  %s\n", entries[i].name);
        return 2;
    }
    /* The same start-up with no kernel after it. Subtracting this is what
     * turns a process time into a workload time. */
    if (strcmp(argv[1], "noop") == 0) {
        printf("noop 0\n");
        return 0;
    }
    for (size_t i = 0; i < sizeof entries / sizeof *entries; i++) {
        if (strcmp(argv[1], entries[i].name) != 0) continue;
        uint64_t scale = argc > 2 ? strtoull(argv[2], NULL, 10) : entries[i].scale;
        uint64_t answer = entries[i].run(scale);
        printf("%s %llu\n", entries[i].name, (unsigned long long)answer);
        return 0;
    }
    fprintf(stderr, "bench: no kernel called %s\n", argv[1]);
    return 2;
}
