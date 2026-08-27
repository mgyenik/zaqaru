/* The half of a mixed program that is never transpiled.
 *
 * Compiled twice and transpiled never: once for x86-64, where it is linked
 * beside the guest half to make the native oracle, and once for wasm32 by
 * clang's own backend, where it is linked beside the *transpiled* guest half.
 * The comparison between those two links is the whole test.
 *
 * `long` and pointer widths differ between those two targets, so nothing here
 * may depend on either. Sizes that matter are spelled `int` or `long long`,
 * and pointers are only passed and dereferenced, never measured. */

int guest_double(int value);
void guest_fill(int *slot, int value);

int foreign_scale(int value) { return value * 10; }

double foreign_blend(double first, double second) { return first * 3.0 - second; }

float foreign_narrow(float value) { return value * 0.5f; }

long long foreign_widen(int value) { return (long long)value * 1000003; }

void foreign_store(int *slot, int value) { *slot = value * 7; }

/* Deliberately deep in the shadow stack, and reading through a pointer into
 * the caller's frame at the same time. Both halves of that matter: the
 * `volatile` array is what makes a real frame get allocated, and the pointer
 * is what that frame would destroy if it were allocated in the wrong place. */
int foreign_reduce(const int *values, int count) {
    volatile int scratch[32];
    for (int index = 0; index < 32; index++) {
        scratch[index] = 0;
    }
    for (int index = 0; index < count; index++) {
        scratch[index & 31] += values[index];
    }
    int total = 0;
    for (int index = 0; index < 32; index++) {
        total += scratch[index];
    }
    return total;
}

/* Calls back into the transpiled half, with a live frame of its own across
 * the call. */
int foreign_round_trip(int value) {
    volatile int scratch[16];
    for (int index = 0; index < 16; index++) {
        scratch[index] = value + index;
    }
    int doubled = guest_double(value);
    int total = doubled;
    for (int index = 0; index < 16; index++) {
        total += scratch[index];
    }
    return total;
}

/* Hands the transpiled side a pointer into this frame and lets it write
 * there. The `volatile` array keeps a real frame alive across the call, so a
 * guest stack started in the wrong place would destroy it. */
int foreign_uses_guest_fill(int value) {
    volatile int scratch[16];
    for (int index = 0; index < 16; index++) {
        scratch[index] = index;
    }
    int slot = 0;
    guest_fill(&slot, value);
    int total = slot;
    for (int index = 0; index < 16; index++) {
        total += scratch[index];
    }
    return total;
}

/* A pointer created on this side for the transpiled side to write through.
 * The mirror of guest_uses_pointer, and the direction that proves the two
 * halves really do address one linear memory rather than merely agreeing
 * about integers: the storage lives in this object's data, and the guest
 * reaches it with an address this object chose. */
static int foreign_storage;

int *foreign_address(void) { return &foreign_storage; }

int foreign_read_back(void) { return foreign_storage; }

int foreign_mixed(int first, double second, float third, long long fourth) {
    double blended = second * 4.0 + (double)third;
    return first + (int)blended + (int)(fourth % 97);
}
