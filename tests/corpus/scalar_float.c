/* Float-plan milestone 3: scalar floating-point arithmetic, compares and
   conversions.

   Values cross the host boundary as *bits* in integer arguments and integer
   returns, and are copied into a `double` or a `float` inside. Two reasons,
   both from the plan: the host-entry wrapper cannot carry floats until the
   next milestone, and comparing bits makes the differential check exact by
   construction — no tolerance anywhere, and a result that differs in its last
   place is a failure rather than a rounding opinion.

   Wasm and SSE are both IEEE-754 round-to-nearest-even, which is the mode
   `MXCSR` starts in, so every arithmetic result below is bit-exact. The three
   places where the naive mapping is *wrong* — `min`/`max` on ties and NaN,
   truncating conversions out of range, and how a compare reports unordered —
   each have functions of their own here. */

typedef unsigned long bits64;
typedef unsigned int bits32;

static double as_double(bits64 bits)
{
	double value;
	__builtin_memcpy(&value, &bits, sizeof value);
	return value;
}

static bits64 double_bits(double value)
{
	bits64 bits;
	__builtin_memcpy(&bits, &value, sizeof bits);
	return bits;
}

static float as_float(bits32 bits)
{
	float value;
	__builtin_memcpy(&value, &bits, sizeof value);
	return value;
}

static bits64 float_bits(float value)
{
	bits32 bits;
	__builtin_memcpy(&bits, &value, sizeof bits);
	return bits;
}

/* ---- double-precision arithmetic ---------------------------------------- */

bits64 double_add(bits64 left, bits64 right)
{
	return double_bits(as_double(left) + as_double(right));
}

bits64 double_subtract(bits64 left, bits64 right)
{
	return double_bits(as_double(left) - as_double(right));
}

bits64 double_multiply(bits64 left, bits64 right)
{
	return double_bits(as_double(left) * as_double(right));
}

bits64 double_divide(bits64 left, bits64 right)
{
	return double_bits(as_double(left) / as_double(right));
}

bits64 double_square_root(bits64 value)
{
	return double_bits(__builtin_sqrt(as_double(value)));
}

/* A chain, so that a result stays in a register across several operations
   rather than round-tripping through memory between each. */
bits64 double_chain(bits64 left, bits64 right)
{
	double a = as_double(left);
	double b = as_double(right);
	double sum = a + b;
	double product = a * b;
	return double_bits((sum - product) / (a + 1.0) + __builtin_sqrt(product * product));
}

/* ---- single-precision arithmetic ---------------------------------------- */

bits64 float_add(bits32 left, bits32 right)
{
	return float_bits(as_float(left) + as_float(right));
}

bits64 float_subtract(bits32 left, bits32 right)
{
	return float_bits(as_float(left) - as_float(right));
}

bits64 float_multiply(bits32 left, bits32 right)
{
	return float_bits(as_float(left) * as_float(right));
}

bits64 float_divide(bits32 left, bits32 right)
{
	return float_bits(as_float(left) / as_float(right));
}

bits64 float_square_root(bits32 value)
{
	return float_bits(__builtin_sqrtf(as_float(value)));
}

/* ---- the extremum rule --------------------------------------------------- */

/* `a < b ? a : b` is exactly what `minsd` computes, tie and NaN behaviour
   included, which is why compilers spell the ternary with it directly. The
   wasm `f64.min` would answer differently for both, so this is where that
   shows. */
bits64 double_minimum(bits64 left, bits64 right)
{
	double a = as_double(left);
	double b = as_double(right);
	return double_bits(a < b ? a : b);
}

bits64 double_maximum(bits64 left, bits64 right)
{
	double a = as_double(left);
	double b = as_double(right);
	return double_bits(a > b ? a : b);
}

bits64 float_minimum(bits32 left, bits32 right)
{
	float a = as_float(left);
	float b = as_float(right);
	return float_bits(a < b ? a : b);
}

bits64 float_maximum(bits32 left, bits32 right)
{
	float a = as_float(left);
	float b = as_float(right);
	return float_bits(a > b ? a : b);
}

/* ---- compares, including the unordered case ------------------------------ */

/* Every relation at once, so that one function's answer pins down the whole
   flag table rather than one row of it. */
int double_relations(bits64 left, bits64 right)
{
	double a = as_double(left);
	double b = as_double(right);
	int answer = 0;
	if (a < b) {
		answer |= 1;
	}
	if (a <= b) {
		answer |= 2;
	}
	if (a == b) {
		answer |= 4;
	}
	if (a != b) {
		answer |= 8;
	}
	if (a > b) {
		answer |= 16;
	}
	if (a >= b) {
		answer |= 32;
	}
	/* An unordered pair is neither less nor greater nor equal, which is the
	   parity flag's whole reason for existing. */
	if (!(a < b) && !(a > b) && !(a == b)) {
		answer |= 64;
	}
	return answer;
}

int float_relations(bits32 left, bits32 right)
{
	float a = as_float(left);
	float b = as_float(right);
	int answer = 0;
	if (a < b) {
		answer |= 1;
	}
	if (a <= b) {
		answer |= 2;
	}
	if (a == b) {
		answer |= 4;
	}
	if (a != b) {
		answer |= 8;
	}
	if (a > b) {
		answer |= 16;
	}
	if (a >= b) {
		answer |= 32;
	}
	if (!(a < b) && !(a > b) && !(a == b)) {
		answer |= 64;
	}
	return answer;
}

/* A compare feeding a branch rather than a `setcc`, which is the shape
   `jp`/`jnp` actually appear in. */
bits64 double_ordered_sum(bits64 left, bits64 right)
{
	double a = as_double(left);
	double b = as_double(right);
	if (a < b) {
		return double_bits(b - a);
	}
	if (a > b) {
		return double_bits(a - b);
	}
	if (a == b) {
		return double_bits(a + b);
	}
	/* Reached only when a pair is unordered. */
	return double_bits(-1.0);
}

/* ---- conversions --------------------------------------------------------- */

/* A C cast from a floating-point type to an integer one truncates towards
   zero and is undefined when the value does not fit — which on x86 means the
   integer indefinite, and in wasm would mean a trap. */
int double_to_int(bits64 value)
{
	return (int)as_double(value);
}

long double_to_long(bits64 value)
{
	return (long)as_double(value);
}

int float_to_int(bits32 value)
{
	return (int)as_float(value);
}

long float_to_long(bits32 value)
{
	return (long)as_float(value);
}

bits64 int_to_double(int value)
{
	return double_bits((double)value);
}

bits64 long_to_double(long value)
{
	return double_bits((double)value);
}

bits64 int_to_float(int value)
{
	return float_bits((float)value);
}

bits64 long_to_float(long value)
{
	return float_bits((float)value);
}

bits64 double_to_float(bits64 value)
{
	return float_bits((float)as_double(value));
}

bits64 float_to_double(bits32 value)
{
	return double_bits((double)as_float(value));
}

/* A conversion round trip through both widths and back, so a mask that lost
   the upper half of a lane somewhere shows up as a wrong number. */
bits64 resize_round_trip(bits64 value)
{
	double original = as_double(value);
	float narrowed = (float)original;
	double widened = (double)narrowed;
	return double_bits(widened + original);
}
