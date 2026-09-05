/* The contract between a lockstep corpus program and the harness.
 *
 * A corpus program is a static, position-dependent executable that is never
 * actually run: the harness `exec`s it under `ptrace` only to have the
 * kernel map its segments, then sets the child's registers by hand and
 * single-steps from a probe's first instruction. So nothing here may depend
 * on libc having been initialised — no TLS, no `errno`, no allocation, and
 * no syscalls.
 *
 * Three symbols are the interface:
 *
 * - `lockstep_stack`, which the harness points `%rsp` into. It is a static
 *   array so that it lives in `.bss` at a low address, which keeps every
 *   address the compared region touches inside the four-gigabyte space the
 *   engine models. The real process stack is where the kernel put it, and
 *   the probe never sees it.
 * - `lockstep_stop`, whose address is the return address the harness pushes.
 *   Reaching it ends the comparison; executing it would be a bug, which is
 *   why it is `hlt` rather than something survivable.
 * - `probe_*`, one per case. The harness finds them in the symbol table and
 *   runs each one over the argument sweep.
 */
#ifndef LOCKSTEP_H
#define LOCKSTEP_H

char lockstep_stack[1 << 16] __attribute__((aligned(64)));

void lockstep_stop(void)
{
	__asm__ volatile("hlt");
}

#endif
