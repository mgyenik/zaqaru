/* The integer core, one probe per shape of thing that can go wrong.
 *
 * Written against the lockstep oracle rather than against an output check,
 * so the point of each probe is the *sequence* of instructions gcc emits for
 * it, not the value it returns: every intermediate register and every flag
 * is compared against hardware after every instruction. A probe that
 * returns the right answer through a wrong flag fails here.
 *
 * Some probes are inline assembly. That is deliberate where the instruction
 * under test is one no compiler emits from C — the rotates, the bit-test
 * family, the string operations, the flag-word transfers — because "wait
 * for a real program to reach it" is exactly the strategy that leaves an
 * instruction untested until it is load-bearing.
 */
#include "lockstep.h"

long probe_arithmetic(long a, long b)
{
	long sum = a + b;
	long difference = a - b;
	long product = a * b;
	return sum ^ difference ^ product ^ (-a) ^ (a + 1) ^ (b - 1);
}

long probe_division(long a, long b)
{
	/* Both signed and unsigned, and both the quotient and the
	 * remainder, because they land in different registers and the byte
	 * width lands in a different one again. */
	long quotient = a / (b | 1);
	long remainder = a % (b | 1);
	unsigned long unsigned_quotient = (unsigned long)a / (unsigned long)(b | 1);
	return quotient ^ remainder ^ (long)unsigned_quotient;
}

long probe_narrow_widths(long a, long b)
{
	/* Every width, including the byte writes that must leave the rest of
	 * the register alone and the four-byte writes that must not. */
	unsigned char byte = (unsigned char)a + (unsigned char)b;
	unsigned short word = (unsigned short)a * (unsigned short)b;
	unsigned int dword = (unsigned int)a - (unsigned int)b;
	signed char narrow = (signed char)a;
	return byte + word + dword + narrow;
}

long probe_shifts(long a, long b)
{
	int count = (int)(b & 63);
	unsigned long logical = (unsigned long)a >> count;
	long arithmetic = a >> count;
	long left = a << count;
	unsigned int narrow = (unsigned int)a >> (count & 31);
	return logical ^ arithmetic ^ left ^ narrow;
}

long probe_conditions(long a, long b)
{
	/* Sixteen conditions, reached as `setcc` at any optimisation level
	 * and as `jcc` at none. */
	long answer = 0;
	answer += (a == b);
	answer += (a != b) << 1;
	answer += (a < b) << 2;
	answer += (a <= b) << 3;
	answer += (a > b) << 4;
	answer += (a >= b) << 5;
	answer += ((unsigned long)a < (unsigned long)b) << 6;
	answer += ((unsigned long)a <= (unsigned long)b) << 7;
	answer += ((unsigned long)a > (unsigned long)b) << 8;
	answer += ((unsigned long)a >= (unsigned long)b) << 9;
	return answer;
}

long probe_conditional_move(long a, long b)
{
	long high = a > b ? a : b;
	long low = a < b ? a : b;
	int narrow = (int)a > (int)b ? (int)a : (int)b;
	return high ^ low ^ narrow;
}

long probe_branchy(long a, long b)
{
	long answer = 0;
	for (long index = 0; index < (b & 15); index++) {
		if ((a >> index) & 1)
			answer += index;
		else
			answer -= index;
	}
	return answer;
}

long probe_switch(long a, long b)
{
	switch ((int)(a & 7)) {
	case 0: return b + 1;
	case 1: return b - 1;
	case 2: return b * 3;
	case 3: return b ^ 0x5555;
	case 4: return b << 2;
	case 5: return b >> 3;
	case 6: return ~b;
	default: return -b;
	}
}

long probe_memory(long a, long b)
{
	/* Loads and stores at every width, through an index the compiler
	 * cannot fold away. */
	static unsigned char bytes[64];
	static unsigned short words[64];
	static unsigned int dwords[64];
	static unsigned long qwords[64];
	int at = (int)(a & 31);
	bytes[at] = (unsigned char)b;
	words[at] = (unsigned short)b;
	dwords[at] = (unsigned int)b;
	qwords[at] = (unsigned long)b;
	return bytes[at] + words[at] + dwords[at] + qwords[at] + (long)&bytes[at];
}

long probe_carry_chain(long a, long b)
{
	/* `adc` and `sbb`, which a compiler reaches for well outside
	 * multi-word arithmetic. */
	unsigned long low, high;
	__asm__ volatile("addq %[b], %[low]\n\t"
			 "adcq $0, %[high]\n\t"
			 "subq %[b], %[low]\n\t"
			 "sbbq $0, %[high]"
			 : [low] "=&r"(low), [high] "=&r"(high)
			 : [b] "r"(b), "0"(a), "1"(a)
			 : "cc");
	return (long)(low ^ high);
}

long probe_rotates(long a, long b)
{
	unsigned long rol = (unsigned long)a, ror = (unsigned long)a;
	unsigned long rcl = (unsigned long)a, rcr = (unsigned long)a;
	unsigned int narrow = (unsigned int)a;
	unsigned char count = (unsigned char)(b & 63);
	__asm__ volatile("rolq %%cl, %[rol]\n\t"
			 "rorq %%cl, %[ror]\n\t"
			 "clc\n\t"
			 "rclq %%cl, %[rcl]\n\t"
			 "stc\n\t"
			 "rcrq %%cl, %[rcr]\n\t"
			 "roll %%cl, %[narrow]"
			 : [rol] "+r"(rol), [ror] "+r"(ror), [rcl] "+r"(rcl),
			   [rcr] "+r"(rcr), [narrow] "+r"(narrow)
			 : "c"(count)
			 : "cc");
	return (long)(rol ^ ror ^ rcl ^ rcr ^ narrow);
}

long probe_double_shift(long a, long b)
{
	unsigned long left = (unsigned long)a, right = (unsigned long)a;
	unsigned char count = (unsigned char)(b & 63);
	__asm__ volatile("shldq %%cl, %[b], %[left]\n\t"
			 "shrdq %%cl, %[b], %[right]"
			 : [left] "+r"(left), [right] "+r"(right)
			 : [b] "r"((unsigned long)b), "c"(count)
			 : "cc");
	return (long)(left ^ right);
}

long probe_bit_operations(long a, long b)
{
	unsigned long value = (unsigned long)a | 1;
	unsigned long offset = (unsigned long)b & 63;
	unsigned long scanned, reversed, tested = 0, set = value, cleared = value,
					     complemented = value;
	__asm__ volatile("bsfq %[value], %[scanned]\n\t"
			 "bsrq %[value], %[reversed]\n\t"
			 "btq %[offset], %[value]\n\t"
			 "setc %b[tested]\n\t"
			 "btsq %[offset], %[set]\n\t"
			 "btrq %[offset], %[cleared]\n\t"
			 "btcq %[offset], %[complemented]"
			 : [scanned] "=&r"(scanned), [reversed] "=&r"(reversed),
			   [tested] "+&r"(tested), [set] "+r"(set),
			   [cleared] "+r"(cleared), [complemented] "+r"(complemented)
			 : [value] "r"(value), [offset] "r"(offset)
			 : "cc");
	return (long)(scanned ^ reversed ^ tested ^ set ^ cleared ^ complemented);
}

long probe_bit_test_in_memory(long a, long b)
{
	/* The form whose offset is a *signed bit index* that may address
	 * outside the operand entirely — glibc's, and the one a naive
	 * modulus gets silently wrong. */
	static unsigned long array[8];
	unsigned long offset = (unsigned long)(b & 255);
	unsigned char tested = 0;
	array[0] = (unsigned long)a;
	array[1] = (unsigned long)~a;
	array[2] = (unsigned long)(a << 3);
	array[3] = (unsigned long)(a >> 5);
	__asm__ volatile("btq %[offset], %[array]\n\t"
			 "setc %[tested]\n\t"
			 "btsq %[offset], %[array]"
			 : [tested] "=r"(tested), [array] "+m"(array)
			 : [offset] "r"(offset)
			 : "cc");
	return (long)(tested + array[0] + array[1] + array[2] + array[3]);
}

long probe_exchange(long a, long b)
{
	static long slot;
	long swapped = a;
	long added = b;
	long compared;
	slot = a ^ b;
	__asm__ volatile("xchgq %[swapped], %[slot]\n\t"
			 "xaddq %[added], %[slot]"
			 : [swapped] "+r"(swapped), [added] "+r"(added), [slot] "+m"(slot)
			 :
			 : "cc");
	compared = a;
	__asm__ volatile("cmpxchgq %[b], %[slot]"
			 : "+a"(compared), [slot] "+m"(slot)
			 : [b] "r"(b)
			 : "cc");
	return swapped ^ added ^ compared ^ slot;
}

long probe_flag_words(long a, long b)
{
	unsigned long word;
	unsigned long carried;
	__asm__ volatile("cmpq %[b], %[a]\n\t"
			 "pushfq\n\t"
			 "popq %[word]\n\t"
			 "pushq %[word]\n\t"
			 "popfq\n\t"
			 "lahf\n\t"
			 "movq %%rax, %[carried]\n\t"
			 "sahf\n\t"
			 "cmc\n\t"
			 "clc\n\t"
			 "stc\n\t"
			 "adcq $0, %[carried]"
			 : [word] "=&r"(word), [carried] "=&r"(carried)
			 : [a] "r"(a), [b] "r"(b)
			 : "cc", "rax");
	return (long)(word ^ carried);
}

long probe_sign_extension(long a, long b)
{
	long widened;
	long high;
	__asm__ volatile("movq %[a], %%rax\n\t"
			 "cqo\n\t"
			 "movq %%rdx, %[high]\n\t"
			 "movl %k[a], %%eax\n\t"
			 "cltq\n\t"
			 "movq %%rax, %[widened]"
			 : [widened] "=&r"(widened), [high] "=&r"(high)
			 : [a] "r"(a)
			 : "rax", "rdx", "cc");
	return widened ^ high ^ b;
}

long probe_string_move(long a, long b)
{
	static unsigned char source[128];
	static unsigned char destination[128];
	unsigned long count = (unsigned long)(b & 63) + 1;
	unsigned char *from = source;
	unsigned char *to = destination;
	for (int index = 0; index < 128; index++)
		source[index] = (unsigned char)(a + index);
	__asm__ volatile("cld\n\t"
			 "rep movsb"
			 : "+D"(to), "+S"(from), "+c"(count)
			 :
			 : "memory", "cc");
	return destination[0] + destination[63] + destination[127];
}

long probe_string_compare(long a, long b)
{
	static unsigned char left[64];
	static unsigned char right[64];
	unsigned long count = 64;
	unsigned char answer;
	unsigned char *first = left;
	unsigned char *second = right;
	for (int index = 0; index < 64; index++) {
		left[index] = (unsigned char)(a + index);
		right[index] = (unsigned char)(a + index + ((index == (b & 63)) ? 1 : 0));
	}
	__asm__ volatile("cld\n\t"
			 "repe cmpsb\n\t"
			 "sete %[answer]"
			 : [answer] "=r"(answer), "+D"(first), "+S"(second), "+c"(count)
			 :
			 : "memory", "cc");
	return answer + count;
}

long probe_string_scan_and_store(long a, long b)
{
	static unsigned char buffer[64];
	unsigned long count = 64;
	unsigned char *cursor = buffer;
	unsigned long remaining;
	__asm__ volatile("cld\n\t"
			 "rep stosb"
			 : "+D"(cursor), "+c"(count)
			 : "a"((unsigned char)a)
			 : "memory", "cc");
	buffer[(b & 63)] = (unsigned char)(a + 1);
	cursor = buffer;
	remaining = 64;
	__asm__ volatile("cld\n\t"
			 "repne scasb"
			 : "+D"(cursor), "+c"(remaining)
			 : "a"((unsigned char)(a + 1))
			 : "memory", "cc");
	return (long)remaining + buffer[0];
}

long probe_backwards_string(long a, long b)
{
	/* The direction flag, which exists in the model only because a
	 * libc's `memmove` sets it to copy an overlapping range backwards
	 * and clears it immediately after. */
	static unsigned char buffer[128];
	unsigned long count = 32;
	unsigned char *from = buffer + 63;
	unsigned char *to = buffer + 95;
	for (int index = 0; index < 128; index++)
		buffer[index] = (unsigned char)(a + index);
	__asm__ volatile("std\n\t"
			 "rep movsb\n\t"
			 "cld"
			 : "+D"(to), "+S"(from), "+c"(count)
			 :
			 : "memory", "cc");
	return buffer[64] + buffer[95] + b;
}

long probe_stack_traffic(long a, long b)
{
	unsigned long popped;
	__asm__ volatile("pushq %[a]\n\t"
			 "pushq %[b]\n\t"
			 "popq %%rax\n\t"
			 "popq %[popped]\n\t"
			 "addq %%rax, %[popped]"
			 : [popped] "=&r"(popped)
			 : [a] "r"(a), [b] "r"(b)
			 : "rax", "cc");
	return (long)popped;
}

long probe_nested_calls(long a, long b)
{
	/* Calls and returns through the guest stack, deep enough that a
	 * mistake in either shows up as a wrong return address rather than
	 * as a wrong value. */
	if (a <= 0)
		return b;
	return probe_nested_calls(a - 1, b + a);
}

long probe_increment_decrement(long a, long b)
{
	/* Written in assembly because no compiler emits `inc` any more: it
	 * has a partial-flag dependency that costs more than the byte it
	 * saves, so gcc uses `add $1` and the instruction whose defining
	 * property is that it *preserves the carry* would otherwise never be
	 * reached by this suite at all. */
	unsigned long incremented = (unsigned long)a;
	unsigned long decremented = (unsigned long)a;
	unsigned int narrow = (unsigned int)a;
	unsigned char byte = (unsigned char)a;
	unsigned long carried;
	__asm__ volatile("cmpq %[b], %[a]\n\t"   /* leave a carry behind */
			 "incq %[incremented]\n\t"
			 "decq %[decremented]\n\t"
			 "incl %[narrow]\n\t"
			 "decb %[byte]\n\t"
			 "setc %b[carried]"
			 : [incremented] "+r"(incremented), [decremented] "+r"(decremented),
			   [narrow] "+r"(narrow), [byte] "+r"(byte), [carried] "=&r"(carried)
			 : [a] "r"(a), [b] "r"(b)
			 : "cc");
	return (long)(incremented ^ decremented ^ narrow ^ byte ^ carried);
}
