/* setjmp and longjmp: the saved PC is a resume ID.
 *
 * Under `--resume` every call site stores a continuation in its
 * return-address slot, and `setjmp` saves the word at `(%rsp)` — so the
 * "program counter" a `jmp_buf` holds is a materialized, enterable
 * continuation, saved by code that has no idea that is what it is doing.
 * `longjmp` then jumps to it, the exec map misses, and the miss path tells a
 * tagged ID from an address and hands the kernel somewhere to go.
 *
 * Each case prints, so the comparison against the same program run natively
 * is on what happened and in what order rather than on a single answer.
 */

#include <setjmp.h>
#include <stdio.h>

static jmp_buf env;

/* 1. The same frame: setjmp and longjmp with nothing in between. */
static void same_frame(void)
{
	if (setjmp(env) == 0) {
		longjmp(env, 11);
	}
	printf("same_frame\n");
}

/* 2. Across frames, with callee-saved registers the jump has to restore.
 *
 * The values are live across the call that never returns, so a `longjmp`
 * that restored them wrongly prints different numbers rather than crashing —
 * which is the failure worth catching, since crashing is the easy one. */
static void descend(int n)
{
	if (n == 0) {
		longjmp(env, 22);
	}
	descend(n - 1);
}

static void across_frames(void)
{
	volatile long a = 0x1111, b = 0x2222, c = 0x3333;
	int value = setjmp(env);
	if (value == 0) {
		descend(6);
	}
	printf("across_frames:%d %lx %lx %lx\n", value, a, b, c);
}

/* 3. The canonical idiom, and the one case that tells this design from the
 * tempting simplification of reading the continuation back out of the stack.
 *
 * `setjmp`'s return-address slot is *reused* by the call this frame makes
 * next: `work()` writes its own call-site continuation to the identical
 * address. So at longjmp time that slot names "as if `work()` returned
 * normally", and entering it would print the after-work line instead of the
 * jumped-to one — silently, which is the failure class with no detector. The
 * `jmp_buf`'s saved word is the only surviving record of where `setjmp`
 * returns to. */
static void work(void)
{
	longjmp(env, 33);
}

static void reused_slot(void)
{
	if (setjmp(env) != 0) {
		printf("reused_slot:jumped\n");
		return;
	}
	work();
	printf("reused_slot:work returned\n");
}

/* 4. A tight loop. The wasm frames between the jump and its target are
 * discarded by a throw, not left standing — so this must run in constant
 * stack. A design that called the continuation instead would leak a frame
 * chain per iteration and exhaust the stack long before the end. */
static void many(void)
{
	volatile int count = 0;
	if (setjmp(env) == 0 || count < 20000) {
		count++;
		if (count < 20000) {
			longjmp(env, 1);
		}
	}
	printf("many:%d\n", count);
}

/* 5. A longjmp whose target frame was itself entered by a resume body.
 *
 * The first jump leaves `nested`'s frame running inside the resume body the
 * driver entered; the second `setjmp` therefore saves a continuation from
 * *that* frame, and the second jump has to reach it. Resume bodies' call
 * sites push IDs like anyone else's, so this should compose — but it is
 * where "the frames re-materialize lazily" does the most work, so it is a
 * case rather than an inference. */
static jmp_buf inner;

static void nested(void)
{
	if (setjmp(env) == 0) {
		longjmp(env, 44);
	}
	/* Running in a resumed frame from here on. */
	if (setjmp(inner) == 0) {
		longjmp(inner, 55);
	}
	printf("nested\n");
}

int main(void)
{
	same_frame();
	across_frames();
	reused_slot();
	many();
	nested();
	printf("done\n");
	return 0;
}
