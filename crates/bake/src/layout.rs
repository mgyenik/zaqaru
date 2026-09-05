//! Where the module's own data goes, so the program has room below it.
//!
//! A guest program's `PT_LOAD` segments have to end up at the addresses its
//! ELF states, and linear memory is where they go — a `-no-pie` x86-64
//! executable's first segment is at `0x400000`, four megabytes up. The
//! kernel's own data, the image blob and the shadow stack are all placed by
//! `wasm-ld`, from `1024` upward by default, straight across that address.
//! So the bake tells the linker to start the module's data above a region
//! reserved for the program, and the kernel loads the program into it.

/// How much of the low address space a container leaves to the program.
///
/// Sixty-four megabytes: a static glibc program tops out a few megabytes in
/// and a static CPython an order of magnitude further, and an untouched
/// reservation costs nothing but address space. A program that exceeds it
/// does not corrupt anything — its segment collides with the module's data
/// and the kernel refuses to load it, by name. The native interpreter
/// reserves the same region for the same reason.
pub const PROGRAM_REGION: u64 = 64 << 20;

/// The link argument that starts the module's data above the region.
pub fn link_arguments() -> Vec<String> {
    vec![format!("--global-base={PROGRAM_REGION}")]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_region_is_a_whole_number_of_wasm_pages() {
        assert_eq!(PROGRAM_REGION % 65536, 0);
    }

    #[test]
    fn the_data_starts_at_the_region_top() {
        assert_eq!(link_arguments(), vec!["--global-base=67108864".to_string()]);
    }
}
