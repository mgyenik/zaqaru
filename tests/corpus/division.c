/* Division and the double-width dividend it consumes.

   Each of these makes the compiler emit a `cdq`/`cqo` (or a zeroed `edx`)
   followed by `idiv` or `div`, at a different width. The remainder cases
   matter as much as the quotients: the two land in different registers, and
   for a byte-wide divide the remainder goes to `ah`. */

int signed_quotient(int a, int b) { return a / b; }
int signed_remainder(int a, int b) { return a % b; }

unsigned unsigned_quotient(unsigned a, unsigned b) { return a / b; }
unsigned unsigned_remainder(unsigned a, unsigned b) { return a % b; }

long long quad_quotient(long long a, long long b) { return a / b; }
long long quad_remainder(long long a, long long b) { return a % b; }

unsigned long long quad_unsigned_quotient(unsigned long long a, unsigned long long b) {
	return a / b;
}

short word_quotient(short a, short b) { return (short)(a / b); }

unsigned char byte_quotient(unsigned char a, unsigned char b) {
	return (unsigned char)(a / b);
}

unsigned char byte_remainder(unsigned char a, unsigned char b) {
	return (unsigned char)(a % b);
}

/* Sign extension on its own, without a division following it. */
long long widen(int value) { return value; }
