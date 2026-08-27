/* A reference specimen: compiled with `clang --target=wasm32 -c`, its
   linking metadata is the known-good example our emitter is diffed against.

   It is written to exercise every metadata shape the transpiler emits — a
   defined function, a call to an undefined one, initialised and
   zero-initialised data, a read-only string, and a stack frame (which is
   what makes clang reference the linker's `__stack_pointer`). Code quality
   is beside the point, so it is compiled without optimisation. */

extern int sink(int *values);

int counter = 7;
int uninitialised_slot;
const char message[] = "hi";

int add(int a, int b) { return a + b; }

int use_everything(int a) {
	int values[4] = {a, a + 1, a + 2, a + 3};
	counter += sink(values);
	return counter + uninitialised_slot + (int)message[1];
}
