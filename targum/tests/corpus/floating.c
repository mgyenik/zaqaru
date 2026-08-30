/* The x87, held to hardware register by register.
 *
 * `long double` is the only way to reach the unit from C on this ABI —
 * `double` and `float` have been SSE's since the x86-64 ABI was written —
 * so the arithmetic probes are written in it, and everything the compiler
 * has no spelling for is written in assembly.
 *
 * The comparison is exact: the full eighty bits of each stack register, the
 * control word, and the status word with its condition codes and its sticky
 * exception flags. A rounding that is one bit off, a condition code set from
 * the wrong relation, a stack that ends one slot deep — each of those fails
 * here at the instruction that did it.
 */
#include "lockstep.h"

static long double as_extended(long bits)
{
	/* Built out of the argument rather than written as a literal, so the
	 * sweep reaches denormals, infinities and NaNs without anybody having
	 * to enumerate them. */
	union {
		long bits;
		double value;
	} cast = { bits };
	return (long double)cast.value;
}

long probe_extended_arithmetic(long a, long b)
{
	long double left = as_extended(a);
	long double right = as_extended(b);
	long double sum = left + right;
	long double difference = left - right;
	long double product = left * right;
	long double quotient = left / right;
	return (long)(double)(sum + difference + product + quotient);
}

long probe_extended_from_integers(long a, long b)
{
	/* `fild` in three widths, and `fist`/`fistp` back out. */
	long double value = (long double)a + (long double)(int)b + (long double)(short)b;
	long back = (long)value;
	int narrow = (int)value;
	return back ^ narrow;
}

long probe_extended_round_trip(long a, long b)
{
	/* `fstp m80` and `fld m80`: the ten-byte form, which has no width in
	 * the register file and travels as bytes. */
	static long double slot;
	long double value = as_extended(a) * as_extended(b);
	slot = value;
	__asm__ volatile("" ::: "memory");
	return (long)(double)(slot + value);
}

long probe_extended_comparison(long a, long b)
{
	long double left = as_extended(a);
	long double right = as_extended(b);
	long answer = 0;
	answer += (left == right);
	answer += (left < right) << 1;
	answer += (left > right) << 2;
	answer += __builtin_isunordered(left, right) << 3;
	return answer;
}

long probe_status_word(long a, long b)
{
	/* `fcom` into the status word, then `fnstsw %ax` to read it — the
	 * pre-`fcomi` idiom, which is what a compiler emits for a `long
	 * double` comparison at `-O0` and what glibc's own code uses. */
	long double left = as_extended(a);
	long double right = as_extended(b);
	unsigned short status;
	__asm__ volatile("fldt %[right]\n\t"
			 "fldt %[left]\n\t"
			 "fucomp %%st(1)\n\t"
			 "fnstsw %%ax\n\t"
			 "movw %%ax, %[status]\n\t"
			 "fstp %%st(0)"
			 : [status] "=r"(status)
			 : [left] "m"(left), [right] "m"(right)
			 : "ax", "st", "st(1)", "cc");
	return status;
}

long probe_stack_motion(long a, long b)
{
	/* `fxch`, `fincstp`, `fdecstp` and `ffree`: the stack itself, which
	 * is where an emulation that tracks TOP wrongly diverges silently. */
	long double first = as_extended(a);
	long double second = as_extended(b);
	long double result;
	__asm__ volatile("fldt %[first]\n\t"
			 "fldt %[second]\n\t"
			 "fxch %%st(1)\n\t"
			 "fincstp\n\t"
			 "fdecstp\n\t"
			 "faddp %%st, %%st(1)\n\t"
			 "fstpt %[result]"
			 : [result] "=m"(result)
			 : [first] "m"(first), [second] "m"(second)
			 : "st", "st(1)", "cc", "memory");
	return (long)(double)result;
}

long probe_unary_operations(long a, long b)
{
	long double value = as_extended(a);
	long double other = as_extended(b);
	long double root, negated, absolute, rounded, remainder, scaled;
	__asm__ volatile("fldt %[value]\n\t"
			 "fsqrt\n\t"
			 "fstpt %[root]\n\t"
			 "fldt %[value]\n\t"
			 "fchs\n\t"
			 "fstpt %[negated]\n\t"
			 "fldt %[value]\n\t"
			 "fabs\n\t"
			 "fstpt %[absolute]\n\t"
			 "fldt %[value]\n\t"
			 "frndint\n\t"
			 "fstpt %[rounded]"
			 : [root] "=m"(root), [negated] "=m"(negated),
			   [absolute] "=m"(absolute), [rounded] "=m"(rounded)
			 : [value] "m"(value)
			 : "st", "cc", "memory");
	__asm__ volatile("fldt %[other]\n\t"
			 "fldt %[value]\n\t"
			 "fprem\n\t"
			 "fstpt %[remainder]\n\t"
			 "fstp %%st(0)\n\t"
			 "fldt %[other]\n\t"
			 "fldt %[value]\n\t"
			 "fscale\n\t"
			 "fstpt %[scaled]\n\t"
			 "fstp %%st(0)"
			 : [remainder] "=m"(remainder), [scaled] "=m"(scaled)
			 : [value] "m"(value), [other] "m"(other)
			 : "st", "st(1)", "cc", "memory");
	return (long)(double)(root + negated + absolute + rounded + remainder + scaled);
}

long probe_constants(long a, long b)
{
	long double sum;
	__asm__ volatile("fld1\n\t"
			 "fldl2t\n\t"
			 "faddp %%st, %%st(1)\n\t"
			 "fldl2e\n\t"
			 "faddp %%st, %%st(1)\n\t"
			 "fldpi\n\t"
			 "faddp %%st, %%st(1)\n\t"
			 "fldlg2\n\t"
			 "faddp %%st, %%st(1)\n\t"
			 "fldln2\n\t"
			 "faddp %%st, %%st(1)\n\t"
			 "fldz\n\t"
			 "faddp %%st, %%st(1)\n\t"
			 "fstpt %[sum]"
			 : [sum] "=m"(sum)
			 :
			 : "st", "st(1)", "cc", "memory");
	return (long)(double)sum + a + b;
}

long probe_control_word(long a, long b)
{
	/* `fnstcw`/`fldcw`, and the rounding mode they carry: the same
	 * division, rounded four ways, has four answers, and an emulation
	 * that ignores the control word gets one of them right. */
	unsigned short saved, modified;
	long double left = as_extended(a);
	long double right = as_extended(b);
	long double nearest, down, up, chop;
	__asm__ volatile("fnstcw %[saved]" : [saved] "=m"(saved));
	modified = (unsigned short)((saved & ~0x0c00) | 0x0000);
	__asm__ volatile("fldcw %[modified]\n\t"
			 "fldt %[right]\n\t"
			 "fldt %[left]\n\t"
			 "fdivp %%st, %%st(1)\n\t"
			 "fstpt %[out]"
			 : [out] "=m"(nearest)
			 : [modified] "m"(modified), [left] "m"(left), [right] "m"(right)
			 : "st", "st(1)", "cc", "memory");
	modified = (unsigned short)((saved & ~0x0c00) | 0x0400);
	__asm__ volatile("fldcw %[modified]\n\t"
			 "fldt %[right]\n\t"
			 "fldt %[left]\n\t"
			 "fdivp %%st, %%st(1)\n\t"
			 "fstpt %[out]"
			 : [out] "=m"(down)
			 : [modified] "m"(modified), [left] "m"(left), [right] "m"(right)
			 : "st", "st(1)", "cc", "memory");
	modified = (unsigned short)((saved & ~0x0c00) | 0x0800);
	__asm__ volatile("fldcw %[modified]\n\t"
			 "fldt %[right]\n\t"
			 "fldt %[left]\n\t"
			 "fdivp %%st, %%st(1)\n\t"
			 "fstpt %[out]"
			 : [out] "=m"(up)
			 : [modified] "m"(modified), [left] "m"(left), [right] "m"(right)
			 : "st", "st(1)", "cc", "memory");
	modified = (unsigned short)((saved & ~0x0c00) | 0x0c00);
	__asm__ volatile("fldcw %[modified]\n\t"
			 "fldt %[right]\n\t"
			 "fldt %[left]\n\t"
			 "fdivp %%st, %%st(1)\n\t"
			 "fstpt %[out]\n\t"
			 "fldcw %[saved]"
			 : [out] "=m"(chop)
			 : [modified] "m"(modified), [saved] "m"(saved), [left] "m"(left),
			   [right] "m"(right)
			 : "st", "st(1)", "cc", "memory");
	return (long)(double)(nearest + down + up + chop);
}

long probe_environment(long a, long b)
{
	/* `fnstenv`/`fldenv` and `fnsave`/`frstor`: the layouts the crate
	 * owns, round-tripped. `fnstenv`'s documented side effect — it masks
	 * every exception in the live control word, which is the entire
	 * reason `feholdexcept` exists — is part of what is compared. */
	static unsigned char environment[28];
	static unsigned char saved[108];
	long double value = as_extended(a) + as_extended(b);
	long double restored;
	__asm__ volatile("fldt %[value]\n\t"
			 "fnstenv %[environment]\n\t"
			 "fldenv %[environment]\n\t"
			 "fnsave %[saved]\n\t"
			 "frstor %[saved]\n\t"
			 "fstpt %[restored]"
			 : [environment] "=m"(environment), [saved] "=m"(saved),
			   [restored] "=m"(restored)
			 : [value] "m"(value)
			 : "st", "cc", "memory");
	return (long)(double)restored;
}

long probe_compare_to_flags(long a, long b)
{
	/* `fcomi` and `fucomi`, which report into the integer flags rather
	 * than into the status word — the modern idiom, and the one that
	 * couples the two register files. */
	long double left = as_extended(a);
	long double right = as_extended(b);
	unsigned long below, equal, unordered;
	__asm__ volatile("fldt %[right]\n\t"
			 "fldt %[left]\n\t"
			 "fcomi %%st(1), %%st\n\t"
			 "setb %b[below]\n\t"
			 "sete %b[equal]\n\t"
			 "setp %b[unordered]\n\t"
			 "fstp %%st(0)\n\t"
			 "fstp %%st(0)"
			 : [below] "=&r"(below), [equal] "=&r"(equal),
			   [unordered] "=&r"(unordered)
			 : [left] "m"(left), [right] "m"(right)
			 : "st", "st(1)", "cc");
	return (long)((below & 1) + ((equal & 1) << 1) + ((unordered & 1) << 2));
}

long probe_memory_forms(long a, long b)
{
	/* The forms a compiler picks only sometimes: the non-popping store,
	 * and arithmetic against a memory operand rather than a register.
	 * Written out because coverage is a claim, and a claim that rests on
	 * "gcc will probably emit it" is not one. */
	union {
		long bits;
		double value;
	} left = { a }, right = { b };
	float narrow = (float)right.value;
	double stored, divided;
	long double kept;
	__asm__ volatile("fldl %[left]\n\t"
			 "fstl %[stored]\n\t"   /* fst m64: stores and keeps */
			 "fdivl %[right]\n\t"   /* fdiv m64 */
			 "fstl %[divided]\n\t"
			 "fdivs %[narrow]\n\t"  /* fdiv m32 */
			 "fsubrl %[left]\n\t"   /* fsubr m64 */
			 "fst %%st(1)\n\t"      /* fst st(i): the register form */
			 "fstpt %[kept]\n\t"
			 "fstp %%st(0)"
			 : [stored] "=m"(stored), [divided] "=m"(divided), [kept] "=m"(kept)
			 : [left] "m"(left.value), [right] "m"(right.value), [narrow] "m"(narrow)
			 : "st", "st(1)", "cc", "memory");
	return (long)(stored + divided + (double)kept);
}

long probe_save_and_restore_the_unit(long a, long b)
{
	/* `fxsave`/`fxrstor`: the whole unit and the vector file in one
	 * 512-byte area. What reaches this in the wild is
	 * `_dl_runtime_resolve` — lazy binding saves the vector registers
	 * around a symbol lookup, because the resolver is ordinary C and
	 * would otherwise clobber arguments passing through them.
	 *
	 * Two things are checked. The *round trip*: save, scribble over
	 * every register, restore, and the oracle compares all eighty bits
	 * of each stack register and all sixteen vector registers against
	 * hardware afterwards. And the *layout*: the bytes this reads back
	 * are the control word, the status word, the abridged tag byte and
	 * MXCSR, at the offsets the area puts them.
	 *
	 * Two ranges are deliberately not read, for two different reasons.
	 *
	 * Bytes 8..24 are FIP and FDP, the address of the last x87
	 * instruction and of its operand. This engine stores zeros, as the
	 * `x87` crate already does for `fnstenv`. Measured 2026-08-30, the
	 * machine under this suite stores zeros too when no exception is
	 * pending — so the divergence is narrower than it looks — but it is
	 * a divergence and it is named here rather than hidden by a probe
	 * that happens not to look.
	 *
	 * Bytes 28..32 are MXCSR_MASK, which is not a semantic at all: it is
	 * a statement of which processor this is, and this container reports
	 * one processor on every host exactly as `cpuid` does. Comparing it
	 * against the host would be comparing the two claims, not the two
	 * behaviours. */
	static unsigned char area[512] __attribute__((aligned(16)));
	static unsigned char after[512] __attribute__((aligned(16)));
	long double first = as_extended(a);
	long double second = as_extended(b);
	unsigned short control, status;
	unsigned short tag;
	unsigned int mxcsr;
	int same;

	__asm__ volatile("fldt %[first]\n\t"
			 "fldt %[second]\n\t"
			 "fmul %%st(1), %%st\n\t"
			 "fxsave %[area]\n\t"
			 /* Scribble: a different stack, different vectors. */
			 "fninit\n\t"
			 "fld1\n\t"
			 "pcmpeqd %%xmm0, %%xmm0\n\t"
			 "pcmpeqd %%xmm7, %%xmm7\n\t"
			 "movdqa %%xmm0, %%xmm3\n\t"
			 /* And back. */
			 "fxrstor %[area]\n\t"
			 "fxsave %[after]"
			 : [area] "=m"(area), [after] "=m"(after)
			 : [first] "m"(first), [second] "m"(second)
			 : "st", "st(1)", "xmm0", "xmm3", "xmm7", "cc", "memory");

	control = (unsigned short)(area[0] | (area[1] << 8));
	status = (unsigned short)(area[2] | (area[3] << 8));
	tag = area[4];
	mxcsr = (unsigned int)area[24] | ((unsigned int)area[25] << 8) |
		((unsigned int)area[26] << 16) | ((unsigned int)area[27] << 24);

	/* The saved area and the one taken after the restore have to agree
	 * everywhere the round trip preserves.
	 *
	 * The mask's bytes are skipped here as well, and the reason is worth
	 * stating: the lockstep oracle compares the register file after
	 * *every* instruction, so a byte this loop merely loads is a byte
	 * compared against the host — whatever the loop then does with it.
	 * Excluding a range from the arithmetic above is not enough; it has
	 * to be excluded from the load. FIP and FDP are not skipped, because
	 * both machines store zeros there and comparing them is information
	 * rather than noise. */
	same = 1;
	for (int index = 0; index < 512; index++) {
		if (index >= 28 && index < 32)
			continue;
		if (area[index] != after[index])
			same = 0;
	}
	return control + status + tag + mxcsr + same;
}
