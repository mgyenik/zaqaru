/* A whole process: the shape M6's boot path exists to run.
 *
 * Every other corpus guest is a *function* a test calls. This one is a
 * program — it starts at `_start` with the stack the kernel built, reads its
 * own arguments and environment off it, writes something derived from them,
 * and leaves through `exit_group`. Which is the point: argc/argv/envp/auxv
 * are only right if something actually reads them, and an exit status only
 * arrives if the throw reaches the boot catch.
 *
 * No libc, because there is no libc in a `-nostdlib` link and because the
 * whole question here is whether the kernel hands over what a libc would
 * need. What `_start` does by hand is what `__libc_start_main` would do.
 */

#define SYS_write 1
#define SYS_exit_group 231

/* The auxiliary vector's tags this program checks for itself. */
#define AT_NULL 0
#define AT_PHDR 3
#define AT_PHNUM 5
#define AT_PAGESZ 6
#define AT_ENTRY 9
#define AT_RANDOM 25
#define AT_SYSINFO_EHDR 33

static long sys3(long number, long a, long b, long c) {
	long result;
	__asm__ volatile("syscall"
	                 : "=a"(result)
	                 : "a"(number), "D"(a), "S"(b), "d"(c)
	                 : "rcx", "r11", "memory");
	return result;
}

static long length_of(const char *text) {
	long length = 0;
	while (text[length]) {
		length++;
	}
	return length;
}

static void put(const char *text) {
	sys3(SYS_write, 1, (long)text, length_of(text));
}

/* The program proper, given the stack exactly as `_start` received it. */
static void run(long *stack) {
	long argc = stack[0];
	char **argv = (char **)&stack[1];
	char **envp = argv + argc + 1;

	for (long i = 0; i < argc; i++) {
		put(argv[i]);
		put("\n");
	}
	for (char **entry = envp; *entry; entry++) {
		put(*entry);
		put("\n");
	}

	/* Past the environment's terminator lies the auxiliary vector, which
	 * is where a libc would find the program headers before it could set
	 * up TLS at all. */
	long *vector = (long *)envp;
	while (*vector) {
		vector++;
	}
	vector++;

	long headers = 0, count = 0, page = 0, entry_point = 0, random = 0;
	int vdso = 0;
	for (long *pair = vector; pair[0] != AT_NULL; pair += 2) {
		switch (pair[0]) {
		case AT_PHDR: headers = pair[1]; break;
		case AT_PHNUM: count = pair[1]; break;
		case AT_PAGESZ: page = pair[1]; break;
		case AT_ENTRY: entry_point = pair[1]; break;
		case AT_RANDOM: random = pair[1]; break;
		case AT_SYSINFO_EHDR: vdso = 1; break;
		}
	}

	/* The program headers really are readable at the address the kernel
	 * named: the first is a `PT_PHDR` or `PT_LOAD`, and either way its
	 * type word is small and nonzero for a linked executable. */
	put(headers && *(unsigned *)headers ? "phdr:yes\n" : "phdr:no\n");
	put(count ? "phnum:yes\n" : "phnum:no\n");
	put(page == 4096 ? "pagesz:4096\n" : "pagesz:other\n");
	put(entry_point ? "entry:yes\n" : "entry:no\n");
	put(random ? "random:yes\n" : "random:no\n");
	/* A vDSO would mean libc never issues a clock syscall, which is the
	 * only form the kernel can answer. */
	put(vdso ? "vdso:yes\n" : "vdso:no\n");

	sys3(SYS_exit_group, 7, 0, 0);
}

/* `%rsp` points at `argc`, and nothing may be pushed before it is read —
 * hence the assembly. The stack pointer becomes the argument and the entry
 * tail-jumps, so there is no instruction after it to reach: a program leaves
 * through `exit_group`, never by returning.
 *
 * The `.size` is not decoration. Function extents are how a translator knows
 * where one body ends, and hand-written assembly is exactly the case where a
 * compiler does not emit them for you. */
__asm__(".globl _start\n"
        ".type _start,@function\n"
        "_start:\n"
        "  mov %rsp, %rdi\n"
        "  jmp run_from_stack\n"
        ".size _start, .-_start\n");

void run_from_stack(long *stack);
void run_from_stack(long *stack) { run(stack); }
