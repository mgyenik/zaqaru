/* Milestone 4, the payoff demo (first half): this object calls a function
   the other object defines, and vice versa. Each is transpiled on its own
   and the two are linked by stock `wasm-ld`. */

int pong(int steps);

/* Defined here, referenced from the other object: an undefined data symbol
   on that side, resolved by the linker. */
int shared_total = 0;

int ping(int steps) {
	shared_total += 1;
	if (steps <= 0) {
		return 0;
	}
	return 1 + pong(steps - 1);
}

int read_shared_total(void) { return shared_total; }
