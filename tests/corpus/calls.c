/* Milestone 4: calls, returns, and a guest stack in linear memory.

   `helper` is static so that the assembler may resolve the call without a
   relocation; `recursive_fibonacci` needs a real stack; `spill_heavy` keeps
   more values live than there are callee-saved registers, forcing pushes and
   spill slots. */

__attribute__((noinline)) static int helper(int value) { return value * 3 + 1; }

int apply_helper(int value) { return helper(value) + helper(value + 1); }

int recursive_fibonacci(int n) {
	if (n < 2) {
		return n;
	}
	return recursive_fibonacci(n - 1) + recursive_fibonacci(n - 2);
}

__attribute__((noinline)) int consume(int a, int b, int c, int d) {
	return a - b + c - d;
}

int spill_heavy(int seed) {
	int a = seed + 1;
	int b = seed + 2;
	int c = seed + 3;
	int d = seed + 4;
	int e = seed + 5;
	int f = seed + 6;
	int total = consume(a, b, c, d);
	total += consume(e, f, a, b);
	total += consume(c, d, e, f);
	return total + a + b + c + d + e + f;
}
