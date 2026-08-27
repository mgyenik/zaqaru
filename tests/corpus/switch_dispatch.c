/* `switch` statements dense enough that the compiler builds a jump table.

   These are the case the transpiler cannot lift the way it lifts everything
   else: a table entry is a *code* address, which has no wasm equivalent at
   all, so the dispatch has to be recognised and turned into a `br_table`.
   The shapes below are chosen to stress the recognition — a dispatch inside a
   loop, two in one function, one whose cases fall through, and one whose
   index is narrower than a register. */

int classify(int selector, int value) {
	switch (selector) {
	case 0: return value * 3;
	case 1: return value - 7;
	case 2: return value ^ 0x5a;
	case 3: return value + 11;
	case 4: return value << 2;
	case 5: return value >> 1;
	case 6: return -value;
	case 7: return ~value;
	case 8: return value / 3;
	case 9: return value % 7;
	default: return -1;
	}
}

/* Cases that fall into each other, so several table entries land on the same
   block and the blocks are not laid out in index order. */
int accumulate(int selector, int value) {
	int total = 0;
	switch (selector) {
	case 0:
		total += 1;
		/* fall through */
	case 1:
		total += 2;
		/* fall through */
	case 2:
		total += 4;
		break;
	case 3:
		total += 8;
		break;
	case 4:
	case 5:
		total += 16;
		break;
	case 6:
		total += 32;
		break;
	case 7:
		total = value;
		break;
	default:
		total = -1;
		break;
	}
	return total;
}

/* A dispatch inside a loop: the switch's blocks are inside the loop body, so
   the structured translation has to nest a `br_table` within a `loop`. */
int fold(int seed, int steps) {
	int total = 0;
	for (int step = 0; step < (steps & 15); step++) {
		switch ((seed + step) & 7) {
		case 0: total += 1; break;
		case 1: total -= 2; break;
		case 2: total *= 3; break;
		case 3: total ^= 0x33; break;
		case 4: total += step; break;
		case 5: total -= step; break;
		case 6: total = ~total; break;
		default: total <<= 1; break;
		}
	}
	return total;
}

/* Two dispatches in one function, so the entry scan must not run one table
   into the next. */
int twice(int first, int second) {
	int left;
	switch (first & 7) {
	case 0: left = 10; break;
	case 1: left = 20; break;
	case 2: left = 30; break;
	case 3: left = 40; break;
	case 4: left = 50; break;
	case 5: left = 60; break;
	case 6: left = 70; break;
	default: left = 80; break;
	}
	switch (second & 7) {
	case 0: return left + 1;
	case 1: return left + 2;
	case 2: return left + 3;
	case 3: return left + 4;
	case 4: return left + 5;
	case 5: return left + 6;
	case 6: return left + 7;
	default: return left + 8;
	}
}

/* A narrower index, so the zero-extension in front of the dispatch matters. */
int from_byte(unsigned char selector, int value) {
	switch (selector) {
	case 0: return value;
	case 1: return value + 1;
	case 2: return value + 2;
	case 3: return value + 3;
	case 4: return value + 4;
	case 5: return value + 5;
	case 6: return value + 6;
	case 7: return value + 7;
	case 8: return value + 8;
	default: return 0;
	}
}

/* One switch nested inside another. */
int nested(int outer, int inner) {
	switch (outer & 3) {
	case 0:
		switch (inner & 7) {
		case 0: return 1;
		case 1: return 2;
		case 2: return 3;
		case 3: return 4;
		case 4: return 5;
		case 5: return 6;
		case 6: return 7;
		default: return 8;
		}
	case 1: return 100;
	case 2: return 200;
	default: return 300;
	}
}
