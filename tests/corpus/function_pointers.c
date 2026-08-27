/* Indirect calls: the C-style interface, and every other shape a function
   pointer takes.

   A function pointer's *value* is a table slot rather than an address once
   translated, so nothing here returns one to be compared — the tests call
   through them, compare them with each other, and check them against null,
   which is what real interface code does. */

struct text_ops {
	int (*transform)(int value);
	int (*combine)(int left, int right);
};

static int doubled(int value) { return value * 2; }
static int negated(int value) { return -value; }
static int sum(int left, int right) { return left + right; }
static int difference(int left, int right) { return left - right; }

/* Two vtables in read-only data. Each pointer is a relocation applied to the
   data segment itself, which is the only case where a function address has to
   be written into memory rather than computed. */
static const struct text_ops doubling = {doubled, sum};
static const struct text_ops negating = {negated, difference};

static const struct text_ops *ops_for(int which) {
	return which ? &negating : &doubling;
}

int apply_transform(int which, int value) {
	return ops_for(which)->transform(value);
}

int apply_combine(int which, int left, int right) {
	return ops_for(which)->combine(left, right);
}

/* An array of handlers, indexed at run time — the dispatch compiles to a
   load and an indirect call through a scaled address. */
typedef int (*handler)(int value);

static const handler handlers[4] = {doubled, negated, doubled, negated};

int dispatch(int index, int value) { return handlers[index & 3](value); }

/* A function pointer passed as an argument, and a call through a register. */
__attribute__((noinline)) static int apply(handler action, int value) {
	return action(value);
}

int apply_doubled(int value) { return apply(doubled, value); }
int apply_negated(int value) { return apply(negated, value); }

/* Null is still null: the linker leaves table slot zero unassigned. */
__attribute__((noinline)) static int apply_or_zero(handler action, int value) {
	return action ? action(value) : 0;
}

int guarded_present(int value) { return apply_or_zero(doubled, value); }
int guarded_absent(int value) { return apply_or_zero(0, value); }

/* Comparing function pointers, which must agree even though the values
   themselves differ from the native ones. */
int same_handler(int left, int right) {
	return handlers[left & 3] == handlers[right & 3];
}

int is_doubled(int index) { return handlers[index & 3] == doubled; }

/* A pointer stored in mutable data, chosen at run time and called later. */
static handler installed = doubled;

void install(int which) { installed = which ? negated : doubled; }
int run_installed(int value) { return installed(value); }

/* A tail call through a function pointer. */
int tail_apply(int index, int value) { return handlers[index & 3](value); }
