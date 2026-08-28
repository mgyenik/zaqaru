/* An indirect function, and therefore a procedure linkage table.
 *
 * `ifunc` is how a libc ships several implementations of `memcpy` and picks
 * one at startup from what the processor turns out to support. The mechanism
 * survives static linking: the linker emits an `R_X86_64_IRELATIVE`
 * relocation and a sixteen-byte stub that jumps through a slot in the global
 * offset table, and `__libc_start_main` walks the relocations before `main`,
 * calls each resolver, and writes the answer into the slot.
 *
 * So a static binary has a linkage table whose entries are functions that no
 * symbol names and no unwind entry describes — and without them, calls to
 * `memcpy` and `strlen` in a static glibc resolve to nothing at all. 77 of
 * the functions a static `hello` can reach were refused for exactly this.
 */

static int chosen(int value) { return value * 3 + 1; }

static void *resolve(void) { return (void *)chosen; }

int dispatched(int value) __attribute__((ifunc("resolve")));

/* Calling it goes through the stub, which is the whole point. */
int through_the_table(int value) { return dispatched(value); }
