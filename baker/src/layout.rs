//! Where the module's own data goes, when the container carries a program
//! that has to be loaded.
//!
//! A relocatable guest is never loaded: every operand resolves to
//! symbol+addend, `wasm-ld` places the data, and there are no virtual
//! addresses to honour. A *linked* guest is the opposite — its operands are
//! addresses, so its `PT_LOAD` segments have to end up at exactly the
//! addresses the ELF states, and linear memory is where they go.
//!
//! Which collides with the module itself, in three ways, none of which the
//! runtime can fix on its own:
//!
//! 1. **The image blob covers the program's address.** A `-no-pie` x86-64
//!    executable's first `PT_LOAD` is at `0x400000`, four megabytes up.
//!    `wasm-ld` places module data from `1024` upward, and `__image_blob`
//!    holds every file in the container image — a hundred megabytes and
//!    more for a real one. It reaches four megabytes long before it ends,
//!    so the bytes the program must occupy are already the image the
//!    program is being read out of.
//!
//! 2. **The arenas grow into the program.** `brk` and `mmap` are carved
//!    from the top of what the module occupies at boot. With the program
//!    below that, `brk` walks straight through the program's own text and
//!    hands the guest addresses inside itself.
//!
//! 3. **Reading the program allocates over where it goes.** Loading means
//!    reading the executable's bytes out of the image, which for a real
//!    interpreter is tens of megabytes. The allocator takes those pages
//!    from the end of memory, moving its arena past `0x400000` — and then
//!    placing the program writes over the live allocator that made the read
//!    possible. This one only bites at a binary size big enough to matter,
//!    which is the worst way to find out.
//!
//! Carving arenas *around* the program cannot fix any of this, because the
//! program's base is below the module's data rather than above it: the
//! region is taken before kisal runs an instruction.
//!
//! So the bake decides it instead. The module's data starts above whatever
//! the program needs, leaving the low region for the program alone — and
//! then everything downstream of the data (`__heap_base`, the allocator, and
//! the arenas above them) is above the program by construction rather than
//! by luck. That is what makes the loaded addresses faithful, which is what
//! makes `AT_PHDR`, `/proc/self/maps`, and any program that reads its own
//! ELF headers work with no special case anywhere.

/// Wasm's own page, which every memory boundary is a multiple of.
const WASM_PAGE: u64 = 65536;

/// The linker's default, which is what a container with nothing to load
/// keeps. Reserving a region for a program that does not exist would cost
/// every relocatable-tier container a hole it has no use for.
pub const DEFAULT_DATA_BASE: u64 = 1024;

/// Where the bake starts placing position-independent files.
///
/// High on purpose, and the reason is discovery rather than layout. A
/// shared object's text begins a few kilobytes above its base, so at a low
/// base it sits exactly where the program's own integer constants sit —
/// every `mov $0x1770,%eax` reads as an instruction taking the address of
/// code, and the operand harvest cannot tell the two apart. Measured on
/// `ld-linux-x86-64.so.2`: read at zero, eleven address-taken functions
/// against three at a base up here, and the eight extra ones shredded a
/// region no strong witness covered into pieces beginning partway through
/// real instructions.
///
/// A quarter of a gigabyte of address space is what it costs. Address
/// space, not memory: nothing writes to the gap, and a wasm engine reserves
/// rather than commits. Higher would be better still for the same reason,
/// and the ceiling is the 32-bit address space everything has to share.
pub const DYNAMIC_BASE: u64 = 0x1000_0000;

/// Modules are packed upward from [`DYNAMIC_BASE`] at this alignment.
///
/// A whole wasm page, which is more than the 4 KiB any loader asks for and
/// keeps every module boundary a boundary memory can be grown to.
pub const MODULE_ALIGNMENT: u64 = WASM_PAGE;

/// The lowest address a *fixed* executable may be linked at.
///
/// The same argument as [`DYNAMIC_BASE`], from the other side. When a file
/// states its own addresses there is no base to choose, so the only
/// available answer is to refuse — and the floor for refusing someone
/// else's choice has to sit well below the base we pick for our own.
/// `0x400000` is what GNU ld and lld both emit for `-no-pie`, and it is
/// where the entire tested corpus lives; anything under a megabyte would
/// have to have been linked that way on purpose.
pub const MINIMUM_FIXED_ADDRESS: u64 = 0x10_0000;

/// Where the module's data must start for a program occupying up to `top`.
///
/// No headroom: the region holds the loaded program and nothing else, and
/// `top` is already the highest address any of its segments reaches. A bake
/// placing several programs passes the highest top of all of them.
pub fn module_data_base(top: u64) -> u64 {
    // A whole wasm page, so the boundary between the guest's region and the
    // module's data is also a boundary memory can actually be grown to.
    let base = top.div_ceil(WASM_PAGE) * WASM_PAGE;
    base.max(DEFAULT_DATA_BASE.div_ceil(WASM_PAGE) * WASM_PAGE)
}

/// The link arguments that put the data there, for a container carrying a
/// linked program. Empty when there is nothing to load, which leaves the
/// linker at its default.
pub fn link_arguments(program_top: Option<u64>) -> Vec<String> {
    match program_top {
        Some(top) => vec![format!("--global-base={}", module_data_base(top))],
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_data_starts_above_the_program() {
        // The shape that matters: a program at the usual base with a
        // realistic span, and data that begins past all of it.
        let top = 0x400000 + 48 * 1024 * 1024;
        let base = module_data_base(top);
        assert!(base >= top, "the module's data overlaps the program");
        assert_eq!(base % WASM_PAGE, 0, "the boundary is not a memory boundary");
        assert!(
            base - top < WASM_PAGE,
            "the region is larger than the program needs"
        );
    }

    #[test]
    fn nothing_to_load_means_nothing_to_reserve() {
        assert!(link_arguments(None).is_empty());
    }

    #[test]
    fn a_tiny_program_still_leaves_the_linker_room() {
        // Rounding a small top down to the linker's own default would put
        // the module's data at address zero, which is not a place data can
        // go.
        assert!(module_data_base(16) >= DEFAULT_DATA_BASE);
    }
}
