//! The block cache: decode once, execute many times, and never execute a
//! byte the guest has since overwritten.
//!
//! A block is a decoded run from a guest program counter to the first
//! control transfer, capped so that pathological straight-line code cannot
//! make one unbounded entry. The cache is what makes interpretation fast
//! enough to be a floor at all — the spike measured decode-every-time at
//! 61 MIPS in wasm against 125 for the cached variant — and the map probe it
//! costs is paid once per *block*, never per instruction.
//!
//! Two properties are worth stating because getting either wrong is silent:
//!
//! **A block is a cache, not a claim about the program.** It is entered only
//! through its entry address. A jump into the middle of a cached block
//! simply creates a second, overlapping block, and that is correct by
//! construction: two entries into the same bytes are two cache entries. The
//! transpiler's whole splitting-and-sharing problem — which arm owns which
//! byte, what a mid-instruction target means — does not arise, and it must
//! not be reintroduced here in the name of saving memory nobody measured.
//!
//! **A block is registered against every page its bytes touch, not its
//! first.** A block spanning a page boundary that is registered once is a
//! stale-execution bug that shows up only when something writes the second
//! page — which is to say, only under a JIT, only sometimes, and never with
//! a diagnostic.

use std::collections::HashMap;

use iced_x86::{Decoder, DecoderError, DecoderOptions, FlowControl, Instruction, Mnemonic};

use crate::space::{Access, Fault, PAGE_SHIFT, Space};

/// How many instructions a block may hold.
///
/// The cap exists for straight-line code with no control transfer in it —
/// a large unrolled loop body, a `memcpy` written as a thousand moves — which
/// would otherwise decode into one enormous entry the first time it is
/// touched. Sixty-four is enough that ordinary basic blocks are never split
/// and small enough that the worst case is bounded.
pub const MAX_INSTRUCTIONS: usize = 64;

/// The longest an x86-64 instruction can be.
const MAX_INSTRUCTION_BYTES: u64 = 15;

/// How many blocks the cache holds before it is emptied and refilled.
///
/// A flush rather than an eviction policy: the cache is pure, so throwing all
/// of it away costs only the decode work to rebuild what is still hot, and a
/// least-recently-used structure would cost bookkeeping on the hit path,
/// which is the one path that must stay cheap. Revisit when a measurement
/// says a real workload thrashes it.
pub const CAPACITY: usize = 64 * 1024;

/// Slots in the direct-mapped lookup. A power of two, so the index is a
/// mask, and large enough that a working set of a few thousand blocks —
/// which is what a Python import graph turns out to be — mostly fits.
const RECENT: usize = 4096;

/// A decoded run of instructions, entered at one address.
pub struct Block {
    /// The guest address the block is entered at, and the key it is found by.
    pub entry: u64,
    /// One past the last byte the block decoded. A block that ends without a
    /// control transfer falls through to here.
    pub end: u64,
    pub instructions: Vec<Instruction>,
    /// Whether every instruction but the last falls straight through.
    ///
    /// Kept for the diagnostics that ask; the run loop uses the
    /// per-instruction [`crate::quick::Quick::checks_rip`] instead, because
    /// an extended block has conditional branches in its middle and one
    /// flag for the whole block cannot say which instructions they are.
    ///
    /// Blocks end at the first control transfer, so by construction only
    /// the last instruction can branch — and the only other way `rip` moves
    /// unexpectedly is a `rep`-prefixed string operation, which stays put
    /// while it has iterations left. When neither is present in the prefix,
    /// the run loop already knows where each of those instructions goes and
    /// does not have to ask `rip` afterwards to find out.
    pub simple: bool,
    /// The same instructions, pre-decoded — see [`crate::quick`]. Parallel
    /// to `instructions` rather than folded into it, because the general
    /// path still wants the original and a lowered op that had to carry one
    /// would save nothing.
    pub quick: Vec<crate::quick::Quick>,
    /// The function the bake compiled for exactly these bytes, if it
    /// compiled one, and which member of it this block is: a table index
    /// the run loop calls instead of interpreting. Looked up when the block
    /// is decoded and never again, which is what makes it free — see
    /// [`crate::tier1`].
    pub compiled: Option<(u32, u32, u64)>,
    /// Pages the compiled region's other members sit on, which the block
    /// is registered against too: a write to any of them drops the block,
    /// because the region was compiled for all of them together.
    pub also: Vec<u64>,
}

impl Block {
    /// The pages the block's bytes came from.
    fn pages(&self) -> std::ops::RangeInclusive<u64> {
        (self.entry >> PAGE_SHIFT)..=((self.end - 1) >> PAGE_SHIFT)
    }

    /// Every page the block is registered against: its own, and its
    /// compiled region's.
    fn all_pages(&self) -> Vec<u64> {
        let mut pages: Vec<u64> = self.pages().collect();
        for page in &self.also {
            if !pages.contains(page) {
                pages.push(*page);
            }
        }
        pages
    }
}

/// Why a block ended where it did. Nothing acts on this yet; it is what a
/// hot-block tier will select traces by, and what a diagnostic prints.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Terminator {
    /// The last instruction transfers control.
    Transfer,
    /// The last instruction is a `syscall`. The loop owns syscalls, so a
    /// block never contains one anywhere but at its end — the same rule a
    /// compiled trace will have to obey.
    Syscall,
    /// The block hit [`MAX_INSTRUCTIONS`], or ran out of executable bytes.
    /// It falls through to [`Block::end`].
    FellThrough,
}

/// What a fetch could not do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FetchError {
    /// The bytes are not there, or not executable. A real `SIGSEGV`.
    Fault(Fault),
    /// The bytes are there and they are not an instruction. A real `SIGILL`.
    Undefined { address: u64 },
}

impl From<Fault> for FetchError {
    fn from(fault: Fault) -> Self {
        FetchError::Fault(fault)
    }
}

/// Decoded blocks, keyed by entry address, and the page registry that
/// invalidation hangs on.
pub struct BlockCache {
    /// A slab: invalidated blocks leave a hole for the next one, so a block
    /// index is stable for as long as the block lives.
    blocks: Vec<Option<Block>>,
    free: Vec<usize>,
    entries: HashMap<u64, usize>,
    /// A direct-mapped cache in front of `entries`.
    ///
    /// Blocks end at the first control transfer, and control transfers turn
    /// out to be a fifth of everything a real program retires — so a block
    /// averages about five instructions and this lookup is paid every fifth
    /// one, not once in a while. `HashMap`'s hasher is SipHash, which is
    /// chosen to resist an adversary picking keys; the keys here are a
    /// guest's own instruction addresses, and the worst a bad distribution
    /// costs is a miss into the map that was going to be consulted anyway.
    ///
    /// **It validates rather than invalidates.** A hit is only believed
    /// after the block it names is confirmed to still be entered at the
    /// address asked for — so a block that was freed, or whose slab index
    /// was reused by another block, simply misses. Nothing has to remember
    /// to clear this, which is the property the alternative did not have:
    /// a stale entry here would hand back a block decoded from bytes the
    /// guest has since overwritten, and that is the one bug this whole file
    /// exists to make impossible.
    recent: Vec<(u64, usize)>,
    /// Which blocks took bytes from each page. The list is short —
    /// a page holds a few dozen blocks — so removal is a scan.
    registry: HashMap<u64, Vec<usize>>,
    /// How many blocks have been decoded since the cache was last emptied,
    /// for the diagnostics that ask how much decoding a workload does.
    pub decoded: u64,
    /// Of those, how many got a tier-1 compiled function attached — the
    /// numerator of the attach rate, which separates "the bake compiled
    /// nothing that matches" from "it matched but never ran".
    pub attached: u64,
    pub flushes: u64,
}

impl Default for BlockCache {
    fn default() -> Self {
        Self {
            blocks: Vec::new(),
            free: Vec::new(),
            entries: HashMap::new(),
            // Allocated once, here, rather than checked for on a path that
            // runs every fifth instruction.
            recent: vec![(u64::MAX, 0); RECENT],
            registry: HashMap::new(),
            decoded: 0,
            attached: 0,
            flushes: 0,
        }
    }
}

impl BlockCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many blocks are cached.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The block entered at `address`, decoding it if it is not cached.
    ///
    /// The returned index stays valid until something invalidates the block,
    /// which nothing can do while the caller is executing it: the store path
    /// only *queues* pages, and the loop drains the queue between blocks.
    pub fn entry(&mut self, address: u64, space: &mut Space) -> Result<usize, FetchError> {
        // Shifted by four before masking: instruction addresses are dense
        // and their low bits are the ones that differ, but so are the bits
        // just above, and a block entry is never one byte from another.
        let slot = ((address >> 4) as usize) & (RECENT - 1);
        let (cached, index) = self.recent[slot];
        if cached == address
            && let Some(block) = &self.blocks[index]
            && block.entry == address
        {
            return Ok(index);
        }
        if let Some(index) = self.entries.get(&address) {
            self.recent[slot] = (address, *index);
            return Ok(*index);
        }
        let block = decode(address, space)?;
        let index = self.install(block, space);
        self.recent[slot] = (address, index);
        Ok(index)
    }

    pub fn block(&self, index: usize) -> &Block {
        self.blocks[index]
            .as_ref()
            .expect("a block index outlived its block")
    }

    fn install(&mut self, block: Block, space: &mut Space) -> usize {
        if self.entries.len() >= CAPACITY {
            self.flush(space);
        }
        if block.compiled.is_some() {
            self.attached += 1;
        }
        let pages: Vec<u64> = block.all_pages();
        let entry = block.entry;
        let index = match self.free.pop() {
            Some(index) => {
                self.blocks[index] = Some(block);
                index
            }
            None => {
                self.blocks.push(Some(block));
                self.blocks.len() - 1
            }
        };
        self.entries.insert(entry, index);
        for page in pages {
            self.registry.entry(page).or_default().push(index);
            space.mark_code(page);
        }
        self.decoded += 1;
        index
    }

    /// Drops every block that took bytes from `page`.
    ///
    /// The pages of a dropped block are *all* deregistered, not just the one
    /// that triggered this: a block straddling a boundary is stale on both
    /// sides once either side changes.
    pub fn invalidate_page(&mut self, page: u64, space: &mut Space) {
        let Some(indices) = self.registry.remove(&page) else {
            return;
        };
        for index in indices {
            let Some(block) = self.blocks[index].take() else {
                continue;
            };
            self.entries.remove(&block.entry);
            for other in block.all_pages() {
                if other == page {
                    continue;
                }
                if let Some(list) = self.registry.get_mut(&other) {
                    list.retain(|candidate| *candidate != index);
                    if list.is_empty() {
                        self.registry.remove(&other);
                        space.clear_code(other);
                    }
                }
            }
            self.free.push(index);
        }
        // The store path already cleared this page's bit on its way to
        // queueing it; unmapping clears it too. Saying so again is harmless
        // and makes the invariant hold no matter who called.
        space.clear_code(page);
    }

    /// Drains every page a write has landed on since the last drain.
    ///
    /// Called by the run loop between blocks, which is what guarantees the
    /// next instruction is fetched from bytes that are current.
    pub fn drain_invalidations(&mut self, space: &mut Space) {
        while space.has_dirty_code() {
            for page in space.take_dirty_code() {
                self.invalidate_page(page, space);
            }
        }
    }

    /// Throws the whole cache away.
    pub fn flush(&mut self, space: &mut Space) {
        for page in self.registry.keys() {
            space.clear_code(*page);
        }
        self.blocks.clear();
        self.free.clear();
        self.entries.clear();
        self.registry.clear();
        self.flushes += 1;
    }
}

/// Decodes one block starting at `address`.
fn decode(address: u64, space: &Space) -> Result<Block, FetchError> {
    let cap = MAX_INSTRUCTION_BYTES * MAX_INSTRUCTIONS as u64;
    let bytes = space.fetch(address, cap)?;
    let mut decoder = Decoder::with_ip(64, bytes, address, DecoderOptions::NONE);
    let mut instructions = Vec::new();
    let mut end = address;
    while instructions.len() < MAX_INSTRUCTIONS {
        if !decoder.can_decode() {
            break;
        }
        let instruction = decoder.decode();
        if instruction.is_invalid() {
            return match decoder.last_error() {
                // The bytes ran out. Either the executable run ends here —
                // in which case the *next* block will fetch from the page
                // that refused and report the fault — or the block simply
                // reached the cap. Both are "the block ends here", and the
                // empty case cannot loop forever because the cap is longer
                // than the longest instruction.
                DecoderError::NoMoreBytes if !instructions.is_empty() => {
                    let (compiled, also) = attach(bytes, address, end, space);
                    Ok(Block {
                        entry: address,
                        end,
                        quick: lower_all(&instructions),
                        simple: straight_through(&instructions),
                        compiled,
                        also,
                        instructions,
                    })
                }
                DecoderError::NoMoreBytes => Err(FetchError::Fault(Fault {
                    address: address + bytes.len() as u64,
                    access: Access::Fetch,
                })),
                _ => Err(FetchError::Undefined {
                    address: instruction.ip(),
                }),
            };
        }
        end = instruction.next_ip();
        let terminates = terminator(&instruction).is_some();
        instructions.push(instruction);
        if terminates {
            break;
        }
    }
    let (compiled, also) = attach(bytes, address, end, space);
    Ok(Block {
        entry: address,
        end,
        quick: lower_all(&instructions),
        simple: straight_through(&instructions),
        compiled,
        also,
        instructions,
    })
}

/// What the bake compiled for a decoded block's bytes, if anything: the
/// region's function and member, and the pages of the region's other
/// members. See [`crate::tier1::lookup`].
fn attach(bytes: &[u8], address: u64, end: u64, space: &Space) -> (Option<(u32, u32, u64)>, Vec<u64>) {
    match crate::tier1::lookup(&bytes[..(end - address) as usize], address, space) {
        Some(found) => (Some((found.function, found.which, found.base)), found.pages),
        None => (None, Vec::new()),
    }
}

/// Lowers a block's instructions and records, for each, whether the run
/// loop has to consult `rip` afterwards.
///
/// That is a property of the instruction rather than of the lowering: an
/// instruction iced calls `Next` with no repeat prefix goes to the next one
/// whether or not [`crate::quick`] understood it.
fn lower_all(instructions: &[Instruction]) -> Vec<crate::quick::Quick> {
    instructions
        .iter()
        .map(|instruction| {
            let mut lowered = crate::quick::Quick::lower(instruction);
            lowered.checks_rip = instruction.flow_control() != FlowControl::Next
                || instruction.has_rep_prefix()
                || instruction.has_repe_prefix()
                || instruction.has_repne_prefix();
            lowered
        })
        .collect()
}

/// Whether every instruction but the last simply falls through.
///
/// Conservative on both counts: anything iced does not call `Next`, and
/// anything carrying a repeat prefix, makes the block ordinary. Being wrong
/// in the other direction would mean the loop stopped consulting `rip` for
/// an instruction that changes it, which is a silent jump to the wrong
/// place — so the test is over what the *prefix* contains rather than over
/// what the terminator is.
fn straight_through(instructions: &[Instruction]) -> bool {
    let Some((_, prefix)) = instructions.split_last() else {
        return true;
    };
    prefix.iter().all(|instruction| {
        instruction.flow_control() == FlowControl::Next
            && !instruction.has_rep_prefix()
            && !instruction.has_repe_prefix()
            && !instruction.has_repne_prefix()
    })
}

/// Whether an instruction ends its block, and why.
///
/// Control transfers, obviously. And `syscall`, deliberately: the loop owns
/// syscalls, so no block ever holds one in its middle. That costs a map
/// probe per syscall — against the thousands of instructions a syscall
/// itself costs — and buys two things. The kernel writes guest memory, so a
/// syscall is a moment when the cache may need to be invalidated, and having
/// it at a block boundary means the drain that handles it is the same drain
/// everything else uses. And it is the rule a compiled trace will have to
/// obey anyway, so tier zero and tier one agree about where a block can end.
pub fn terminator(instruction: &Instruction) -> Option<Terminator> {
    if matches!(
        instruction.mnemonic(),
        Mnemonic::Syscall | Mnemonic::Sysenter | Mnemonic::Sysexit | Mnemonic::Sysret
    ) {
        return Some(Terminator::Syscall);
    }
    match instruction.flow_control() {
        FlowControl::Next => None,
        // A conditional branch does *not* end a block, and that is the
        // whole of the extended-basic-block idea. Its fall-through is the
        // next instruction, so decoding can carry straight on; when it is
        // taken the run loop notices `rip` disagreeing and leaves.
        //
        // Blocks were averaging five instructions because control transfers
        // are a fifth of what a real program retires, and a profile of the
        // Django import put the run loop and the block lookup at 22% of the
        // engine between them — all of it paid per block. Following the
        // fall-through is what makes a block long enough for that to be
        // amortised.
        //
        // Calls and returns still end one. A `call` comes back to the
        // instruction after it, which would be the middle of this block,
        // and a block is only ever entered at its start — so continuing
        // past one builds bytes that can never be reached through this
        // entry.
        FlowControl::ConditionalBranch => None,
        _ => Some(Terminator::Transfer),
    }
}

#[cfg(test)]
mod tests {
    use iced_x86::code_asm::*;

    use super::*;
    use crate::arena::Arena;
    use crate::space::{PAGE_SIZE, Protection};
    use crate::state::Width;

    /// Assembles at `at` and returns the bytes.
    fn assemble(at: u64, build: impl FnOnce(&mut CodeAssembler)) -> Vec<u8> {
        let mut assembler = CodeAssembler::new(64).expect("assembler");
        build(&mut assembler);
        assembler.assemble(at).expect("assemble")
    }

    struct Fixture {
        arena: Arena,
        space: Space,
        cache: BlockCache,
    }

    impl Fixture {
        fn new(length: u64) -> Self {
            let arena = Arena::new(length);
            let mut space = Space::new(arena.limit());
            space.protect(arena.base(), arena.length(), Protection::ALL);
            Self {
                arena,
                space,
                cache: BlockCache::new(),
            }
        }

        fn place(&mut self, at: u64, bytes: &[u8]) {
            self.space.write(at, bytes).expect("place code");
            // Placing is not the guest storing: a harness writing a program
            // into memory before it runs must not leave the pages queued for
            // invalidation.
            self.cache.drain_invalidations(&mut self.space);
        }

        fn base(&self) -> u64 {
            self.arena.base()
        }
    }

    #[test]
    fn a_block_ends_at_the_first_control_transfer() {
        let mut fixture = Fixture::new(0x2_0000);
        let at = fixture.base();
        let code = assemble(at, |assembler| {
            assembler.mov(rax, 1u64).unwrap();
            assembler.add(rax, rcx).unwrap();
            assembler.ret().unwrap();
            assembler.mov(rbx, 2u64).unwrap();
        });
        fixture.place(at, &code);
        let index = fixture.cache.entry(at, &mut fixture.space).unwrap();
        let block = fixture.cache.block(index);
        assert_eq!(block.instructions.len(), 3);
        assert_eq!(
            block.instructions.last().unwrap().mnemonic(),
            Mnemonic::Ret
        );
    }

    #[test]
    fn a_block_ends_at_a_syscall() {
        let mut fixture = Fixture::new(0x2_0000);
        let at = fixture.base();
        let code = assemble(at, |assembler| {
            assembler.mov(eax, 60u32).unwrap();
            assembler.syscall().unwrap();
            assembler.mov(ebx, 1u32).unwrap();
        });
        fixture.place(at, &code);
        let index = fixture.cache.entry(at, &mut fixture.space).unwrap();
        let block = fixture.cache.block(index);
        assert_eq!(block.instructions.len(), 2);
        assert_eq!(
            terminator(block.instructions.last().unwrap()),
            Some(Terminator::Syscall)
        );
    }

    #[test]
    fn a_straight_run_is_capped() {
        let mut fixture = Fixture::new(0x2_0000);
        let at = fixture.base();
        let code = assemble(at, |assembler| {
            for _ in 0..MAX_INSTRUCTIONS + 10 {
                assembler.nop().unwrap();
            }
        });
        fixture.place(at, &code);
        let index = fixture.cache.entry(at, &mut fixture.space).unwrap();
        let block = fixture.cache.block(index);
        assert_eq!(block.instructions.len(), MAX_INSTRUCTIONS);
        assert_eq!(block.end, at + MAX_INSTRUCTIONS as u64);
    }

    #[test]
    fn a_second_entry_into_the_same_bytes_is_a_second_block() {
        let mut fixture = Fixture::new(0x2_0000);
        let at = fixture.base();
        let code = assemble(at, |assembler| {
            assembler.nop().unwrap();
            assembler.nop().unwrap();
            assembler.ret().unwrap();
        });
        fixture.place(at, &code);
        let first = fixture.cache.entry(at, &mut fixture.space).unwrap();
        let second = fixture.cache.entry(at + 1, &mut fixture.space).unwrap();
        assert_ne!(first, second);
        assert_eq!(fixture.cache.block(first).instructions.len(), 3);
        assert_eq!(fixture.cache.block(second).instructions.len(), 2);
        assert_eq!(fixture.cache.len(), 2);
    }

    #[test]
    fn a_store_to_a_cached_page_drops_the_block() {
        let mut fixture = Fixture::new(0x2_0000);
        let at = fixture.base();
        let code = assemble(at, |assembler| {
            assembler.nop().unwrap();
            assembler.ret().unwrap();
        });
        fixture.place(at, &code);
        fixture.cache.entry(at, &mut fixture.space).unwrap();
        assert_eq!(fixture.cache.len(), 1);
        fixture.space.store(at, Width::Byte, 0x90).unwrap();
        assert!(fixture.space.has_dirty_code());
        fixture.cache.drain_invalidations(&mut fixture.space);
        assert_eq!(fixture.cache.len(), 0);
        // And the page is no longer marked, so the next store to it is free.
        fixture.space.store(at, Width::Byte, 0x90).unwrap();
        assert!(!fixture.space.has_dirty_code());
    }

    /// The silent class: a block whose bytes straddle a page boundary, with
    /// the write landing on the *second* page. Registering only the entry
    /// page passes every other test in this file and fails this one.
    #[test]
    fn a_block_spanning_two_pages_is_registered_against_both() {
        let mut fixture = Fixture::new(0x4_0000);
        let at = fixture.base() + PAGE_SIZE - 3;
        let code = assemble(at, |assembler| {
            assembler.nop().unwrap();
            assembler.nop().unwrap();
            assembler.nop().unwrap();
            assembler.nop().unwrap();
            assembler.ret().unwrap();
        });
        fixture.place(at, &code);
        fixture.cache.entry(at, &mut fixture.space).unwrap();
        assert_eq!(fixture.cache.len(), 1);
        // A store to the second page, which holds the block's tail.
        fixture
            .space
            .store(fixture.base() + PAGE_SIZE, Width::Byte, 0x90)
            .unwrap();
        fixture.cache.drain_invalidations(&mut fixture.space);
        assert_eq!(fixture.cache.len(), 0, "the tail page was not registered");
    }

    #[test]
    fn a_fetch_from_a_non_executable_page_is_a_fault() {
        let arena = Arena::new(0x2_0000);
        let mut space = Space::new(arena.limit());
        space.protect(arena.base(), PAGE_SIZE, Protection::READ_WRITE);
        let mut cache = BlockCache::new();
        assert_eq!(
            cache.entry(arena.base(), &mut space),
            Err(FetchError::Fault(Fault {
                address: arena.base(),
                access: Access::Fetch,
            }))
        );
    }

    #[test]
    fn bytes_that_are_not_an_instruction_are_undefined_rather_than_a_fault() {
        let mut fixture = Fixture::new(0x2_0000);
        let at = fixture.base();
        // `ud2`, decoded as an instruction, is not this case; a genuinely
        // unassignable opcode is.
        fixture.place(at, &[0x0f, 0x0b]);
        let index = fixture.cache.entry(at, &mut fixture.space).unwrap();
        assert_eq!(
            fixture.cache.block(index).instructions[0].mnemonic(),
            Mnemonic::Ud2
        );
        let elsewhere = at + 0x100;
        fixture.place(elsewhere, &[0xff, 0xff]);
        assert_eq!(
            fixture.cache.entry(elsewhere, &mut fixture.space),
            Err(FetchError::Undefined { address: elsewhere })
        );
    }

    #[test]
    fn a_block_at_the_end_of_the_executable_run_faults_on_the_next_page() {
        let arena = Arena::new(0x4_0000);
        let mut space = Space::new(arena.limit());
        space.protect(arena.base(), PAGE_SIZE, Protection::ALL);
        space.protect(arena.base() + PAGE_SIZE, PAGE_SIZE, Protection::READ_WRITE);
        let mut cache = BlockCache::new();
        // A `mov` with a four-byte immediate, straddling the boundary.
        let at = arena.base() + PAGE_SIZE - 3;
        let code = assemble(at, |assembler| {
            assembler.mov(eax, 0x1234_5678u32).unwrap();
        });
        space.write(at, &code).unwrap();
        cache.drain_invalidations(&mut space);
        assert_eq!(
            cache.entry(at, &mut space),
            Err(FetchError::Fault(Fault {
                address: arena.base() + PAGE_SIZE,
                access: Access::Fetch,
            }))
        );
    }
}
