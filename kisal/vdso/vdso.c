/* The vDSO: the clock, without a syscall.
 *
 * Linux maps a small shared object into every process holding fast
 * implementations of the few calls that only *read* kernel state — the
 * clocks. glibc finds it through `AT_SYSINFO_EHDR` in the auxiliary vector,
 * resolves these symbols out of its dynamic symbol table, and calls them
 * directly. That is why a native `strace` of a program that reads the clock
 * ten thousand times shows no clock syscalls at all.
 *
 * This is that object, for kisal. It is compiled by the host toolchain at
 * build time exactly as Linux compiles its own, because an ELF with correct
 * hash tables and symbol versioning is not a thing to hand-assemble — and
 * glibc checks the version, silently falling back to syscalls if it does not
 * match, which is a failure that looks like success.
 *
 * # What it reads
 *
 * A page the kernel writes, and `rdtsc`. A real vDSO interpolates between
 * kernel timestamps using the processor's cycle counter; here `rdtsc`
 * answers from the retired-instruction counter, so the same arithmetic
 * works and gives something better than a real one does: between kernel
 * updates the time a guest reads is a pure function of how far it has
 * executed.
 *
 * # What it must not do
 *
 * Every instruction here is executed by the interpreter, so this stays to
 * plain integer work: no SSE, no string instructions, nothing the engine
 * would have to name. And it must not *fail* — a vDSO that returns an error
 * for a clock it does not know sends glibc to the syscall, which is the
 * fallback and is correct, so unknown clocks return -1 deliberately.
 */

typedef unsigned char u8;
typedef unsigned int u32;
typedef unsigned long long u64;
typedef long long i64;

/* Must match `kisal::vdso::Timebase` exactly; a test pins the layout. */
struct timebase {
    /* Bumped to an odd value before a write and to the next even one after,
     * so a reader that sees the same even value twice knows nothing moved
     * underneath it. Linux's seqlock, and needed for the same reason: the
     * kernel writes this while a guest thread may be reading it. */
    volatile u32 sequence;
    u32 usable;
    /* What the clocks read at `base_tsc`. */
    u64 base_realtime;
    u64 base_monotonic;
    u64 base_tsc;
    /* Nanoseconds per tick, as `(delta * multiplier) >> shift`. */
    u64 multiplier;
    u32 shift;
    u32 padding;
};

struct timespec {
    i64 seconds;
    i64 nanoseconds;
};

struct timeval {
    i64 seconds;
    i64 microseconds;
};

/* The kernel maps the page immediately below this object. Its address is
 * therefore ours minus a page, which the linker script arranges and which
 * this computes without a relocation the loader would have to apply. */
extern __attribute__((visibility("hidden"))) char __vdso_image_start[];

static const struct timebase *timebase(void)
{
    return (const struct timebase *)(__vdso_image_start - 4096);
}

static u64 ticks(void)
{
    u32 low, high;
    __asm__ __volatile__("rdtsc" : "=a"(low), "=d"(high));
    return ((u64)high << 32) | low;
}

/* `(delta * multiplier) >> shift`, in 128 bits.
 *
 * A 64-bit multiply would overflow: the counter advances about a billion per
 * retired instruction, so a delta across one scheduling slice is already
 * 10^15, and the multiplier is what carries the precision. x86-64's `mul`
 * gives the full 128-bit product for free, and `shrd` takes the window out
 * of it. */
static u64 scaled(u64 delta, u64 multiplier, u32 shift)
{
    u64 low, high;
    __asm__("mulq %3" : "=a"(low), "=d"(high) : "a"(delta), "r"(multiplier));
    __asm__("shrdq %%cl, %1, %0" : "+r"(low) : "r"(high), "c"(shift));
    return low;
}

/* Stops the compiler moving a load across this point.
 *
 * Not paranoia about processors — there is one, and the guest's own
 * instructions are what order things here. It is about the *compiler*, which
 * will happily hoist a field read out of the retry loop and above the
 * sequence check, and then the seqlock protects nothing. Measured: without
 * it, `usable` was read once before the loop was entered at all. */
#define barrier() __asm__ __volatile__("" ::: "memory")

/* Nanoseconds now, or 0 when the kernel has not published a timebase — which
 * is not an error, it is "ask the kernel", and the callers turn it into the
 * syscall fallback.
 *
 * The retry is real. A guest thread is preempted at instruction boundaries
 * and the kernel refreshes this page at scheduling points, so a thread *can*
 * be stopped in the middle of here and resumed against a page that has
 * moved underneath it. Half of one timebase and half of another is a time
 * that never happened. */
static u64 now(int monotonic)
{
    for (;;) {
        const struct timebase *held = timebase();
        u32 before = held->sequence;
        barrier();
        if (before & 1) {
            continue;
        }
        if (!held->usable) {
            return 0;
        }
        u64 base = monotonic ? held->base_monotonic : held->base_realtime;
        u64 base_tsc = held->base_tsc;
        u64 multiplier = held->multiplier;
        u32 shift = held->shift;
        barrier();
        /* Read after, and compared: if the kernel wrote while this was
         * reading, everything above may be a mixture of two timebases. */
        if (held->sequence != before) {
            continue;
        }
        return base + scaled(ticks() - base_tsc, multiplier, shift);
    }
}

#define CLOCK_REALTIME 0
#define CLOCK_MONOTONIC 1
#define CLOCK_MONOTONIC_RAW 4
#define CLOCK_REALTIME_COARSE 5
#define CLOCK_MONOTONIC_COARSE 6
#define CLOCK_BOOTTIME 7

static int monotonic_clock(int which)
{
    switch (which) {
    case CLOCK_MONOTONIC:
    case CLOCK_MONOTONIC_RAW:
    case CLOCK_MONOTONIC_COARSE:
    case CLOCK_BOOTTIME:
        return 1;
    default:
        return 0;
    }
}

static int known_clock(int which)
{
    switch (which) {
    case CLOCK_REALTIME:
    case CLOCK_MONOTONIC:
    case CLOCK_MONOTONIC_RAW:
    case CLOCK_REALTIME_COARSE:
    case CLOCK_MONOTONIC_COARSE:
    case CLOCK_BOOTTIME:
        return 1;
    default:
        return 0;
    }
}

int __vdso_clock_gettime(int which, struct timespec *into)
{
    if (!known_clock(which)) {
        /* Not ours. `-ENOSYS` is how a vDSO says "issue the syscall", and
         * glibc does exactly that — the per-process CPU clocks arrive here
         * and have to. */
        return -38;
    }
    u64 nanoseconds = now(monotonic_clock(which));
    if (!nanoseconds) {
        return -38;
    }
    into->seconds = (i64)(nanoseconds / 1000000000ull);
    into->nanoseconds = (i64)(nanoseconds % 1000000000ull);
    return 0;
}

int __vdso_gettimeofday(struct timeval *into, void *timezone)
{
    u64 nanoseconds = now(0);
    if (!nanoseconds) {
        return -38;
    }
    if (into) {
        into->seconds = (i64)(nanoseconds / 1000000000ull);
        into->microseconds = (i64)(nanoseconds % 1000000000ull / 1000);
    }
    /* The timezone argument has been obsolete since 4.2BSD and Linux fills
     * it with zeroes rather than refusing. */
    if (timezone) {
        ((u32 *)timezone)[0] = 0;
        ((u32 *)timezone)[1] = 0;
    }
    return 0;
}

i64 __vdso_time(i64 *into)
{
    u64 nanoseconds = now(0);
    if (!nanoseconds) {
        return -38;
    }
    i64 seconds = (i64)(nanoseconds / 1000000000ull);
    if (into) {
        *into = seconds;
    }
    return seconds;
}

int __vdso_clock_getres(int which, struct timespec *into)
{
    if (!known_clock(which)) {
        return -38;
    }
    if (into) {
        into->seconds = 0;
        /* One nanosecond, which is what the store's paths are denominated
         * in and therefore the truth rather than a flattering number. */
        into->nanoseconds = 1;
    }
    return 0;
}

/* One processor, and it is processor zero. A container is not scheduled
 * across sockets and `sched_getcpu` asking is how a program finds out. */
int __vdso_getcpu(u32 *cpu, u32 *node, void *unused)
{
    (void)unused;
    if (cpu) {
        *cpu = 0;
    }
    if (node) {
        *node = 0;
    }
    return 0;
}
