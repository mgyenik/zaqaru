//! The block-level differential: every block the sweep finds in an ELF,
//! run from random machine state both ways — compiled, alone under wasmtime,
//! and interpreted, natively — and the two machines compared afterwards.
//!
//! ```text
//! tier1-diff <elf> [address] [--trials N]
//! ```
//!
//! Blocks with an instruction the lowering declines are skipped: those go
//! through a helper that needs the whole engine, which this bench does not
//! have. Everything else — the ALU, moves, the stack, branches, calls and
//! returns, the inline permission checks — is exercised here, against the
//! interpreter as the oracle, with a RAM window both sides share the
//! contents of and random bytes in it.

use targum::block::BlockCache;
use targum::flags::{Flags, Rule};
use targum::space::{Protection, Space};
use targum::state::{Tcb, Width, layout};
use targum::Engine;
use wasmtime::{Engine as Wasm, Linker, Memory, MemoryType, Module, Ref, RefType, Store, Table, TableType};

const RAM: u64 = 0x2_0000;
const RAM_BYTES: u64 = 0x1_0000;
const TCB: u32 = 0x1000;
const VITALS: u32 = 0x2000;
const BITMAPS: u32 = 0x3000;
/// Words per bitmap in the bench: enough pages for a static binary's data
/// to be mapped at its own addresses.
const BITMAP_WORDS: u32 = 64;
const LIMIT: u64 = 0x100_0000;

/// A data segment of the binary, mapped at its own address on both sides
/// so that `rip`-relative loads — jump tables above all — find what the
/// program would.
#[derive(Clone)]
struct Data {
    address: u64,
    bytes: Vec<u8>,
    writable: bool,
    executable: bool,
}

fn data_segments(elf: &[u8]) -> Vec<Data> {
    let object = zaqaru::reader::ObjectFile::parse(elf).expect("parse the elf");
    object
        .segments
        .iter()
        .filter(|segment| segment.address + segment.memory_size <= LIMIT && segment.address != 0)
        .map(|segment| {
            let mut bytes = segment.bytes.clone();
            bytes.resize(segment.memory_size.max(segment.file_size) as usize, 0);
            Data {
                address: segment.address,
                bytes,
                writable: segment.writable && !segment.executable,
                executable: segment.executable,
            }
        })
        .collect()
}

fn page_range(address: u64, length: u64) -> std::ops::Range<u64> {
    (address >> 12)..((address + length + 0xfff) >> 12)
}
/// The whole control block. Every field is fixed-width, so the layout is
/// the same on both targets and the x87 unit travels with the rest.
const TCB_BYTES: usize = std::mem::size_of::<Tcb>();

/// Runs one instruction of a candidate natively against a copy of the
/// wasm-side machine and RAM, and answers what `targum_step` would.
fn step_natively(candidate: &zaqaru::tier1::Candidate, address: u64, image: &[u8], ram: &[u8], data: &[Data]) -> (i32, Vec<u8>, Vec<u8>, Vec<Data>) {
    use targum::exec::{Cpu, Step};
    let (mut space, _arenas) = native_world(candidate, ram, data);
    let mut cache = BlockCache::new();
    cache.drain_invalidations(&mut space);
    // By address, as the engine's helper does: a block entered at the
    // address holds the declined instruction first.
    let index = cache.entry(address, &mut space).expect("decode");
    let instruction = cache.block(index).instructions[0];
    let mut tcb = Tcb::new();
    // SAFETY: the leading fields are `repr(C)` plain data at this layout.
    unsafe {
        std::ptr::copy_nonoverlapping(image.as_ptr(), &mut tcb as *mut Tcb as *mut u8, TCB_BYTES);
    }
    let mut cpu = Cpu::new(&mut tcb, &mut space);
    let code = match cpu.step(&instruction) {
        Ok(Step::Syscall) => 1,
        Ok(Step::Retired) => {
            if cpu.tcb.rip == instruction.next_ip() && !cpu.space.has_dirty_code() { 0 } else { 2 }
        }
        Err(_) => {
            cpu.tcb.rip = instruction.ip();
            cpu.tcb.retired = cpu.tcb.retired.wrapping_sub(1);
            3
        }
    };
    let mut out = vec![0u8; TCB_BYTES];
    // SAFETY: as above.
    unsafe {
        std::ptr::copy_nonoverlapping(&tcb as *const Tcb as *const u8, out.as_mut_ptr(), TCB_BYTES);
    }
    let mut ram_out = vec![0u8; RAM_BYTES as usize];
    space.read(RAM, &mut ram_out).expect("read the ram");
    let mut data_out = data.to_vec();
    for segment in data_out.iter_mut() {
        if segment.writable {
            space.read(segment.address, &mut segment.bytes).expect("read data");
        }
    }
    (code, out, ram_out, data_out)
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// A machine state both sides start from.
struct Start {
    registers: [u64; 16],
    rule: Rule,
    width: Width,
    left: u64,
    right: u64,
    result: u64,
    ram: Vec<u8>,
}

impl Start {
    fn random(rng: &mut Rng) -> Self {
        let mut registers = [0u64; 16];
        for (number, register) in registers.iter_mut().enumerate() {
            // Mostly addresses inside the RAM window, so that memory
            // operands mostly land; sometimes anything at all.
            *register = match rng.next() % 4 {
                0 => rng.next(),
                _ => RAM + (rng.next() % (RAM_BYTES - 0x100)) & !7,
            };
            if number == 4 {
                *register = RAM + RAM_BYTES / 2;
            }
        }
        let width = match rng.next() % 4 {
            0 => Width::Byte,
            1 => Width::Word,
            2 => Width::Dword,
            _ => Width::Qword,
        };
        let rule = match rng.next() % 3 {
            0 => Rule::Add,
            1 => Rule::Sub,
            _ => Rule::Logic,
        };
        let left = width.truncate(rng.next());
        let right = width.truncate(rng.next());
        let result = width.truncate(match rule {
            Rule::Add => left.wrapping_add(right),
            Rule::Sub => left.wrapping_sub(right),
            _ => left & right,
        });
        let mut ram = vec![0u8; RAM_BYTES as usize];
        for chunk in ram.chunks_mut(8) {
            chunk.copy_from_slice(&rng.next().to_le_bytes()[..chunk.len()]);
        }
        Self {
            registers,
            rule,
            width,
            left,
            right,
            result,
            ram,
        }
    }

    fn tcb(&self, rip: u64) -> Tcb {
        let mut tcb = Tcb::new();
        tcb.registers = self.registers;
        tcb.rip = rip;
        tcb.flags.record(self.rule, self.width, self.left, self.right, self.result);
        tcb
    }
}

/// What a side ended with.
#[derive(Debug, PartialEq)]
struct End {
    registers: [u64; 16],
    rip: u64,
    retired: u64,
    status: u64,
    ram: Vec<u8>,
    /// The compiled side's exit kind; the interpreted side copies it.
    kind: u64,
}

fn main() -> anyhow::Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let mut trials = 20usize;
    let mut positional: Vec<String> = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--trials" => {
                trials = arguments[index + 1].parse()?;
                index += 1;
            }
            flag if flag.starts_with("--") => {}
            other => positional.push(other.to_string()),
        }
        index += 1;
    }
    let elf = std::fs::read(&positional[0])?;
    let only = positional
        .get(1)
        .map(|text| u64::from_str_radix(text.trim_start_matches("0x"), 16))
        .transpose()?;
    let candidates = zaqaru::tier1::sweep(&elf)?;
    let data = data_segments(&elf);
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut checked = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    if arguments.iter().any(|a| a == "--regions") {
        let regions = zaqaru::tier1::region::form(&candidates);
        for region in &regions {
            if region.members.len() < 2 { continue; }
            if let Some(only) = only && region.base() != only { continue; }
            let built = zaqaru::tier1::build(&region.members, usize::MAX, false);
            if built.functions == 0 { skipped += 1; continue; }
            checked += 1;
            let entry = region.members[0].clone();
            for trial in 0..trials {
                let start = Start::random(&mut rng);
                let compiled = run_compiled(&built.object, &entry, &start, &data)?;
                let quantum = compiled.retired + u64::from(compiled.kind == targum::tier1::KIND_INTERPRET);
                let interpreted = run_interpreted(&entry, &start, quantum, &data, compiled.kind);
                // Only a genuine divergence: the interpreter naturally runs
                // past a region into code this harness has not mapped and
                // faults early, which is not the compiler being wrong. Two
                // sides that retired the same and still differ is.
                if compiled.retired == interpreted.retired && compiled != interpreted {
                    failed += 1;
                    eprintln!("MISMATCH region {:#x} ({} members) trial {trial}", region.base(), region.members.len());
                    report(&start, &compiled, &interpreted);
                    if failed > 15 { println!("{checked} regions, {failed} mismatched (stop)"); return Ok(()); }
                    break;
                }
            }
        }
        println!("{checked} regions checked, {skipped} skipped, {failed} mismatched");
        return Ok(());
    }

    for candidate in &candidates {
        if let Some(only) = only
            && candidate.address != only
        {
            continue;
        }
        let built = zaqaru::tier1::build(std::slice::from_ref(candidate), usize::MAX, false);
        if built.functions == 0 {
            skipped += 1;
            continue;
        }
        checked += 1;
        for trial in 0..trials {
            let start = Start::random(&mut rng);
            let compiled = run_compiled(&built.object, candidate, &start, &data)?;
            // Everything the compiled side retired, and the instruction it
            // handed back if it did, which the interpreter attempts.
            let quantum = compiled.retired + u64::from(compiled.kind == targum::tier1::KIND_INTERPRET);
            let interpreted = run_interpreted(candidate, &start, quantum, &data, compiled.kind);
            if compiled != interpreted {
                failed += 1;
                eprintln!(
                    "MISMATCH block {:#x} ({} instructions, {} bytes) trial {trial}",
                    candidate.address,
                    candidate.instructions,
                    candidate.bytes.len()
                );
                report(&start, &compiled, &interpreted);
                break;
            }
        }
    }
    println!("{checked} blocks checked, {skipped} skipped (deferred), {failed} mismatched");
    Ok(())
}

fn report(start: &Start, compiled: &End, interpreted: &End) {
    let names = [
        "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12",
        "r13", "r14", "r15",
    ];
    eprintln!(
        "  flags in: {:?} {:?} left {:#x} right {:#x} result {:#x}",
        start.rule, start.width, start.left, start.right, start.result
    );
    for number in 0..16 {
        if compiled.registers[number] != interpreted.registers[number]
            || start.registers[number] != compiled.registers[number]
        {
            eprintln!(
                "  {:>3}: in {:#x}  compiled {:#x}  interpreted {:#x}{}",
                names[number],
                start.registers[number],
                compiled.registers[number],
                interpreted.registers[number],
                if compiled.registers[number] != interpreted.registers[number] { "  <--" } else { "" }
            );
        }
    }
    eprintln!(
        "  rip: compiled {:#x} interpreted {:#x}{}",
        compiled.rip,
        interpreted.rip,
        if compiled.rip != interpreted.rip { "  <--" } else { "" }
    );
    eprintln!(
        "  retired: compiled {} interpreted {}{}",
        compiled.retired,
        interpreted.retired,
        if compiled.retired != interpreted.retired { "  <--" } else { "" }
    );
    eprintln!(
        "  status: compiled {:#x} interpreted {:#x}{}",
        compiled.status,
        interpreted.status,
        if compiled.status != interpreted.status { "  <--" } else { "" }
    );
    if compiled.ram != interpreted.ram {
        let first = compiled.ram.iter().zip(&interpreted.ram).position(|(a, b)| a != b).unwrap();
        eprintln!(
            "  ram differs first at {:#x}: compiled {:#x} interpreted {:#x}",
            RAM + first as u64,
            compiled.ram[first],
            interpreted.ram[first]
        );
    }
}

fn run_compiled(object: &[u8], candidate: &zaqaru::tier1::Candidate, start: &Start, data: &[Data]) -> anyhow::Result<End> {
    let engine = Wasm::default();
    let module = Module::new(&engine, object)?;
    let mut store = Store::new(&engine, ());
    let memory = Memory::new(&mut store, MemoryType::new((LIMIT / 65536) as u32, None))?;
    let table = Table::new(&mut store, TableType::new(RefType::FUNCREF, 4, None), Ref::Func(None))?;
    let mut linker = Linker::new(&engine);
    linker.define(&mut store, "env", "__linear_memory", memory)?;
    linker.define(&mut store, "env", "__indirect_function_table", table)?;
    // The helper, for real: the machine and the RAM window are copied out
    // of the wasm memory, the interpreter's own `step` runs natively, and
    // both are copied back. Slow, and exactly what the engine does.
    let for_step = candidate.clone();
    let data_for_step: Vec<Data> = data.to_vec();
    linker.func_wrap("env", "targum_step", move |mut caller: wasmtime::Caller<'_, ()>, tcb_at: i32, address: i64| -> i32 {
        let mut image = vec![0u8; TCB_BYTES];
        memory.read(&caller, tcb_at as usize, &mut image).expect("read the tcb");
        let mut ram = vec![0u8; RAM_BYTES as usize];
        memory.read(&caller, RAM as usize, &mut ram).expect("read the ram");
        // The writable data too, both ways: a deferred instruction may
        // store into `.data`.
        let mut current: Vec<Data> = data_for_step.clone();
        for segment in current.iter_mut() {
            memory.read(&caller, segment.address as usize, &mut segment.bytes).expect("read data");
        }
        let (code, image, ram, current) = step_natively(&for_step, address as u64, &image, &ram, &current);
        memory.write(&mut caller, tcb_at as usize, &image).expect("write the tcb");
        memory.write(&mut caller, RAM as usize, &ram).expect("write the ram");
        for segment in &current {
            if segment.writable {
                memory.write(&mut caller, segment.address as usize, &segment.bytes).expect("write data");
            }
        }
        code
    })?;
    linker.func_wrap(
        "env",
        "targum_condition",
        move |caller: wasmtime::Caller<'_, ()>, tcb: i32, condition: i32| -> i32 {
            let mut bytes = [0u8; 48];
            memory.read(&caller, tcb as usize + layout::FLAGS as usize, &mut bytes).expect("read");
            // SAFETY: `Flags` is `repr(C)` plain data at exactly this layout.
            let flags: Flags = unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const Flags) };
            let condition = targum::flags::Condition::from_code(condition as u8).expect("condition");
            i32::from(condition.holds(&flags))
        },
    )?;
    linker.func_wrap("env", "targum_code_write", |_address: i64, _length: i32| {})?;
    let _instance = linker.instantiate(&mut store, &module)?;

    // The control block.
    let tcb = start.tcb(candidate.address);
    let mut block = vec![0u8; TCB_BYTES];
    // SAFETY: the control block is plain data of a fixed layout.
    unsafe {
        std::ptr::copy_nonoverlapping(&tcb as *const Tcb as *const u8, block.as_mut_ptr(), TCB_BYTES);
    }
    memory.write(&mut store, TCB as usize, &block)?;
    // The vitals and bitmaps: the RAM window readable and writable, and
    // nothing else, one word each.
    let mut vitals = [0u8; 32];
    let put = |into: &mut [u8], at: usize, value: u32| into[at..at + 4].copy_from_slice(&value.to_le_bytes());
    put(&mut vitals, 0, BITMAPS);
    put(&mut vitals, 4, BITMAP_WORDS);
    put(&mut vitals, 8, BITMAPS + BITMAP_WORDS * 8);
    put(&mut vitals, 12, BITMAP_WORDS);
    put(&mut vitals, 16, BITMAPS + 2 * BITMAP_WORDS * 8);
    put(&mut vitals, 20, BITMAP_WORDS);
    vitals[24..32].copy_from_slice(&LIMIT.to_le_bytes());
    memory.write(&mut store, VITALS as usize, &vitals)?;
    let mut readable = vec![0u64; BITMAP_WORDS as usize];
    let mut writable = vec![0u64; BITMAP_WORDS as usize];
    let mut set = |map: &mut Vec<u64>, pages: std::ops::Range<u64>| {
        for page in pages {
            map[(page / 64) as usize] |= 1 << (page % 64);
        }
    };
    set(&mut readable, page_range(RAM, RAM_BYTES));
    set(&mut writable, page_range(RAM, RAM_BYTES));
    for segment in data {
        set(&mut readable, page_range(segment.address, segment.bytes.len() as u64));
        if segment.writable {
            set(&mut writable, page_range(segment.address, segment.bytes.len() as u64));
        }
        memory.write(&mut store, segment.address as usize, &segment.bytes)?;
    }
    let mut bitmaps = Vec::new();
    for word in readable.iter().chain(&writable) {
        bitmaps.extend_from_slice(&word.to_le_bytes());
    }
    bitmaps.resize(3 * BITMAP_WORDS as usize * 8, 0);
    memory.write(&mut store, BITMAPS as usize, &bitmaps)?;
    memory.write(&mut store, RAM as usize, &start.ram)?;

    let Some(Ref::Func(Some(function))) = table.get(&mut store, 1) else {
        anyhow::bail!("no function in slot 1")
    };
    let typed = function.typed::<(i32, i32, i64, i64, i32), i64>(&store)?;
    let exit = typed.call(&mut store, (TCB as i32, VITALS as i32, candidate.address as i64, 1000, 0))?;
    let mut after = vec![0u8; TCB_BYTES];
    memory.read(&store, TCB as usize, &mut after)?;
    let mut registers = [0u64; 16];
    for (number, register) in registers.iter_mut().enumerate() {
        *register = u64::from_le_bytes(after[number * 8..number * 8 + 8].try_into().unwrap());
    }
    let long = |at: u32| u64::from_le_bytes(after[at as usize..at as usize + 8].try_into().unwrap());
    // SAFETY: as above.
    let flags: Flags = unsafe {
        std::ptr::read_unaligned(after[layout::FLAGS as usize..].as_ptr() as *const Flags)
    };
    let mut ram = vec![0u8; RAM_BYTES as usize];
    memory.read(&store, RAM as usize, &mut ram)?;
    Ok(End {
        registers,
        rip: long(layout::RIP),
        retired: long(layout::RETIRED),
        status: flags.status(),
        ram,
        kind: targum::tier1::exit_kind(exit as u64),
    })
}

/// The native world: the code page, the RAM window, and the data segments,
/// at their addresses. The arenas are kept alive with the space.
fn native_world(candidate: &zaqaru::tier1::Candidate, ram: &[u8], data: &[Data]) -> (Space, Vec<targum::arena::Arena>) {
    let code_page = candidate.address & !0xfff;
    let code_len = ((candidate.address + candidate.bytes.len() as u64 + 0xfff) & !0xfff) - code_page;
    let mut arenas = vec![
        targum::arena::Arena::at(code_page, code_len),
        targum::arena::Arena::at(RAM, RAM_BYTES),
    ];
    let mut space = Space::new(LIMIT.max(code_page + code_len));
    space.protect(code_page, code_len, Protection::ALL);
    space.protect(RAM, RAM_BYTES, Protection::READ_WRITE);
    space.write(candidate.address, &candidate.bytes).expect("place the code");
    space.protect(code_page, code_len, Protection { read: true, write: false, execute: true });
    space.write(RAM, ram).expect("place the ram");
    for segment in data {
        let page = segment.address & !0xfff;
        let len = ((segment.address + segment.bytes.len() as u64 + 0xfff) & !0xfff) - page;
        if page < code_page + code_len && code_page < page + len {
            continue; // overlaps the code page; the code arena already covers it
        }
        arenas.push(targum::arena::Arena::at(page, len));
        space.protect(page, len, Protection::ALL);
        space.write(segment.address, &segment.bytes).expect("place data");
        let prot = Protection { read: true, write: segment.writable, execute: segment.executable };
        if !segment.writable || segment.executable {
            space.protect(page, len, prot);
        }
    }
    (space, arenas)
}

fn run_interpreted(candidate: &zaqaru::tier1::Candidate, start: &Start, quantum: u64, data: &[Data], kind: u64) -> End {
    let (mut space, _arenas) = native_world(candidate, &start.ram, data);
    let mut cache = BlockCache::new();
    cache.drain_invalidations(&mut space);
    let mut tcb = start.tcb(candidate.address);
    // Exactly as many instructions as the compiled side retired, plus the
    // one it handed back: the compiled block stops at its own exit, and the
    // interpreter would carry on into whatever lies at the target.
    if quantum > 0 {
        let _ = Engine::run(&mut tcb, &mut space, &mut cache, quantum);
    }
    let mut ram = vec![0u8; RAM_BYTES as usize];
    space.read(RAM, &mut ram).expect("read the ram");
    End {
        registers: tcb.registers,
        rip: tcb.rip,
        retired: tcb.retired,
        status: tcb.flags.status(),
        ram,
        kind,
    }
}
