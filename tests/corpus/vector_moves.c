/* Struct copies and neighbour swaps: the shapes a compiler spells with SSE
   register moves even though there is no floating point anywhere in them.

   This is the payoff the plan names for XMM state — a sixteen-byte assignment
   becomes `movdqa`/`movaps`, an eight-byte one `movq`, and an adjacent-slot
   swap a wide load plus a lane shuffle. There are no loops here on purpose:
   the subject is the *movers*, and a loop would bring auto-vectorised packed
   arithmetic along with it, which is a different milestone's corpus.

   Every function builds its own starting state from its argument, so each is
   self-contained and the differential comparison needs no lockstep between
   calls. */

struct quad {
	int a, b, c, d;
};

struct pair {
	int a, b;
};

static struct quad quads[4];
static struct pair pairs[4];
static int words[16];

static void fill_quad(struct quad *target, int value)
{
	target->a = value;
	target->b = value * 3 + 1;
	target->c = value ^ 0x5a5a;
	target->d = ~value;
}

static int quad_digest(const struct quad *source)
{
	return source->a + source->b * 7 + source->c * 13 + source->d * 17;
}

/* A sixteen-byte assignment out of a local and then between two array slots:
   one whole XMM register through a load and a store each time. */
int copy_quad(int value, int index)
{
	struct quad built;
	fill_quad(&built, value);
	quads[index & 3] = built;
	quads[(index + 1) & 3] = quads[index & 3];
	return quad_digest(&quads[(index + 1) & 3]);
}

/* Three-way rotation, so several copies are in flight at once and the
   compiler has to use more than one register. */
int rotate_quads(int value, int index)
{
	struct quad built;
	fill_quad(&built, value);
	quads[0] = built;
	fill_quad(&built, value + 1);
	quads[1] = built;
	fill_quad(&built, value + 2);
	quads[2] = built;

	struct quad held = quads[index & 1];
	quads[index & 1] = quads[(index + 1) & 3];
	quads[(index + 1) & 3] = quads[(index + 2) & 3];
	quads[(index + 2) & 3] = held;

	return quad_digest(&quads[0]) ^ (quad_digest(&quads[1]) * 3)
	       ^ (quad_digest(&quads[2]) * 5);
}

/* An eight-byte assignment: the low half of a register on its own, which is
   where `movq` appears rather than a whole-register move. */
int copy_pair(int value, int index)
{
	pairs[index & 3].a = value;
	pairs[index & 3].b = value * 5 - 3;
	pairs[(index + 1) & 3] = pairs[index & 3];
	return pairs[(index + 1) & 3].a + pairs[(index + 1) & 3].b * 11;
}

/* Swapping two neighbouring words — the case the plan restored to its natural
   form, which compilers fuse into one wide move plus a lane swap rather than
   two narrow ones. */
int swap_neighbours(int value, int index)
{
	int slot = index & 6;
	words[slot] = value;
	words[slot + 1] = value * 9 + 4;

	int held = words[slot];
	words[slot] = words[slot + 1];
	words[slot + 1] = held;

	return words[slot] + words[slot + 1] * 3;
}

/* The same swap on struct fields rather than array elements, which reaches it
   through a different address computation. */
int swap_pair_fields(int value, int index)
{
	struct pair *slot = &pairs[index & 3];
	slot->a = value;
	slot->b = ~value;

	int held = slot->a;
	slot->a = slot->b;
	slot->b = held;

	return slot->a * 3 + slot->b;
}

/* A fixed-size copy to an address the compiler cannot prove aligned, which is
   where the unaligned movers appear instead of the aligned ones. The size
   stays constant: a variable one is a `rep movsb` call, which is a string
   instruction rather than a vector one. */
int copy_unaligned(int value, int index)
{
	int slot = index & 3;
	words[0] = value;
	words[1] = value * 3 + 1;
	words[2] = value ^ 0x5a5a;
	words[3] = ~value;
	__builtin_memcpy(&words[slot + 4], &words[0], sizeof(struct quad));
	return words[slot + 4] + words[slot + 5] * 7 + words[slot + 6] * 13
	       + words[slot + 7] * 17;
}

/* Reads one field back out of a copied structure, so a copy that moved the
   wrong lanes shows up as a wrong value on its own rather than only inside a
   digest. */
int copied_field(int value, int index)
{
	struct quad built;
	fill_quad(&built, value);
	quads[3] = built;
	quads[0] = quads[3];
	switch (index & 3) {
	case 0:
		return quads[0].a;
	case 1:
		return quads[0].b;
	case 2:
		return quads[0].c;
	default:
		return quads[0].d;
	}
}
