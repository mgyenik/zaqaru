/* `/proc/self/maps`, read from inside a container.
 *
 * The file is a rendering of the kernel's VMA tree, produced when it is read
 * rather than stored — glibc's `pthread_getattr_np` reads it to find a
 * thread's stack bounds, and a snapshot taken at boot would describe an
 * address space that no longer exists. This is what says the rendering
 * works where it has to: inside the module, where an address is a real
 * offset into linear memory and `usize` is thirty-two bits.
 *
 * The report is the mapped address, then the file's bytes. Parsing hex is
 * the host's job; the guest's job is to prove the file is readable through
 * the ordinary rows.
 */

typedef long ssize;

#define SYS_read 0
#define SYS_write 1
#define SYS_open 2
#define SYS_close 3
#define SYS_mmap 9
#define SYS_munmap 11

#define O_RDONLY 0
#define PROT_READ 1
#define PROT_WRITE 2
#define MAP_PRIVATE 0x02
#define MAP_ANONYMOUS 0x20
#define PAGE 4096

static ssize sys3(long number, long a, long b, long c) {
	ssize result;
	__asm__ volatile("syscall"
	                 : "=a"(result)
	                 : "a"(number), "D"(a), "S"(b), "d"(c)
	                 : "rcx", "r11", "memory");
	return result;
}

static ssize sys6(long number, long a, long b, long c, long d, long e, long f) {
	ssize result;
	register long r10 __asm__("r10") = d;
	register long r8 __asm__("r8") = e;
	register long r9 __asm__("r9") = f;
	__asm__ volatile("syscall"
	                 : "=a"(result)
	                 : "a"(number), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8), "r"(r9)
	                 : "rcx", "r11", "memory");
	return result;
}

long guest_maps(long out) {
	/* Something to look for in the file. */
	long mapped = sys6(SYS_mmap, 0, 3 * PAGE, PROT_READ | PROT_WRITE,
	                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);

	long header[4];
	header[0] = mapped;
	header[1] = 3 * PAGE;
	header[2] = 0;
	header[3] = 0;

	long fd = sys3(SYS_open, (long)"/proc/self/maps", O_RDONLY, 0);
	if (fd < 0) {
		header[2] = fd;
		sys3(SYS_write, out, (long)header, sizeof(header));
		return 0;
	}

	static volatile char text[16384];
	long total = 0;
	for (;;) {
		long read = sys3(SYS_read, fd, (long)&text[total], sizeof(text) - total);
		if (read <= 0) {
			break;
		}
		total += read;
	}
	sys3(SYS_close, fd, 0, 0);

	header[2] = total;
	sys3(SYS_write, out, (long)header, sizeof(header));
	sys3(SYS_write, out, (long)text, total);
	return 0;
}
