/* Milestone 1/5: references to a data symbol at a non-zero offset — the
   case where a mishandled program-counter-relative addend silently reads the
   wrong element (the "off-by-four factory").

   The table has external linkage on purpose: a `static` one would be
   constant-folded away, and the reference must survive into the object for
   there to be an addend to get wrong. */

int table[8] = {10, 20, 30, 40, 50, 60, 70, 80};

int third_element(void) { return table[2]; }

int sum_of_two(void) { return table[1] + table[5]; }

void bump_last(void) { table[7] += 1; }
