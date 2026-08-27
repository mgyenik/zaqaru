/* The transpiled half of a mixed program.
 *
 * Every `foreign_` function here is defined in interop_foreign.c, which is
 * compiled by clang's own wasm backend and never goes through the transpiler
 * at all. Calls to them leave this object undefined, and a generated thunk
 * turns each into a typed wasm call. The `guest_` functions travel the other
 * way: the foreign half calls them through the typed host-entry wrapper the
 * declarations give them.
 *
 * Fixed-width behaviour only. This source is compiled twice — once for
 * x86-64, once natively as the oracle — and its foreign counterpart is
 * compiled for both x86-64 and wasm32, where `long` and pointers are not the
 * same size. Nothing here may depend on that. */

int foreign_scale(int value);
double foreign_blend(double first, double second);
float foreign_narrow(float value);
long long foreign_widen(int value);
void foreign_store(int *slot, int value);
int foreign_reduce(const int *values, int count);
int foreign_round_trip(int value);
int foreign_mixed(int first, double second, float third, long long fourth);
int *foreign_address(void);
int foreign_read_back(void);
int foreign_uses_guest_fill(int value);

/* Called from the foreign side, so it needs a face an ordinary wasm module
 * can call. */
int guest_double(int value) { return value * 2 + 3; }

int guest_uses_scale(int value) { return foreign_scale(value) + 1; }

double guest_uses_blend(double first, double second) {
    return foreign_blend(first, second) * 2.0 + 1.0;
}

float guest_uses_narrow(float value) { return foreign_narrow(value) + 1.5f; }

long long guest_uses_widen(int value) { return foreign_widen(value) + 1; }

/* A pointer into the guest's own stack, dereferenced by code that has no idea
 * a guest stack exists. It works because both sides address one linear
 * memory — which is also why the thunk has to move the linker's stack pointer
 * below this frame before the call. */
int guest_uses_pointer(int value) {
    int slot = 0;
    foreign_store(&slot, value);
    return slot + 1;
}

int guest_uses_array(int seed) {
    int values[8];
    for (int index = 0; index < 8; index++) {
        values[index] = seed + index;
    }
    return foreign_reduce(values, 8) + 1;
}

/* Guest calls foreign calls guest: the case that proves the stack discipline
 * composes. The outer guest frame has to survive a foreign frame *and* a
 * second guest stack started inside it. */
int guest_round_trip(int value) {
    int slot = value + 11;
    int result = foreign_round_trip(value);
    return result + slot;
}

/* A pointer *parameter* on a transpiled function, which is the one shape the
 * typed wrapper had no coverage for: an address arriving from outside and
 * being written through. The address here points into the foreign side's own
 * frame, so the guest stack this call starts has to stay clear of it. */
void guest_fill(int *slot, int value) { *slot = value * 5 + 2; }

int guest_round_trip_pointer(int value) {
    return foreign_uses_guest_fill(value) + 1;
}

/* The other direction of pointer passing: the address comes from the far
 * side, and this side writes through it. Writing before reading keeps the
 * result independent of anything an earlier call left behind. */
int guest_uses_foreign_pointer(int value) {
    int *slot = foreign_address();
    *slot = value * 3 + 1;
    return foreign_read_back() + 2;
}

int guest_uses_mixed(int first, double second, float third, long long fourth) {
    return foreign_mixed(first, second, third, fourth) + 1;
}
