/* Milestone 3: branches and loops.

   Each function is chosen for the control-flow shape it produces rather than
   for what it computes: a loop with a two-way body, a counted loop, a value
   whose sign steers it, a comparison whose flags are read twice, and a
   clamp the compiler turns into conditional moves. */

/* A loop whose body is itself a branch. Defined for positive inputs. */
int gcd(int a, int b) {
	while (a != b) {
		if (a > b) {
			a -= b;
		} else {
			b -= a;
		}
	}
	return a;
}

/* A counted loop, wrapping on overflow exactly as the native code does. */
int fibonacci(int n) {
	int previous = 0;
	int current = 1;
	for (int step = 0; step < n; step++) {
		int next = previous + current;
		previous = current;
		current = next;
	}
	return previous;
}

int absolute(int value) { return value < 0 ? -value : value; }

/* Two conditions read from one comparison — the case where flag liveness
   crosses a basic-block boundary. */
int compare(int a, int b) { return (a > b) - (a < b); }

int clamp(int value, int low, int high) {
	if (value < low) {
		return low;
	}
	if (value > high) {
		return high;
	}
	return value;
}
