/* The x87 stack, reached the way a C compiler reaches it.
 *
 * `long double` on x86-64 is the eighty-bit extended format, and nothing but
 * the x87 can hold one. So every function here compiles to loads, stores,
 * arithmetic and compares on the stack — the instructions the translator
 * lowers into the `x87` crate — and the answers have to match a native run
 * bit for bit.
 *
 * The boundary is deliberately `double` and `long`, never `long double`:
 * SysV passes an extended value on the stack and returns it in `st0`, and
 * the harness's typed wrappers speak integers and doubles. So the extended
 * work happens inside and only its result crosses.
 */

/* The ten-byte format on its own, before any arithmetic. A compiler folds a
 * constant expression in extended precision and emits the answer as an m80
 * in `.rodata`, so these are a load and a store and nothing else — which is
 * what makes them the first thing to check when a longer chain is wrong in
 * its low bits. */
double constant_quarter(double unused) {
	(void)unused;
	long double t = 0.25L;
	return (double)t;
}

double constant_third(double unused) {
	(void)unused;
	long double t = 0.25L / 3.0L;
	return (double)t;
}

double load_add_store(double a) {
	long double t = (long double)a + 0.25L;
	return (double)t;
}

/* The same chain, one step at a time, so a disagreement says which step. */
double chain_step1(double a, double b) {
	long double t = (long double)a * (long double)b;
	return (double)t;
}

double chain_step2(double a, double b) {
	long double t = (long double)a * (long double)b;
	t = t + 0.25L;
	return (double)t;
}

double chain_step3(double a, double b) {
	long double t = (long double)a * (long double)b;
	t = t + 0.25L;
	t = t / 3.0L;
	return (double)t;
}

/* The subtract on its own, in the shape the chain has it: an extended
 * temporary spilled to the frame, the argument reloaded as a double beneath
 * it, and a register-form subtract whose direction the AT&T spelling
 * reverses. */
double subtract_extended(double a) {
	long double t = 0.25L / 3.0L;
	t = t - (long double)a;
	return (double)t;
}

/* The bits an extended value carries *below* what a `double` can hold.
 *
 * A step that rounds its answer to `double` on the way out hides a
 * difference in those bits; a longer chain that keeps the value extended
 * exposes it. This subtracts the double-rounded value back off and scales
 * what is left, so the residual itself is what gets compared.
 */
double divide_residual(double a, double b) {
	long double t = (long double)a * (long double)b;
	t = t + 0.25L;
	t = t / 3.0L;
	long double rounded = (long double)(double)t;
	return (double)((t - rounded) * 18446744073709551616.0L);
}

/* The significand itself, straight out of the ten-byte storage form.
 *
 * Everything else here compares a value that has been rounded back to a
 * `double` somewhere on its way out, which cannot see a difference in the
 * eleven bits an extended value carries beyond one. This copies the bytes.
 */
long divide_significand(double a, double b) {
	long double t = (long double)a * (long double)b;
	t = t + 0.25L;
	t = t / 3.0L;
	/* No initialiser: the copy below fills every byte read from it, and an
	 * initialiser would have clang call `memset`, which the corpus has no
	 * libc to link. */
	unsigned char raw[10];
	__builtin_memcpy(raw, &t, 10);
	long bits = 0;
	for (int index = 0; index < 8; index++) {
		bits |= (long)raw[index] << (index * 8);
	}
	return bits;
}

long divide_exponent(double a, double b) {
	long double t = (long double)a * (long double)b;
	t = t + 0.25L;
	t = t / 3.0L;
	/* No initialiser: the copy below fills every byte read from it, and an
	 * initialiser would have clang call `memset`, which the corpus has no
	 * libc to link. */
	unsigned char raw[10];
	__builtin_memcpy(raw, &t, 10);
	return (long)raw[8] | ((long)raw[9] << 8);
}

/* Arithmetic in extended precision, which is the whole point: the
 * intermediate holds more than a `double` can, so a translation that
 * quietly computed in `double` gives a different answer. */
double extended_chain(double a, double b) {
	long double t = (long double)a * (long double)b;
	t = t + 0.25L;
	t = t / 3.0L;
	t = t - (long double)a;
	return (double)t;
}

/* Enough live values to force the compiler to spill through memory, which
 * is what makes it emit `fstpt`/`fldt` — the ten-byte format the crate owns
 * rather than the translator. */
double spilled_chain(double a, double b) {
	long double p = (long double)a * 1.0000000001L;
	long double q = (long double)b * 1.0000000002L;
	long double r = p + q;
	long double s = p - q;
	long double t = p * q;
	long double u = p / q;
	long double v = r + s * t - u;
	return (double)(v + p + q + r + s + t + u);
}

/* Every cast direction. `(long)x` is the interesting one: without SSE3 the
 * compiler has no `fisttp`, so it saves the control word, sets round-to-
 * zero, converts, and restores — the `fnstcw`/`fldcw` dance. */
long to_integer(double a) {
	long double t = (long double)a * 3.5L;
	return (long)t;
}

double from_integer(long a) {
	long double t = (long double)a;
	t = t / 7.0L;
	return (double)t;
}

double round_trip_float(double a) {
	long double t = (long double)(float)a;
	t = t * 2.0L;
	return (double)(float)(double)t;
}

/* Compares that branch. These become `fucomi`/`fucomip` and a conditional
 * jump, so a wrong flag unpacking takes the wrong branch. */
long compare_branch(double a, double b) {
	long double x = (long double)a * 1.5L;
	long double y = (long double)b * 1.5L;
	long answer = 0;
	if (x > y) {
		answer |= 1;
	}
	if (x < y) {
		answer |= 2;
	}
	if (x == y) {
		answer |= 4;
	}
	if (x >= y) {
		answer |= 8;
	}
	if (!(x <= y)) {
		answer |= 16;
	}
	return answer;
}

/* Unordered, which is the case the parity flag exists for. */
long compare_unordered(double a, double b) {
	long double x = (long double)a;
	long double y = (long double)b;
	long answer = 0;
	if (x < y) {
		answer |= 1;
	}
	if (x > y) {
		answer |= 2;
	}
	if (x == y) {
		answer |= 4;
	}
	/* Neither less, greater nor equal: unordered. */
	if (!(x < y) && !(x > y) && !(x == y)) {
		answer |= 8;
	}
	return answer;
}

double magnitude(double a) {
	long double t = (long double)a;
	if (t < 0.0L) {
		t = -t;
	}
	return (double)(t + 1.0L);
}

/* An accumulation loop of the shape a `strtold` has: digits folded into an
 * extended accumulator, where doing it in `double` loses the low bits. */
double accumulate_digits(long seed, long count) {
	long double value = 0.0L;
	long digit = seed;
	if (count < 0) {
		count = 0;
	}
	if (count > 40) {
		count = 40;
	}
	for (long index = 0; index < count; index++) {
		digit = (digit * 7 + 3) % 10;
		value = value * 10.0L + (long double)digit;
	}
	return (double)(value / 1000000.0L);
}

/* Values small enough that the product is denormal as a `double` but normal
 * as an extended — the range where the two formats visibly differ. */
long denormal_class(double a) {
	long double t = (long double)a * (long double)a;
	long answer = 0;
	if (t == 0.0L) {
		answer |= 1;
	}
	if (t > 0.0L) {
		answer |= 2;
	}
	if ((double)t == 0.0) {
		answer |= 4;
	}
	return answer;
}
