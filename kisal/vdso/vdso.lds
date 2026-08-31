/* The vDSO's layout: one read-and-execute segment, and the symbol version
 * glibc insists on.
 *
 * Modelled on Linux's own `arch/x86/entry/vdso/vdso-layout.lds.S`, and the
 * two things it gets right are both load-bearing.
 *
 * **`PT_DYNAMIC` must not be writable.** glibc decides whether to relocate a
 * `.dynamic` section *in place* by asking whether its segment is writable
 * (`l_ld_readonly`). A default link puts `.dynamic` in a writable segment,
 * glibc's static path then does `d_un.d_ptr += l_addr` straight into the
 * mapped image, and the guest takes a write fault on a page the kernel
 * mapped read-only — which is exactly what happened, in
 * `_dl_non_dynamic_init`, on the second dynamic entry. The real vDSO's
 * `PT_DYNAMIC` is `R`, so glibc adds the base at each use instead and never
 * writes. Explicit `PHDRS` is the only way to say so.
 *
 * **`LINUX_2.6` is not decoration.** glibc's `dl_vdso_vsym` looks the symbols
 * up *by version*, and a vDSO whose symbols carry a different one — or none
 * — is one glibc silently ignores in favour of the syscall. A failure that
 * looks exactly like success.
 */
PHDRS
{
    /* `FILEHDR PHDRS` puts the ELF header and the program headers inside the
     * segment, which is what lets a program find them from the base address
     * `AT_SYSINFO_EHDR` gives it. */
    text PT_LOAD FILEHDR PHDRS FLAGS(5);      /* read + execute */
    dynamic PT_DYNAMIC FLAGS(4);              /* read only; see above */
}

SECTIONS
{
    . = SIZEOF_HEADERS;

    /* The image's own base, defined by the linker and referenced
     * PC-relatively — so finding the timebase page below it needs no
     * relocation, and therefore no loader. A vDSO is mapped, not loaded:
     * nothing applies relocations to it, and a symbol that needed one would
     * read zero. */
    __vdso_image_start = 0;

    .hash           : { *(.hash) }              :text
    .gnu.hash       : { *(.gnu.hash) }
    .dynsym         : { *(.dynsym) }
    .dynstr         : { *(.dynstr) }
    .gnu.version    : { *(.gnu.version) }
    .gnu.version_d  : { *(.gnu.version_d) }
    .dynamic        : { *(.dynamic) }           :text :dynamic
    .rodata         : { *(.rodata*) }           :text
    .text           : { *(.text*) }             :text

    /DISCARD/       : { *(.data*) *(.bss*) *(.comment) *(.note*) }
}

VERSION
{
    LINUX_2.6 {
    global:
        __vdso_clock_gettime;
        __vdso_gettimeofday;
        __vdso_time;
        __vdso_clock_getres;
        __vdso_getcpu;
        clock_gettime;
        gettimeofday;
        time;
        clock_getres;
        getcpu;
    local: *;
    };
}
