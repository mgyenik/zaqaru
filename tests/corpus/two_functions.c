/* Milestone 1: a direct call and a .rodata reference in one object. */
static const int weights[4] = {2, 3, 5, 7};

__attribute__((noinline)) int scale(int value, int index) {
	return value * weights[index & 3];
}

int scale_twice(int value) { return scale(scale(value, 1), 2); }
