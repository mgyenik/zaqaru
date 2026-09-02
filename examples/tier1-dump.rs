//! Prints the wasm text of the compiled block at an address in an ELF, and
//! with `--run`, executes it alone under wasmtime against a stub control
//! block, which is the compiler's own bench.
use wasmtime::{Engine, Instance, Linker, Memory, MemoryType, Module, Ref, RefType, Store, Table, TableType};

fn main() -> anyhow::Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let run = arguments.iter().any(|a| a == "--run");
    let positional: Vec<&String> = arguments.iter().filter(|a| !a.starts_with("--")).collect();
    let elf = std::fs::read(positional[0])?;
    let address = u64::from_str_radix(positional[1].trim_start_matches("0x"), 16)?;
    let candidates = zaqaru::tier1::sweep(&elf)?;
    let candidate = candidates.iter().find(|c| c.address == address).expect("a block at that address");
    eprintln!("block at {:#x}: {} bytes, {} instructions", candidate.address, candidate.bytes.len(), candidate.instructions);
    let built = zaqaru::tier1::build(std::slice::from_ref(candidate), usize::MAX);
    if !run {
        println!("{}", wasmprinter::print_bytes(&built.object)?);
        return Ok(());
    }

    let engine = Engine::default();
    let module = Module::new(&engine, &built.object)?;
    let mut store = Store::new(&engine, ());
    let memory = Memory::new(&mut store, MemoryType::new(4, None))?;
    let table = Table::new(&mut store, TableType::new(RefType::FUNCREF, 4, None), Ref::Func(None))?;
    let mut linker = Linker::new(&engine);
    linker.define(&mut store, "env", "__linear_memory", memory)?;
    linker.define(&mut store, "env", "__indirect_function_table", table)?;
    linker.func_wrap("env", "targum_step", |_tcb: i32, position: i32| -> i32 {
        eprintln!("  helper: step at position {position}");
        0
    })?;
    linker.func_wrap("env", "targum_condition", |_tcb: i32, condition: i32| -> i32 {
        eprintln!("  helper: condition {condition}");
        0
    })?;
    linker.func_wrap("env", "targum_code_write", |address: i64, length: i32| {
        eprintln!("  helper: code write at {address:#x} length {length}");
    })?;
    let _instance: Instance = linker.instantiate(&mut store, &module)?;

    // The stub world: a control block at 0x1000, vitals at 0x2000, bitmaps
    // at 0x3000, a stack at 0x10000, all of the four pages mapped.
    const TCB: u32 = 0x1000;
    const VITALS: u32 = 0x2000;
    let mut bytes = vec![0u8; 0x4000];
    let put64 = |bytes: &mut [u8], at: u32, value: u64| bytes[at as usize..at as usize + 8].copy_from_slice(&value.to_le_bytes());
    let put32 = |bytes: &mut [u8], at: u32, value: u32| bytes[at as usize..at as usize + 4].copy_from_slice(&value.to_le_bytes());
    put64(&mut bytes, TCB + 4 * 8, 0x10000); // rsp
    put64(&mut bytes, TCB + 2 * 8, 0x1234); // rdx
    put64(&mut bytes, TCB + 0 * 8, 0xaaaa); // rax
    put32(&mut bytes, VITALS, 0x3000);
    put32(&mut bytes, VITALS + 4, 1);
    put32(&mut bytes, VITALS + 8, 0x3100);
    put32(&mut bytes, VITALS + 12, 1);
    put32(&mut bytes, VITALS + 16, 0x3200);
    put32(&mut bytes, VITALS + 20, 1);
    put64(&mut bytes, VITALS + 24, 0x40000);
    put64(&mut bytes, 0x3000, 0xffff_ffff_ffff_ffff);
    put64(&mut bytes, 0x3100, 0xffff_ffff_ffff_ffff);
    memory.write(&mut store, 0, &bytes)?;
    memory.write(&mut store, 0x10000, &0x5555u64.to_le_bytes())?; // what `pop rsi` finds

    let Some(Ref::Func(Some(function))) = table.get(&mut store, 1) else { anyhow::bail!("no function in slot 1") };
    let typed = function.typed::<(i32, i32, i64, i64), i64>(&store)?;
    let exit = typed.call(&mut store, (TCB as i32, VITALS as i32, address as i64, 1000))?;
    let mut after = vec![0u8; 0x200];
    memory.read(&store, TCB as usize, &mut after)?;
    let get64 = |at: u32| u64::from_le_bytes(after[at as usize..at as usize + 8].try_into().unwrap());
    println!("exit kind {} rip {:#x}", (exit as u64) >> 32, exit as u64 & 0xffff_ffff);
    println!("retired {}  rip {:#x}", get64(136), get64(128));
    for (name, number) in [("rax", 0), ("rcx", 1), ("rdx", 2), ("rsp", 4), ("rbp", 5), ("rsi", 6), ("rdi", 7), ("r8", 8), ("r9", 9)] {
        println!("  {name} = {:#x}", get64(number * 8));
    }
    let rsp = get64(32);
    let mut top = [0u8; 8];
    memory.read(&store, rsp as usize, &mut top)?;
    println!("  [rsp] = {:#x}", u64::from_le_bytes(top));
    Ok(())
}
