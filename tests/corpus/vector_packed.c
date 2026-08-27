/* Float-plan milestone 5: packed operations and the bit idioms.

   Two families meet here. The loops are shaped so that compilers actually
   vectorise them at `-O2` and `-O3` — a plain accumulation over an array,
   a scaling pass, an element-wise combine — which is where packed arithmetic
   comes from in code nobody wrote with vectors in mind. The idioms are the
   other half: `fabs`, negation, `copysign` and branchless selection all
   compile to bitwise operations against masks in read-only data, and to
   compare masks, not to anything that looks like arithmetic.

   Every function seeds its own arrays from its argument, so each is
   self-contained. Values cross the boundary naturally now that the wrapper
   carries floats. */

#define COUNT 64

static double doubles[COUNT];
static float singles[COUNT];
static int words[COUNT];
static long quads[COUNT];

static void seed(double value, int count)
{
	for (int index = 0; index < COUNT; index++) {
		doubles[index] = value * (double)(index + 1) + (double)index;
		singles[index] = (float)(value * 0.5) + (float)index;
		words[index] = (int)(index * 7 + count);
		quads[index] = (long)index * 1000003L + (long)count;
	}
}

static int clamped(int count)
{
	if (count < 0) {
		return 0;
	}
	return count > COUNT ? COUNT : count;
}

/* ---- loops a compiler will vectorise ------------------------------------ */

double sum_doubles(double value, int count)
{
	seed(value, count);
	double total = 0.0;
	int limit = clamped(count);
	for (int index = 0; index < limit; index++) {
		total += doubles[index];
	}
	return total;
}

double scale_doubles(double value, int count)
{
	seed(value, count);
	int limit = clamped(count);
	for (int index = 0; index < limit; index++) {
		doubles[index] = doubles[index] * 3.5 + 1.25;
	}
	double total = 0.0;
	for (int index = 0; index < COUNT; index++) {
		total += doubles[index] * (double)(index & 3);
	}
	return total;
}

float sum_singles(double value, int count)
{
	seed(value, count);
	float total = 0.0f;
	int limit = clamped(count);
	for (int index = 0; index < limit; index++) {
		total += singles[index] * 2.0f;
	}
	return total;
}

int sum_words(double value, int count)
{
	seed(value, count);
	int total = 0;
	int limit = clamped(count);
	for (int index = 0; index < limit; index++) {
		total += words[index] * 3;
	}
	return total;
}

long sum_quads(double value, int count)
{
	seed(value, count);
	long total = 0;
	int limit = clamped(count);
	for (int index = 0; index < limit; index++) {
		total += quads[index] ^ (long)index;
	}
	return total;
}

/* An element-wise combine of two arrays, which vectorises into a
   load-operate-store loop rather than a reduction. */
long combine_words(double value, int count)
{
	seed(value, count);
	int limit = clamped(count);
	for (int index = 0; index < limit; index++) {
		words[index] = words[index] * 5 - (int)quads[index];
	}
	long total = 0;
	for (int index = 0; index < COUNT; index++) {
		total += (long)words[index];
	}
	return total;
}

/* A reduction that has to compare rather than accumulate, which is where the
   packed minimum and maximum appear. */
double largest_double(double value, int count)
{
	seed(value, count);
	int limit = clamped(count);
	if (limit == 0) {
		return 0.0;
	}
	double best = doubles[0];
	for (int index = 1; index < limit; index++) {
		if (doubles[index] > best) {
			best = doubles[index];
		}
	}
	return best;
}

/* ---- the bit idioms ------------------------------------------------------ */

/* All three of these are bitwise operations against a mask in read-only data
   rather than arithmetic, which is why the design counts them as part of the
   vector work rather than the floating-point work. */
double absolute_double(double value)
{
	return __builtin_fabs(value);
}

double negated_double(double value)
{
	return -value;
}

double copied_sign(double magnitude, double sign)
{
	return __builtin_copysign(magnitude, sign);
}

float absolute_single(float value)
{
	return __builtin_fabsf(value);
}

float negated_single(float value)
{
	return -value;
}

float copied_sign_single(float magnitude, float sign)
{
	return __builtin_copysignf(magnitude, sign);
}

/* Branchless selection: a compare producing a mask, then the mask used to
   pick between two values without a branch. */
double branchless_select(double left, double right, double threshold)
{
	double chosen = left < threshold ? left * 2.0 : right * 3.0;
	return chosen + (left == right ? 1.0 : -1.0);
}

/* The sign bit on its own. Compilers reach the sign of a floating-point value
   by gathering the lane sign bits into an integer register, which is the one
   instruction that crosses from the vector file to the integer one carrying
   something other than a value.

   The result is normalised to nought or one because the standard promises
   only that `signbit` returns something non-zero for a negative value, and
   compilers differ on which non-zero — which would make two configurations
   disagree about what the *source* computes rather than about the
   translation. */
int sign_of_double(double value)
{
	return __builtin_signbit(value) != 0;
}

int sign_of_single(float value)
{
	return __builtin_signbit(value) != 0;
}

/* The same gathering, but reached from a loop rather than from a single
   value, so that the wide form appears as well as the scalar one. */
int any_negative(double value, int count)
{
	seed(value, count);
	int limit = clamped(count);
	int found = 0;
	for (int index = 0; index < limit; index++) {
		if (doubles[index] < 0.0) {
			found = 1;
		}
	}
	return found;
}

/* The absolute value of a difference, which compilers spell as a subtraction
   followed by a mask rather than as a comparison. */
double absolute_difference(double left, double right)
{
	return __builtin_fabs(left - right);
}

/* A sum of absolute values over an array: the idiom and the loop at once,
   which is where a vectorised `andpd` against a broadcast mask appears. */
double sum_of_magnitudes(double value, int count)
{
	seed(value, count);
	double total = 0.0;
	int limit = clamped(count);
	for (int index = 0; index < limit; index++) {
		total += __builtin_fabs(doubles[index]);
	}
	return total;
}
