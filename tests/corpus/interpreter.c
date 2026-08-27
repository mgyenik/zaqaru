/* A small stack machine: the shape of code where all of this meets.

   A dispatch `switch` inside a loop, a table of operator handlers called
   indirectly, a program held in read-only data, and a mutable stack in
   `.bss` — which between them exercise jump tables, function pointers, data
   relocations and loops in one function rather than one at a time. */

enum opcode {
	OP_PUSH,
	OP_ADD,
	OP_SUB,
	OP_MUL,
	OP_DUP,
	OP_SWAP,
	OP_NEG,
	OP_APPLY,
	OP_HALT
};

typedef int (*binary_operator)(int left, int right);

static int operator_add(int left, int right) { return left + right; }
static int operator_sub(int left, int right) { return left - right; }
static int operator_mul(int left, int right) { return left * right; }
static int operator_min(int left, int right) { return left < right ? left : right; }

static const binary_operator operators[4] = {
	operator_add,
	operator_sub,
	operator_mul,
	operator_min,
};

static int stack[64];

/* `program` is a sequence of opcodes, each optionally followed by an operand.
   Returns the value left on top of the stack, or -1 on underflow. */
int interpret(const unsigned char *program, int length, int seed) {
	int top = 0;
	int position = 0;

	stack[top++] = seed;

	while (position < length) {
		unsigned char instruction = program[position++];
		switch (instruction) {
		case OP_PUSH:
			if (position >= length) {
				return -1;
			}
			stack[top++] = (int)program[position++];
			break;
		case OP_ADD:
			if (top < 2) {
				return -1;
			}
			stack[top - 2] += stack[top - 1];
			top--;
			break;
		case OP_SUB:
			if (top < 2) {
				return -1;
			}
			stack[top - 2] -= stack[top - 1];
			top--;
			break;
		case OP_MUL:
			if (top < 2) {
				return -1;
			}
			stack[top - 2] *= stack[top - 1];
			top--;
			break;
		case OP_DUP:
			if (top < 1) {
				return -1;
			}
			stack[top] = stack[top - 1];
			top++;
			break;
		case OP_SWAP: {
			/* Swapping neighbouring slots is the natural spelling, and
			   the one compilers fuse into a single wide move — which is
			   exactly why it is here. */
			if (top < 2) {
				return -1;
			}
			int held = stack[top - 1];
			stack[top - 1] = stack[top - 2];
			stack[top - 2] = held;
			break;
		}
		case OP_NEG:
			if (top < 1) {
				return -1;
			}
			stack[top - 1] = -stack[top - 1];
			break;
		case OP_APPLY: {
			/* Selects a handler at run time and calls through it. */
			if (position >= length || top < 2) {
				return -1;
			}
			binary_operator apply = operators[program[position++] & 3];
			stack[top - 2] = apply(stack[top - 2], stack[top - 1]);
			top--;
			break;
		}
		case OP_HALT:
			position = length;
			break;
		default:
			return -1;
		}
		if (top < 0 || top >= 64) {
			return -1;
		}
	}
	return top > 0 ? stack[top - 1] : -1;
}

static const unsigned char sample_program[] = {
	OP_PUSH, 7, OP_ADD, OP_DUP, OP_MUL, OP_PUSH, 3, OP_APPLY, 1, OP_NEG, OP_HALT,
};

int run_sample(int seed) {
	return interpret(sample_program, (int)sizeof sample_program, seed);
}
