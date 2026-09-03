//! Tier 1 at run time: blocks the bake compiled, attached to the blocks the
//! cache decodes, and the three helpers compiled code calls back into.
//!
//! The design is `docs/tier1-plan.md`. What lives here is the run-time half
//! and it is deliberately small: a lookup, a call, and three helpers. The
//! compiler is in the bake (`zaqaru::tier1`), and nothing here knows what a
//! compiled block does inside — only what it is handed and what it hands
//! back.
//!
//! **Keyed by bytes.** The bake linked in a table of `(hash, length, bytes,
//! function)`; when the cache decodes a block, [`lookup`] hashes the bytes
//! and probes it, and a hit is believed only after the bytes are compared
//! whole. So a compiled block is only ever used for exactly the bytes it was
//! compiled from — wherever they are mapped, in whichever process — and a
//! block the bake compiled for code that never runs, or that the guest has
//! since overwritten, is a function nobody enters. That property is what
//! lets the bake guess.
//!
//! **The contract**, which the compiler and the run loop both honour:
//!
//! ```text
//! (tcb: i32, vitals: i32, entry: i64, budget: i64) -> i64
//! ```
//!
//! `tcb` and `vitals` are addresses in linear memory; `entry` is where the
//! block is running this time, since a compiled block is written entirely
//! as `entry` plus deltas; `budget` is how many instructions may still
//! retire this quantum. The answer packs an exit kind above a guest address:
//! [`KIND_CONTINUE`] with the next instruction to execute, [`KIND_SYSCALL`]
//! past the `syscall` with `%rcx` and `%r11` as hardware leaves them, or
//! [`KIND_INTERPRET`] naming an instruction the block declined — a fault it
//! saw coming, a store onto code — which the interpreter runs *once* before
//! compiled code is consulted for that block again. On every exit the
//! control block is complete: registers, `rip`, the flags record and
//! `retired` are what the interpreter would have left.
//!
//! **The helpers.** An instruction the `Quick` lowering declines is run by
//! [`targum_step`], which is `Cpu::step` reached from compiled code, against
//! the decoded block the run loop was about to interpret; a condition whose
//! feeding operation the compiler could not see is answered by
//! [`targum_condition`]; and a store to a page holding code is reported
//! through [`targum_code_write`] before the block leaves. All three read
//! the loop's own state — the address space, the cache, the block — from a
//! context the loop sets before every compiled call, because a helper has
//! no other way to reach what the loop holds mutably.

use crate::block::BlockCache;
use crate::space::{Space, Vitals};
use crate::state::Tcb;

/// The exit kinds, in the high 32 bits of a compiled block's answer.
pub const KIND_CONTINUE: u64 = 0;
pub const KIND_SYSCALL: u64 = 1;
pub const KIND_INTERPRET: u64 = 2;
const KIND_SHIFT: u32 = 32;

/// What a helper answers, and what compiled code does with it: fell through
/// (carry on), reached a `syscall` (exit `KIND_SYSCALL`), went elsewhere
/// (exit `KIND_CONTINUE` at `rip`), or trapped (exit `KIND_INTERPRET` at
/// `rip`, which the helper left at the instruction).
pub const STEP_FELL_THROUGH: u32 = 0;
pub const STEP_SYSCALL: u32 = 1;
pub const STEP_ELSEWHERE: u32 = 2;
pub const STEP_TRAPPED: u32 = 3;

/// The table's header: magic, entry count, offset of the bytes region,
/// offset of the regions, offset of the members, region count, padding.
pub const TABLE_MAGIC: u32 = u32::from_le_bytes(*b"TGT1");
pub const TABLE_HEADER: usize = 32;
/// One entry: hash `u64`, length `u32`, offset of the bytes `u32`, function
/// `u32`, which member `u32`, region `u32`, padding. Sorted by hash, then
/// length.
pub const TABLE_ENTRY: usize = 32;
/// One region: member count `u32`, index of its first member row `u32`.
pub const TABLE_REGION: usize = 8;
/// One member row: delta from the region's base `i32`, length `u32`,
/// offset of the bytes `u32`. Rows are in dispatch order.
pub const TABLE_MEMBER: usize = 12;

/// What a lookup attaches to a decoded block: the region's function, the
/// member the block is, and every page any member's bytes sit on — so
/// that a write to any of them drops the block, and with it the region.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attached {
    pub function: u32,
    pub which: u32,
    pub pages: Vec<u64>,
}

/// The bytes' hash, FNV-1a: what the bake wrote and what the lookup probes
/// with. Not a defence against anything — the bytes are compared whole on
/// a hit — so speed and a good spread are all it needs.
pub fn hash(bytes: &[u8]) -> u64 {
    let mut state = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        state ^= u64::from(*byte);
        state = state.wrapping_mul(0x0100_0000_01b3);
    }
    state
}

/// The function compiled for exactly these bytes, if the bake compiled one.
///
/// Inside the module the table is what the bake linked in under
/// `targum_tier1_table`; natively there is no compiled code, so there is
/// no table, and the answer is always no — the native engine is tier 0
/// and remains the development instrument.
#[cfg(target_arch = "wasm32")]
pub fn lookup(bytes: &[u8], address: u64, space: &Space) -> Option<Attached> {
    unsafe extern "C" {
        static targum_tier1_table: u8;
    }
    // SAFETY: the bake always links a table, empty or not, at this symbol,
    // and its header says how far it extends.
    let table = core::ptr::addr_of!(targum_tier1_table);
    let word = |at: usize| -> u32 {
        // SAFETY: inside the table, whose extent the header states.
        unsafe { core::ptr::read_unaligned(table.add(at) as *const u32) }
    };
    let long = |at: usize| -> u64 {
        // SAFETY: as above.
        unsafe { core::ptr::read_unaligned(table.add(at) as *const u64) }
    };
    if word(0) != TABLE_MAGIC {
        return None;
    }
    let count = word(4) as usize;
    let bytes_at = word(8) as usize;
    let regions_at = word(12) as usize;
    let members_at = word(16) as usize;
    let wanted = hash(bytes);
    // Binary search on the hash, then a walk over equal hashes: a
    // collision is two entries, and the bytes decide.
    let (mut low, mut high) = (0usize, count);
    while low < high {
        let middle = (low + high) / 2;
        match long(TABLE_HEADER + middle * TABLE_ENTRY).cmp(&wanted) {
            core::cmp::Ordering::Less => low = middle + 1,
            _ => high = middle,
        }
    }
    let mut index = low;
    while index < count {
        let at = TABLE_HEADER + index * TABLE_ENTRY;
        if long(at) != wanted {
            break;
        }
        let length = word(at + 8) as usize;
        let offset = word(at + 12) as usize;
        if length == bytes.len() {
            // SAFETY: the bytes region, whose entries the bake wrote whole.
            let stored = unsafe { core::slice::from_raw_parts(table.add(bytes_at + offset), length) };
            if stored == bytes {
                let function = word(at + 16);
                let which = word(at + 20);
                let region = word(at + 24) as usize;
                // Every other member of the region has to be where the
                // region expects it, byte for byte, and executable: the
                // function was compiled for all of them at fixed deltas
                // from one another, and a region that survived with a
                // changed member would run bytes that are not there.
                let members = word(regions_at + region * TABLE_REGION) as usize;
                let first = word(regions_at + region * TABLE_REGION + 4) as usize;
                let row = |member: usize| -> (i64, usize, usize) {
                    let at = members_at + (first + member) * TABLE_MEMBER;
                    (i64::from(word(at) as i32), word(at + 4) as usize, word(at + 8) as usize)
                };
                let (my_delta, _, _) = row(which as usize);
                let base = address.wrapping_sub(my_delta as u64);
                let mut pages = Vec::new();
                for member in 0..members {
                    let (delta, length, offset) = row(member);
                    let at = base.wrapping_add(delta as u64);
                    let Ok(found) = space.fetch(at, length as u64) else {
                        return None;
                    };
                    // SAFETY: as above.
                    let expected = unsafe { core::slice::from_raw_parts(table.add(bytes_at + offset), length) };
                    if found.len() != length || found != expected {
                        return None;
                    }
                    for page in (at >> crate::space::PAGE_SHIFT)..=((at + length as u64 - 1) >> crate::space::PAGE_SHIFT) {
                        if !pages.contains(&page) {
                            pages.push(page);
                        }
                    }
                }
                return Some(Attached { function, which, pages });
            }
        }
        index += 1;
    }
    None
}

#[cfg(not(target_arch = "wasm32"))]
pub fn lookup(_bytes: &[u8], _address: u64, _space: &Space) -> Option<Attached> {
    None
}

/// What a helper reaches that the run loop holds: the address space, the
/// cache, and the block being run. Raw, because the loop holds all three
/// mutably across the call and a helper is a call *from* the callee.
#[derive(Clone, Copy)]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
struct Context {
    space: *mut Space,
    cache: *mut BlockCache,
    block: usize,
}

thread_local! {
    static CONTEXT: core::cell::Cell<Context> = const {
        core::cell::Cell::new(Context {
            space: core::ptr::null_mut(),
            cache: core::ptr::null_mut(),
            block: 0,
        })
    };
}

/// Sets what the helpers will find, for the compiled call about to be made.
pub fn enter(space: &mut Space, cache: &mut BlockCache, block: usize) {
    RECENT.with(|held| {
        let mut ring = held.borrow_mut();
        let at = ring.0;
        ring.1[at] = cache.block(block).entry;
        ring.0 = (at + 1) % RECENT_ENTRIES;
    });
    CONTEXT.with(|held| {
        held.set(Context {
            space: space as *mut Space,
            cache: cache as *mut BlockCache,
            block,
        })
    });
}

const RECENT_ENTRIES: usize = 16;

thread_local! {
    /// The last compiled blocks entered, oldest overwritten first: what a
    /// fault report names, because the block that went wrong is usually
    /// one of the last few and never the one that faulted.
    static RECENT: core::cell::RefCell<(usize, [u64; RECENT_ENTRIES])> =
        const { core::cell::RefCell::new((0, [0; RECENT_ENTRIES])) };
}

/// The compiled blocks entered most recently, most recent last.
pub fn recent_entries() -> Vec<u64> {
    RECENT.with(|held| {
        let ring = held.borrow();
        (0..RECENT_ENTRIES)
            .map(|index| ring.1[(ring.0 + index) % RECENT_ENTRIES])
            .filter(|entry| *entry != 0)
            .collect()
    })
}

/// Calls the compiled block at a table index.
///
/// On wasm32 a function pointer *is* a table index, so the cast is the
/// call. Natively nothing is ever compiled and this is unreachable.
#[cfg(target_arch = "wasm32")]
pub fn call(function: u32, tcb: &mut Tcb, vitals: &Vitals, entry: u64, budget: u64, which: u32) -> u64 {
    type Compiled = unsafe extern "C" fn(*mut Tcb, *const Vitals, u64, u64, u32) -> u64;
    // SAFETY: the index came from the bake's table, whose functions all
    // have this signature, and the linker put them in the module's table.
    let compiled: Compiled = unsafe { core::mem::transmute(function as usize) };
    unsafe { compiled(tcb, vitals, entry, budget, which) }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn call(_function: u32, _tcb: &mut Tcb, _vitals: &Vitals, _entry: u64, _budget: u64, _which: u32) -> u64 {
    unreachable!("nothing is compiled natively")
}

pub fn exit_kind(exit: u64) -> u64 {
    exit >> KIND_SHIFT
}

pub fn exit_rip(exit: u64) -> u64 {
    exit & 0xffff_ffff
}

/// Runs one instruction the block declined to compile, and says what it did.
///
/// `position` is the instruction's index in the block the loop set in the
/// context. The interpreter's own rules put `rip` and `retired` where they
/// go; on a trap they are put back to the instruction, so that the exit
/// the caller makes hands the interpreter an instruction that has not
/// happened yet, and the trap happens there with the frame it builds.
#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub extern "C" fn targum_step(tcb: *mut Tcb, address: u64) -> u32 {
    use crate::exec::{Cpu, Step};
    let context = CONTEXT.with(|held| held.get());
    // SAFETY: the loop set these for the duration of the compiled call.
    let (space, cache, tcb) = unsafe { (&mut *context.space, &mut *context.cache, &mut *tcb) };
    // One instruction, named by its guest address rather than a position
    // into some block — a region runs instructions from many blocks, so a
    // position names the wrong one. The block cache decodes it once and
    // remembers, which matters because a declined instruction inside a hot
    // loop is reached every iteration: a block entered at the address holds
    // it as its first instruction. A fault fetching is the trap the
    // interpreter would have raised.
    let index = match cache.entry(address, space) {
        Ok(index) => index,
        Err(_) => return STEP_TRAPPED,
    };
    let instruction = cache.block(index).instructions[0];
    let mut cpu = Cpu::new(tcb, space);
    match cpu.step(&instruction) {
        Ok(Step::Syscall) => STEP_SYSCALL,
        Ok(Step::Retired) => {
            if cpu.tcb.rip == instruction.next_ip() && !cpu.space.has_dirty_code() {
                STEP_FELL_THROUGH
            } else {
                STEP_ELSEWHERE
            }
        }
        Err(_) => {
            cpu.tcb.rip = instruction.ip();
            cpu.tcb.retired = cpu.tcb.retired.wrapping_sub(1);
            STEP_TRAPPED
        }
    }
}

/// Whether a condition holds, for a branch whose feeding operation the
/// compiler could not see. `Condition::holds`, reached from compiled code.
#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub extern "C" fn targum_condition(tcb: *const Tcb, condition: u32) -> u32 {
    // SAFETY: the loop's control block, for the duration of the call.
    let tcb = unsafe { &*tcb };
    match crate::flags::Condition::from_code(condition as u8) {
        Some(condition) => u32::from(condition.holds(&tcb.flags)),
        None => 0,
    }
}

/// A store landed on a page some block was decoded from. Queues it exactly
/// as the interpreter's own store does; the block exits right after.
#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub extern "C" fn targum_code_write(address: u64, length: u32) {
    let context = CONTEXT.with(|held| held.get());
    // SAFETY: the loop's address space, for the duration of the call.
    let space = unsafe { &mut *context.space };
    space.note_code_write(address, u64::from(length));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_is_the_one_the_bake_writes() {
        // FNV-1a's published value for the empty string and for "a".
        assert_eq!(hash(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(hash(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn an_exit_packs_a_kind_above_an_address() {
        let exit = (KIND_INTERPRET << KIND_SHIFT) | 0x1234_5678;
        assert_eq!(exit_kind(exit), KIND_INTERPRET);
        assert_eq!(exit_rip(exit), 0x1234_5678);
    }
}

/// The first half of verifying a compiled block: the interpreter runs the
/// block first, from the control block as it stands, on memory as it
/// stands, recording the machine after every retired instruction; then
/// every byte its stores changed is put back. What is left is memory
/// exactly as the compiled block is about to find it, and a record of
/// what the interpreter would make of it at each step.
///
/// Interpreting first is what makes this sound. Running the compiled block
/// first and re-interpreting afterwards would hand the interpreter memory
/// the compiled block had already written — a slot read and then
/// overwritten in the same block reads differently the second time.
/// What the interpreter did, step by step, so the compiled block can be
/// checked against it afterwards: the machine after each retired
/// instruction, and the memory each one wrote (address and its new bytes).
#[cfg(feature = "verify")]
pub struct Trace {
    pub states: Vec<Tcb>,
    /// `writes[i]` is what the i-th retired instruction stored: the new
    /// bytes, by address. Parallel to `states[1..]`.
    pub writes: Vec<Vec<(u64, Vec<u8>)>>,
}

#[cfg(feature = "verify")]
pub fn verify_before(tcb: &Tcb, space: &mut Space, cache: &mut BlockCache, cap: usize) -> Trace {
    use crate::exec::{Cpu, Step};
    let mut check = tcb.clone();
    let mut states = vec![check.clone()];
    let mut writes: Vec<Vec<(u64, Vec<u8>)>> = Vec::new();
    space.journal_begin();
    let mut cursor = 0usize;
    // Reads the journal entries added since the cursor, capturing the bytes
    // they now hold — the interpreter's new values, before any undo.
    macro_rules! capture {
        ($space:expr) => {{
            let mut step = Vec::new();
            while cursor < $space.journal_len() {
                if let Some((address, length)) = $space.journal_entry(cursor) {
                    step.push((address, $space.peek(address, length)));
                }
                cursor += 1;
            }
            writes.push(step);
        }};
    }
    // Across blocks, because a region runs through many: fetch at `rip`,
    // run the block, follow where it went, up to the cap, which is more
    // than a region can retire in one call.
    'blocks: while states.len() <= cap {
        let Ok(index) = cache.entry(check.rip, space) else {
            break;
        };
        let block = cache.block(index);
        let mut cpu = Cpu::new(&mut check, space);
        let mut position = 0usize;
        while position < block.instructions.len() && states.len() <= cap {
            let instruction = &block.instructions[position];
            let before = cpu.tcb.retired;
            match cpu.run(&block.quick[position], instruction) {
                Ok(Step::Retired) => {}
                Ok(Step::Syscall) => {
                    states.push(cpu.tcb.clone());
                    capture!(cpu.space);
                    break 'blocks;
                }
                Err(_) => {
                    cpu.tcb.rip = instruction.ip();
                    cpu.tcb.retired = before;
                    break 'blocks;
                }
            }
            states.push(cpu.tcb.clone());
            capture!(cpu.space);
            if cpu.space.has_dirty_code() {
                break 'blocks;
            }
            if cpu.tcb.rip == instruction.ip() {
                continue;
            }
            if cpu.tcb.rip != instruction.next_ip() {
                break;
            }
            position += 1;
        }
    }
    space.journal_undo();
    Trace { states, writes }
}

/// The second half: the compiled block has run, and the machine it left is
/// compared with the interpreter's at the same number of retired
/// instructions. A block that stopped early — a `rep` after one iteration,
/// a store onto code, an instruction handed back — is compared where it
/// stopped.
#[cfg(feature = "verify")]
pub fn verify_after(trace: &Trace, after: &Tcb, space: &Space, block: &crate::block::Block, exit: u64) {
    let states = &trace.states;
    let snapshot = &states[0];
    let retired = (after.retired.wrapping_sub(snapshot.retired)) as usize;
    let Some(check) = states.get(retired) else {
        // The region retired more than the interpreter run recorded — a
        // long internal loop, which is exactly the win. Verify cannot
        // follow it without recording millions of machines, so it skips
        // this one; the smaller regions are still checked.
        return;
    };
    let mut differences = Vec::new();
    for number in 0..16 {
        if check.registers[number] != after.registers[number] {
            differences.push(format!(
                "r{number}: interpreted {:#x} compiled {:#x} (was {:#x})",
                check.registers[number], after.registers[number], snapshot.registers[number]
            ));
        }
    }
    let handed_back = exit_kind(exit) == KIND_INTERPRET;
    if !handed_back && check.rip != after.rip {
        differences.push(format!("rip: interpreted {:#x} compiled {:#x}", check.rip, after.rip));
    }
    if !handed_back && check.flags.status() != after.flags.status() {
        differences.push(format!(
            "flags: interpreted {:#x} compiled {:#x}",
            check.flags.status(),
            after.flags.status()
        ));
    }
    if check.fs_base != after.fs_base {
        differences.push(format!("fs_base: interpreted {:#x} compiled {:#x}", check.fs_base, after.fs_base));
    }
    // Memory: the compiled region wrote directly, bypassing the journal, so
    // it is compared against what the interpreter's first `retired` steps
    // left. The last write to a byte wins.
    if !handed_back {
        use std::collections::BTreeMap;
        let mut expected: BTreeMap<u64, u8> = BTreeMap::new();
        for step in trace.writes.iter().take(retired) {
            for (address, bytes) in step {
                for (offset, byte) in bytes.iter().enumerate() {
                    expected.insert(address + offset as u64, *byte);
                }
            }
        }
        let mut shown = 0;
        for (address, byte) in &expected {
            let found = space.peek(*address, 1)[0];
            if found != *byte && shown < 6 {
                differences.push(format!(
                    "mem[{address:#x}]: interpreted {byte:#x} compiled {found:#x}"
                ));
                shown += 1;
            }
        }
    }
    if !differences.is_empty() {
        // Where in the interpreter's own trace the compiled machine
        // actually sits, if anywhere: an offset here is a retired-count
        // bug rather than an arithmetic one, and it names the boundary.
        let matches = |state: &Tcb| -> bool {
            state.registers == after.registers && state.rip == after.rip
        };
        let full_match = states.iter().position(matches);
        let rsp_match: Vec<usize> = states
            .iter()
            .enumerate()
            .filter(|(_, state)| state.registers[4] == after.registers[4] && state.rip == after.rip)
            .map(|(index, _)| index)
            .collect();
        panic!(
            "tier 1 verify: the compiled block at {:#x} ({} instructions, exit {:#x}, {} retired) disagrees with the interpreter:\n  {}\n  compiled machine fully matches interpreter step {:?}; rsp+rip match steps {:?}",
            block.entry,
            block.instructions.len(),
            exit,
            retired,
            differences.join("\n  "),
            full_match,
            rsp_match,
        );
    }
}
