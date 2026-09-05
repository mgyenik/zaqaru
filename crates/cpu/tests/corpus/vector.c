/* SSE, held to hardware lane by lane.
 *
 * The arguments arrive as integers and are bit-cast into floats rather than
 * written as literals, so the sweep reaches NaNs, infinities, denormals and
 * negative zero without anybody having to think of them: those are exactly
 * the operands where a plausible-looking implementation differs from the
 * machine, and exactly the ones a hand-written literal never covers.
 */
#include "lockstep.h"

typedef char v16qi __attribute__((vector_size(16)));
typedef short v8hi __attribute__((vector_size(16)));
typedef int v4si __attribute__((vector_size(16)));
typedef long v2di __attribute__((vector_size(16)));
typedef float v4sf __attribute__((vector_size(16)));
typedef double v2df __attribute__((vector_size(16)));

static double as_double(long bits)
{
	union {
		long bits;
		double value;
	} cast = { bits };
	return cast.value;
}

static float as_float(long bits)
{
	union {
		int bits;
		float value;
	} cast = { (int)bits };
	return cast.value;
}

static long as_bits(double value)
{
	union {
		double value;
		long bits;
	} cast = { value };
	return cast.bits;
}

long probe_scalar_double(long a, long b)
{
	double left = as_double(a);
	double right = as_double(b);
	double sum = left + right;
	double difference = left - right;
	double product = left * right;
	double quotient = left / right;
	return as_bits(sum) ^ as_bits(difference) ^ as_bits(product) ^ as_bits(quotient);
}

long probe_scalar_single(long a, long b)
{
	float left = as_float(a);
	float right = as_float(b);
	float sum = left + right;
	float difference = left - right;
	float product = left * right;
	float quotient = left / right;
	return (long)(sum + difference + product + quotient);
}

long probe_square_root(long a, long b)
{
	double value = __builtin_sqrt(as_double(a));
	float narrow = __builtin_sqrtf(as_float(b));
	return as_bits(value) ^ (long)narrow;
}

long probe_minimum_maximum(long a, long b)
{
	/* The two cases where x86's min and max are not IEEE's: a NaN
	 * anywhere answers the second operand, and so do two zeros of
	 * opposite sign. The sweep supplies both. */
	double left = as_double(a);
	double right = as_double(b);
	double low, high;
	__asm__ volatile("minsd %[right], %[low]\n\t"
			 "maxsd %[right], %[high]"
			 : [low] "=&x"(low), [high] "=&x"(high)
			 : [right] "x"(right), "0"(left), "1"(left));
	return as_bits(low) ^ as_bits(high);
}

long probe_ordered_compare(long a, long b)
{
	/* `ucomisd` is the only floating-point instruction that writes the
	 * integer flags, and parity is how it reports unordered — which is
	 * the entire reason the parity flag is modelled at all. */
	double left = as_double(a);
	double right = as_double(b);
	long answer = 0;
	answer += (left == right);
	answer += (left < right) << 1;
	answer += (left <= right) << 2;
	answer += (left > right) << 3;
	answer += (left != right) << 4;
	answer += __builtin_isunordered(left, right) << 5;
	return answer;
}

long probe_conversions(long a, long b)
{
	double value = as_double(a);
	float narrow = as_float(b);
	long truncated = (long)value;
	int narrowed = (int)value;
	double widened = (double)narrow;
	float shortened = (float)value;
	double from_integer = (double)b;
	float from_integer_single = (float)(int)b;
	return truncated ^ narrowed ^ as_bits(widened) ^ (long)shortened ^
	       as_bits(from_integer) ^ (long)from_integer_single;
}

long probe_packed_double(long a, long b)
{
	v2df left = { as_double(a), as_double(b) };
	v2df right = { as_double(b), as_double(a) };
	v2df sum = left + right;
	v2df product = left * right;
	v2df quotient = left / right;
	return as_bits(sum[0]) ^ as_bits(product[1]) ^ as_bits(quotient[0]);
}

long probe_packed_single(long a, long b)
{
	v4sf left = { as_float(a), as_float(b), as_float(a ^ b), as_float(a + b) };
	v4sf right = { as_float(b), as_float(a), as_float(a - b), as_float(~a) };
	v4sf sum = left + right;
	v4sf product = left * right;
	return (long)(sum[0] + sum[3] + product[1] + product[2]);
}

long probe_packed_integer(long a, long b)
{
	v16qi bytes = { (char)a,	   (char)b,	  (char)(a >> 8),  (char)(b >> 8),
			(char)(a >> 16),   (char)(b >> 16), (char)(a >> 24), (char)(b >> 24),
			(char)(a >> 32),   (char)(b >> 32), (char)(a >> 40), (char)(b >> 40),
			(char)(a >> 48),   (char)(b >> 48), (char)(a >> 56), (char)(b >> 56) };
	v8hi words = { (short)a, (short)b, (short)(a >> 16), (short)(b >> 16),
		       (short)(a >> 32), (short)(b >> 32), (short)(a >> 48), (short)(b >> 48) };
	v4si dwords = { (int)a, (int)b, (int)(a >> 32), (int)(b >> 32) };
	v2di qwords = { a, b };
	v16qi byte_sum = bytes + bytes;
	v8hi word_product = words * words;
	v4si dword_difference = dwords - dwords;
	v2di qword_sum = qwords + qwords;
	return byte_sum[3] + word_product[2] + dword_difference[1] + qword_sum[0];
}

long probe_packed_compare(long a, long b)
{
	v4si left = { (int)a, (int)b, (int)(a >> 32), (int)(b >> 32) };
	v4si right = { (int)b, (int)a, (int)(b >> 32), (int)(a >> 32) };
	v4si equal = (left == right);
	v4si greater = (left > right);
	return equal[0] + greater[1] + equal[2] + greater[3];
}

long probe_shifts_and_masks(long a, long b)
{
	v8hi words = { (short)a, (short)b, (short)(a >> 16), (short)(b >> 16),
		       (short)(a >> 32), (short)(b >> 32), (short)(a >> 48), (short)(b >> 48) };
	v4si dwords = { (int)a, (int)b, (int)(a >> 32), (int)(b >> 32) };
	v8hi left = words << 3;
	v8hi right = words >> 5;
	v4si arithmetic = dwords >> 7;
	return left[1] + right[2] + arithmetic[3];
}

long probe_sign_mask(long a, long b)
{
	/* `pmovmskb` and its two floating-point siblings: how a vectorised
	 * `strlen` turns sixteen lanes into a branch. */
	v16qi bytes = { (char)a,	 (char)b,	(char)(a >> 8),  (char)(b >> 8),
			(char)(a >> 16), (char)(b >> 16), (char)(a >> 24), (char)(b >> 24),
			(char)(a >> 32), (char)(b >> 32), (char)(a >> 40), (char)(b >> 40),
			(char)(a >> 48), (char)(b >> 48), (char)(a >> 56), (char)(b >> 56) };
	v4sf singles = { as_float(a), as_float(b), as_float(a ^ b), as_float(~b) };
	v2df doubles = { as_double(a), as_double(b) };
	int byte_mask, single_mask, double_mask;
	__asm__ volatile("pmovmskb %[bytes], %[byte_mask]\n\t"
			 "movmskps %[singles], %[single_mask]\n\t"
			 "movmskpd %[doubles], %[double_mask]"
			 : [byte_mask] "=r"(byte_mask), [single_mask] "=r"(single_mask),
			   [double_mask] "=r"(double_mask)
			 : [bytes] "x"(bytes), [singles] "x"(singles), [doubles] "x"(doubles));
	return byte_mask + single_mask + double_mask;
}

long probe_shuffles(long a, long b)
{
	v4si dwords = { (int)a, (int)b, (int)(a >> 32), (int)(b >> 32) };
	v4si shuffled;
	v8hi words = { (short)a, (short)b, (short)(a >> 16), (short)(b >> 16),
		       (short)(a >> 32), (short)(b >> 32), (short)(a >> 48), (short)(b >> 48) };
	v8hi low_shuffled, high_shuffled;
	__asm__ volatile("pshufd $0x1b, %[in], %[out]"
			 : [out] "=x"(shuffled)
			 : [in] "x"(dwords));
	__asm__ volatile("pshuflw $0x39, %[in], %[out]"
			 : [out] "=x"(low_shuffled)
			 : [in] "x"(words));
	__asm__ volatile("pshufhw $0x93, %[in], %[out]"
			 : [out] "=x"(high_shuffled)
			 : [in] "x"(words));
	return shuffled[0] + low_shuffled[1] + high_shuffled[6];
}

long probe_interleave_and_pack(long a, long b)
{
	v16qi left = { (char)a,		(char)b,	 (char)(a >> 8),  (char)(b >> 8),
		       (char)(a >> 16), (char)(b >> 16), (char)(a >> 24), (char)(b >> 24),
		       (char)(a >> 32), (char)(b >> 32), (char)(a >> 40), (char)(b >> 40),
		       (char)(a >> 48), (char)(b >> 48), (char)(a >> 56), (char)(b >> 56) };
	v8hi words = { (short)a, (short)b, (short)(a >> 16), (short)(b >> 16),
		       (short)(a >> 32), (short)(b >> 32), (short)(a >> 48), (short)(b >> 48) };
	v16qi unpacked, packed_signed, packed_unsigned;
	__asm__ volatile("punpcklbw %[right], %[out]"
			 : [out] "=x"(unpacked)
			 : [right] "x"(left), "0"(left));
	__asm__ volatile("packsswb %[right], %[out]"
			 : [out] "=x"(packed_signed)
			 : [right] "x"(words), "0"(words));
	__asm__ volatile("packuswb %[right], %[out]"
			 : [out] "=x"(packed_unsigned)
			 : [right] "x"(words), "0"(words));
	return unpacked[5] + packed_signed[2] + packed_unsigned[9];
}

long probe_saturating(long a, long b)
{
	v8hi left = { (short)a, (short)b, (short)(a >> 16), (short)(b >> 16),
		      (short)(a >> 32), (short)(b >> 32), (short)(a >> 48), (short)(b >> 48) };
	v8hi right = { (short)b, (short)a, (short)(b >> 16), (short)(a >> 16),
		       (short)(b >> 32), (short)(a >> 32), (short)(b >> 48), (short)(a >> 48) };
	v8hi added, subtracted, added_unsigned;
	__asm__ volatile("paddsw %[right], %[out]"
			 : [out] "=x"(added)
			 : [right] "x"(right), "0"(left));
	__asm__ volatile("psubsw %[right], %[out]"
			 : [out] "=x"(subtracted)
			 : [right] "x"(right), "0"(left));
	__asm__ volatile("paddusw %[right], %[out]"
			 : [out] "=x"(added_unsigned)
			 : [right] "x"(right), "0"(left));
	return added[0] + subtracted[3] + added_unsigned[6];
}

long probe_aligned_and_unaligned_moves(long a, long b)
{
	static v2di aligned[4] __attribute__((aligned(16)));
	static char unaligned[80] __attribute__((aligned(16)));
	v2di value = { a, b };
	v2di loaded, loaded_unaligned;
	__asm__ volatile("movdqa %[value], %[slot]"
			 : [slot] "=m"(aligned[1])
			 : [value] "x"(value));
	__asm__ volatile("movdqa %[slot], %[out]"
			 : [out] "=x"(loaded)
			 : [slot] "m"(aligned[1]));
	__asm__ volatile("movdqu %[value], %[slot]"
			 : [slot] "=m"(unaligned[3])
			 : [value] "x"(value));
	__asm__ volatile("movdqu %[slot], %[out]"
			 : [out] "=x"(loaded_unaligned)
			 : [slot] "m"(unaligned[3]));
	return loaded[0] + loaded_unaligned[1];
}

long probe_moves_between_files(long a, long b)
{
	/* `movd`/`movq` in both directions, and the zeroing they do on the
	 * way into a vector register. */
	v2di value = { a, b };
	long back;
	int narrow;
	v2di moved;
	__asm__ volatile("movq %[a], %[moved]"
			 : [moved] "=x"(moved)
			 : [a] "r"(a));
	__asm__ volatile("movq %[value], %[back]"
			 : [back] "=r"(back)
			 : [value] "x"(value));
	__asm__ volatile("movd %[value], %[narrow]"
			 : [narrow] "=r"(narrow)
			 : [value] "x"(value));
	return moved[0] + moved[1] + back + narrow;
}

long probe_bitwise_lanes(long a, long b)
{
	v2di left = { a, b };
	v2di right = { b, a };
	v2di and_not;
	__asm__ volatile("pandn %[right], %[out]"
			 : [out] "=x"(and_not)
			 : [right] "x"(right), "0"(left));
	v2di conjunction = left & right;
	v2di disjunction = left | right;
	v2di difference = left ^ right;
	return and_not[0] + conjunction[1] + disjunction[0] + difference[1];
}

long probe_lane_widths(long a, long b)
{
	/* The lane widths the compiler happened not to choose. Coverage is
	 * the point: a packed add is not one instruction, it is four, and
	 * three of them are only reached by asking. */
	v8hi words = { (short)a, (short)b, (short)(a >> 16), (short)(b >> 16),
		       (short)(a >> 32), (short)(b >> 32), (short)(a >> 48), (short)(b >> 48) };
	v4si dwords = { (int)a, (int)b, (int)(a >> 32), (int)(b >> 32) };
	v8hi word_sum, word_shifted;
	v4si dword_sum, dword_difference;
	__asm__ volatile("paddw %[right], %[out]"
			 : [out] "=x"(word_sum)
			 : [right] "x"(words), "0"(words));
	__asm__ volatile("psrlw $3, %[out]"
			 : [out] "=x"(word_shifted)
			 : "0"(words));
	__asm__ volatile("paddd %[right], %[out]"
			 : [out] "=x"(dword_sum)
			 : [right] "x"(dwords), "0"(dwords));
	__asm__ volatile("psubd %[right], %[out]"
			 : [out] "=x"(dword_difference)
			 : [right] "x"(dwords), "0"(dwords));
	return word_sum[1] + word_shifted[2] + dword_sum[3] + dword_difference[0];
}

long probe_nan_propagation(long a, long b)
{
	/* The rules that decide which NaN comes out, which are x86's and not
	 * the ones a language's `+` happens to implement.
	 *
	 * Three separate rules, and each is a different arm:
	 *  - arithmetic propagates the *destination's* NaN, quieted, payload
	 *    and all, and only the source's when the destination is not one;
	 *  - an invalid operation produces the real indefinite, whose sign
	 *    bit is set — `0.0 / 0.0` is `0xffc00000`, not `0x7fc00000`;
	 *  - minimum and maximum answer the *second* operand whenever the
	 *    pair is unordered, unchanged, signalling NaN and all.
	 *
	 * Written in assembly with the operands placed by hand, because what
	 * is being pinned is which operand wins — and a compiler is free to
	 * commute the two. */
	float left = as_float(a);
	float right = as_float(b);
	float sum = left, difference = left, product = left, quotient = left;
	float low = left, high = left, root;
	float generated = 0.0f, zero = 0.0f;
	double wide_left = as_double(a), wide_right = as_double(b), wide_sum = wide_left;

	__asm__ volatile("addss %[right], %[sum]\n\t"
			 "subss %[right], %[difference]\n\t"
			 "mulss %[right], %[product]\n\t"
			 "divss %[right], %[quotient]\n\t"
			 "minss %[right], %[low]\n\t"
			 "maxss %[right], %[high]\n\t"
			 "sqrtss %[right], %[root]\n\t"
			 "divss %[zero], %[generated]\n\t"
			 "addsd %[wide_right], %[wide_sum]"
			 : [sum] "+x"(sum), [difference] "+x"(difference), [product] "+x"(product),
			   [quotient] "+x"(quotient), [low] "+x"(low), [high] "+x"(high),
			   [root] "=x"(root), [generated] "+x"(generated), [wide_sum] "+x"(wide_sum)
			 : [right] "x"(right), [zero] "x"(zero), [wide_right] "x"(wide_right));

	return (long)(sum + difference + product + quotient + low + high + root + generated) +
	       as_bits(wide_sum);
}

long probe_sse3(long a, long b)
{
	/* SSE3: the lane-duplicating movers and the horizontal family, which
	 * is what vectorised numerics reach for — a complex multiply needs
	 * the real part in both halves, and a dot product ends by folding a
	 * vector against itself. numpy's linear algebra is where these came
	 * up. */
	v2df doubles = { as_double(a), as_double(b) };
	v4sf singles = { as_float(a), as_float(b), as_float(a ^ b), as_float(~b) };
	v2df duplicated, horizontal_sum, horizontal_difference, alternating;
	v4sf shifted_high, shifted_low, single_sum, single_alternating;

	__asm__ volatile("movddup %[in], %[out]" : [out] "=x"(duplicated) : [in] "x"(doubles));
	__asm__ volatile("movshdup %[in], %[out]" : [out] "=x"(shifted_high) : [in] "x"(singles));
	__asm__ volatile("movsldup %[in], %[out]" : [out] "=x"(shifted_low) : [in] "x"(singles));
	__asm__ volatile("haddpd %[right], %[out]"
			 : [out] "=x"(horizontal_sum)
			 : [right] "x"(doubles), "0"(doubles));
	__asm__ volatile("hsubpd %[right], %[out]"
			 : [out] "=x"(horizontal_difference)
			 : [right] "x"(doubles), "0"(doubles));
	__asm__ volatile("addsubpd %[right], %[out]"
			 : [out] "=x"(alternating)
			 : [right] "x"(doubles), "0"(doubles));
	__asm__ volatile("haddps %[right], %[out]"
			 : [out] "=x"(single_sum)
			 : [right] "x"(singles), "0"(singles));
	__asm__ volatile("addsubps %[right], %[out]"
			 : [out] "=x"(single_alternating)
			 : [right] "x"(singles), "0"(singles));

	return as_bits(duplicated[1]) ^ as_bits(horizontal_sum[0]) ^
	       as_bits(horizontal_difference[1]) ^ as_bits(alternating[0]) +
	       (long)(shifted_high[0] + shifted_low[3] + single_sum[2] +
		      single_alternating[1]);
}
