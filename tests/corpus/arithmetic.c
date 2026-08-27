/* Milestone 3: the arithmetic and flag machinery, at every operand width.

   The corpus above exercises control-flow shapes; this one exercises the
   places a flag can be computed wrongly without anything crashing —
   sub-register writes, unsigned versus signed comparison, shifts by a
   variable count, and multiplication overflow. */

unsigned char byte_mix(unsigned char a, unsigned char b) {
	unsigned char result = a;
	result += b;
	result ^= 0x5a;
	result <<= 3;
	result >>= 1;
	result = (unsigned char)-result;
	result = (unsigned char)~result;
	result &= b;
	result |= a;
	return result;
}

short word_mix(short a, short b) {
	short result = (short)(a - b);
	result = (short)(result * 3);
	result = (short)(result << 5);
	result = (short)(result >> 2);
	result ^= a;
	return result;
}

long long quad_mix(long long a, long long b) {
	long long result = a * b;
	result += a;
	result -= b;
	result &= ~a;
	result |= b;
	result ^= a << 7;
	result >>= 3;
	return result;
}

/* Unsigned comparison drives the carry flag, signed comparison the sign and
   overflow flags; a mix-up between them shows up only for large values. */
int unsigned_order(unsigned a, unsigned b) {
	if (a < b) {
		return -1;
	}
	if (a > b) {
		return 1;
	}
	return 0;
}

int signed_order(int a, int b) {
	if (a < b) {
		return -1;
	}
	if (a > b) {
		return 1;
	}
	return 0;
}

/* Shift counts come from a register, so the count-of-zero case (where every
   flag keeps its previous value) is reachable. */
int variable_shifts(int value, int count) {
	int masked = count & 31;
	return (value << masked) + (value >> masked) + (int)((unsigned)value >> masked);
}

/* Reads the overflow flag straight after a multiply. */
int multiply_overflows(int a, int b) {
	int result;
	return __builtin_mul_overflow(a, b, &result) ? -1 : result;
}

long long quad_multiply_overflows(long long a, long long b) {
	long long result;
	return __builtin_mul_overflow(a, b, &result) ? -1 : result;
}

/* `test` rather than `cmp`: the zero flag from a bitwise and. */
int has_bits(unsigned value, unsigned mask) { return (value & mask) != 0; }
