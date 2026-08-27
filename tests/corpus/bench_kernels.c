/* The benchmark kernels: each stresses a different kind of machine-state
 * traffic, so that no single number can stand in for "performance".
 *
 * Compiled two ways — for x86-64 and transpiled, and for wasm32 by clang's
 * own backend as the ceiling — so, like the interop corpus, nothing here may
 * depend on the width of `long` or of a pointer.
 *
 * Every loop carries a serial dependence on purpose. A vectorisable loop
 * measures the vectoriser, and a foldable one measures constant propagation;
 * a serial chain measures what the emitted code does with the machine state
 * on every iteration, which is the thing register promotion changes.
 */

/* Integer arithmetic: xorshift steps, flag-setting adds and a data-dependent
 * branch. Every step reads and writes registers and nothing else, which makes
 * this the kernel promotion should help most. */
long long bench_integer(int iterations, long long seed) {
    unsigned long long state = (unsigned long long)seed | 1;
    unsigned long long total = 0;
    for (int index = 0; index < iterations; index++) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        total += state;
        if (state & 1) {
            total ^= 0x9e3779b97f4a7c15ull;
        }
    }
    return (long long)total;
}

/* Memory traversal: a serial hash over a buffer. Every iteration is a load
 * plus a dependent multiply, so this is bounded by linear memory either way —
 * the kernel promotion should help least, kept to say so out loud. */
static unsigned long long buffer[8192];

long long bench_memory(int passes, long long seed) {
    unsigned long long state = (unsigned long long)seed | 1;
    for (int index = 0; index < 8192; index++) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        buffer[index] = state;
    }
    unsigned long long hash = 1469598103934665603ull;
    for (int pass = 0; pass < passes; pass++) {
        for (int index = 0; index < 8192; index++) {
            hash = (hash ^ buffer[index]) * 1099511628211ull;
        }
    }
    return (long long)hash;
}

/* Scalar floating point: a polynomial step iterated on itself, XMM traffic
 * throughout. The range clamp keeps the value finite for any iteration
 * count, and adds the comparison-and-branch shape float code is full of. */
double bench_float(int iterations, double x) {
    double value = x;
    for (int index = 0; index < iterations; index++) {
        value = ((0.25 * value - 0.5) * value + 1.5) * value - 0.75;
        if (value > 4.0 || value < -4.0) {
            value *= 0.0625;
        }
    }
    return value;
}

/* Call-heavy recursion: almost nothing but calls and returns. This is the
 * kernel the flush-and-reload discipline could plausibly make slower, kept
 * to measure exactly that. */
long long bench_calls(int depth) {
    if (depth < 2) {
        return (long long)depth;
    }
    return bench_calls(depth - 1) + bench_calls(depth - 2) + 1;
}
