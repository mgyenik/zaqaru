/* The corpus signature inference is graded against.
 *
 * Every function here is global, because only a global function needs a
 * signature: a local symbol's contract is between the compiler and itself,
 * and after interprocedural register allocation it is frequently not SysV at
 * all. Local functions are also unstable to name — gcc renames them with
 * `.constprop` and `.isra` suffixes at higher optimisation levels — which
 * would make an exact-match test measure the compiler rather than the
 * analysis.
 *
 * Types are fixed-width for the same reason the interop corpus is: `long` and
 * pointers are eight bytes here and four on the wasm32 side, so a source that
 * used them would be asking two different questions at once.
 *
 * `signatures.expected` says what inference must produce for each of these,
 * and is where the cases inference cannot get right are recorded — with the
 * reason, so that a limitation stays visible instead of being absorbed.
 */

/* ---- the plain shapes, one per register file and width ---- */

int add_ints(int first, int second) { return first + second; }

long long add_longs(long long first, long long second) { return first + second; }

double add_doubles(double first, double second) { return first + second; }

float add_floats(float first, float second) { return first + second; }

/* ---- every argument register of each file ---- */

int six_integers(int a, int b, int c, int d, int e, int f) {
    return a + b + c + d + e + f;
}

double eight_doubles(double a, double b, double c, double d, double e, double f,
                     double g, double h) {
    return a + b + c + d + e + f + g + h;
}

/* ---- pointers, which are addresses and therefore 32 bits on wasm32 even
 * though the guest holds them in 64-bit registers ---- */

int dereference(const int *slot) { return *slot; }

void store_through(int *slot, int value) { *slot = value; }

int sum_array(const int *values, int count) {
    int total = 0;
    for (int index = 0; index < count; index++) {
        total += values[index];
    }
    return total;
}

/* ---- nothing in, nothing out ---- */

int global_counter;

void bump_counter(void) { global_counter++; }

/* ---- the interprocedural cases: a function that hands its arguments
 * straight on never touches the registers itself, so its own body says
 * nothing about what it takes ---- */

int forward_both(int first, int second) { return add_ints(first, second); }

double forward_double(double first, double second) {
    return add_doubles(first, second);
}

int forward_and_adjust(int value) { return add_ints(value, 5) + 1; }

/* Recursion, so the call graph has a cycle and the fixpoint has to converge
 * rather than walk a topological order. */
int countdown(int value) { return value <= 0 ? 0 : countdown(value - 1) + 1; }

/* Mutual recursion, the same thing across two nodes. */
int is_even(int value);
int is_odd(int value) { return value == 0 ? 0 : is_even(value - 1); }
int is_even(int value) { return value == 0 ? 1 : is_odd(value - 1); }

/* ---- narrowing and widening, where the register width and the C type
 * deliberately disagree ---- */

long long widen(int value) { return (long long)value * 3; }

int narrow(long long value) { return (int)(value / 3); }

/* ---- a trailing parameter the body never reads, which is the case SysV's
 * in-order assignment cannot recover: nothing in the machine code
 * distinguishes an ignored second argument from there being none ---- */

int ignores_second(int first, int second) {
    (void)second;
    return first + 1;
}
