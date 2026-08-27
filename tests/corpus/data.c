/* Milestone 5: data sections, and relocations that live inside them.

   Between them these touch `.rodata` (the lookup table and the string),
   `.data` (the counter), `.bss` (the scratch array), and a pointer stored in
   data, which is the only case where a relocation has to be translated into
   the data segment itself rather than into code. */

static const int lookup[8] = {2, 3, 5, 7, 11, 13, 17, 19};

int table_lookup(int index) { return lookup[index & 7]; }

/* A fixed element: the reference carries a non-zero addend, which is where a
   mishandled program-counter offset reads the wrong slot. */
int fifth_element(void) { return lookup[4]; }

int counter = 100;

int bump_counter(int delta) {
	counter += delta;
	return counter;
}

static const char greeting[] = "hello, world";

int greeting_length(void) {
	const char *cursor = greeting;
	while (*cursor != '\0') {
		cursor++;
	}
	return (int)(cursor - greeting);
}

static int scratch[16];

int store_then_load(int index, int value) {
	scratch[index & 15] = value;
	return scratch[(index + 1) & 15];
}

/* A pointer held in data, so its relocation is applied to the segment
   itself. It is mutable and externally visible on purpose: a `static const`
   pointer would just be folded into the code that reads it. */
const int *table_pointer = lookup;

int through_pointer(int index) { return table_pointer[index & 7]; }
