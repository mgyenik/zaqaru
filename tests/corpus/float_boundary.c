/* Float-plan milestone 4: functions with natural floating-point signatures.

   The previous milestone's corpus passed everything across the host boundary
   as bits in integer registers, because the uniform wrapper had nowhere else
   to put a `double`. Now that it fills both register files, these are written
   the way anyone would write them — `double` in, `double` out — and the
   harness is what knows which SysV slot each argument belongs in.

   Nothing here takes more than six integer or eight floating-point arguments:
   past that SysV puts them on the stack, which is a separate piece of work
   from carrying floats at all. */

double double_identity(double value)
{
	return value;
}

double double_sum(double left, double right)
{
	return left + right;
}

double double_blend(double a, double b, double c, double d)
{
	return (a + b) * (c - d) / (a * a + 1.0);
}

/* Every floating-point argument register at once, so a wrapper that filled
   only some of them shows up. */
double double_eight(double a, double b, double c, double d, double e, double f,
		    double g, double h)
{
	return a + b * 2.0 + c * 4.0 + d * 8.0 + e * 16.0 + f * 32.0 + g * 64.0
	       + h * 128.0;
}

float float_identity(float value)
{
	return value;
}

float float_sum(float left, float right)
{
	return left + right;
}

/* The same register sweep at single precision, where each argument occupies
   only the low half of its register. */
float float_eight(float a, float b, float c, float d, float e, float f, float g,
		  float h)
{
	return a + b * 2.0f + c * 4.0f + d * 8.0f + e * 16.0f + f * 32.0f
	       + g * 64.0f + h * 128.0f;
}

/* The two files filled at once, and interleaved in the source so that the
   slot each argument lands in is not the order it was written in. */
double mixed_arguments(int first, double second, long third, double fourth,
		       float fifth, int sixth)
{
	return (double)first + second + (double)third * 0.5 + fourth
	       + (double)fifth * 0.25 + (double)sixth * 0.125;
}

/* Floating-point arguments with an integer result, and the other way round:
   each direction leaves the other result register holding whatever it held. */
int compare_doubles(double left, double right)
{
	if (left < right) {
		return -1;
	}
	if (left > right) {
		return 1;
	}
	if (left == right) {
		return 0;
	}
	return 2;
}

long double_to_long_natural(double value)
{
	return (long)value;
}

double long_to_double_natural(long value)
{
	return (double)value;
}

float double_narrow(double value)
{
	return (float)value;
}

double float_widen(float value)
{
	return (double)value;
}

/* A guest-to-guest call carrying floats. Nothing special happens here — all
   state is in globals, so faithful emulation already moves them — but a
   corpus that only ever crossed the host boundary would not say so. */
static double weighted(double value, double weight)
{
	return value * weight + 1.0;
}

double apply_weights(double value, double first, double second)
{
	return weighted(weighted(value, first), second);
}

/* A loop accumulating in a register across iterations, which is where a
   scalar operation that failed to preserve the high lane of its destination
   would eventually show. */
double accumulate(double seed, double step, int count)
{
	double total = seed;
	for (int index = 0; index < count && index < 64; index++) {
		total = total * 1.5 + step;
	}
	return total;
}
