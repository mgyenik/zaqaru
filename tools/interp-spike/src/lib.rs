//! Interpreter-throughput spike: how many x86-64 instructions per second can
//! a straightforward interpreter retire, natively and inside wasm?
//!
//! Two variants price the two ends of tier 0:
//! - `run_decode`: fetch + iced-decode every instruction, every time — the
//!   pessimistic floor, no cache at all.
//! - `run_cached`: instructions decoded once into a flat cache (what a basic
//!   block cache's hit path looks like), executed by the same generic
//!   semantics loop.
//!
//! The guest is a 7-instruction hot loop mixing ALU, a load, a store, a flag
//! write, and a conditional branch. Iteration count arrives in `rcx`, the
//! way an interpreter would preset a register file. Guest memory is a 64 KiB
//! arena addressed through a mask — the stand-in for the real design, where
//! the guest address space is linear memory and bounds come free.
//!
//! Each retired instruction also bumps a counter, deliberately: the real
//! loop pays one for preemption.

use iced_x86::code_asm::*;
use iced_x86::{Decoder, DecoderOptions, Instruction, Mnemonic, OpKind, Register};

const BASE: u64 = 0x1000;
const MEM_MASK: u64 = 0xFFFF;

pub struct Machine {
    regs: [u64; 16],
    rip: u64,
    zf: bool,
    sf: bool,
    cf: bool,
    memory: Vec<u8>,
    pub retired: u64,
}

impl Machine {
    fn new(iterations: u64) -> Self {
        let mut machine = Machine {
            regs: [0; 16],
            rip: BASE,
            zf: false,
            sf: false,
            cf: false,
            memory: vec![0; (MEM_MASK + 1) as usize],
            retired: 0,
        };
        machine.regs[1] = iterations; // rcx
        machine
    }

    fn reg(&self, register: Register) -> u64 {
        self.regs[register as usize - Register::RAX as usize]
    }

    fn set_reg(&mut self, register: Register, value: u64) {
        self.regs[register as usize - Register::RAX as usize] = value;
    }

    fn load(&self, address: u64) -> u64 {
        let at = (address & MEM_MASK) as usize;
        u64::from_le_bytes(self.memory[at..at + 8].try_into().unwrap())
    }

    fn store(&mut self, address: u64, value: u64) {
        let at = (address & MEM_MASK) as usize;
        self.memory[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn effective_address(&self, instruction: &Instruction) -> u64 {
        let mut address = instruction.memory_displacement64();
        let base = instruction.memory_base();
        if base != Register::None {
            address = address.wrapping_add(self.reg(base));
        }
        let index = instruction.memory_index();
        if index != Register::None {
            address = address
                .wrapping_add(self.reg(index).wrapping_mul(instruction.memory_index_scale() as u64));
        }
        address
    }

    fn read_operand(&self, instruction: &Instruction, operand: u32) -> u64 {
        match instruction.op_kind(operand) {
            OpKind::Register => self.reg(instruction.op_register(operand)),
            OpKind::Immediate8to64 | OpKind::Immediate32to64 | OpKind::Immediate64 => {
                instruction.immediate(operand)
            }
            OpKind::Immediate32 => u64::from(instruction.immediate32()),
            OpKind::Memory => self.load(self.effective_address(instruction)),
            other => panic!("operand kind {other:?} not in the spike"),
        }
    }

    fn write_operand(&mut self, instruction: &Instruction, operand: u32, value: u64) {
        match instruction.op_kind(operand) {
            OpKind::Register => self.set_reg(instruction.op_register(operand), value),
            OpKind::Memory => self.store(self.effective_address(instruction), value),
            other => panic!("destination kind {other:?} not in the spike"),
        }
    }

    fn flags(&mut self, result: u64, carry: bool) {
        self.zf = result == 0;
        self.sf = (result as i64) < 0;
        self.cf = carry;
    }

    /// Executes one decoded instruction; answers false when the guest `ret`s.
    fn step(&mut self, instruction: &Instruction) -> bool {
        self.retired += 1;
        let mut next = instruction.next_ip();
        match instruction.mnemonic() {
            Mnemonic::Mov => {
                let value = self.read_operand(instruction, 1);
                self.write_operand(instruction, 0, value);
            }
            Mnemonic::Add => {
                let left = self.read_operand(instruction, 0);
                let right = self.read_operand(instruction, 1);
                let (result, carry) = left.overflowing_add(right);
                self.flags(result, carry);
                self.write_operand(instruction, 0, result);
            }
            Mnemonic::Sub | Mnemonic::Cmp => {
                let left = self.read_operand(instruction, 0);
                let right = self.read_operand(instruction, 1);
                let (result, borrow) = left.overflowing_sub(right);
                self.flags(result, borrow);
                if instruction.mnemonic() == Mnemonic::Sub {
                    self.write_operand(instruction, 0, result);
                }
            }
            Mnemonic::Xor => {
                let result = self.read_operand(instruction, 0) ^ self.read_operand(instruction, 1);
                self.flags(result, false);
                self.write_operand(instruction, 0, result);
            }
            Mnemonic::Dec => {
                let result = self.read_operand(instruction, 0).wrapping_sub(1);
                let carry = self.cf; // dec preserves CF
                self.flags(result, carry);
                self.write_operand(instruction, 0, result);
            }
            Mnemonic::Jne => {
                if !self.zf {
                    next = instruction.near_branch64();
                }
            }
            Mnemonic::Ret => return false,
            other => panic!("mnemonic {other:?} not in the spike"),
        }
        self.rip = next;
        true
    }
}

/// The guest: 3 setup instructions, then a 7-instruction loop, then `ret`.
pub fn guest() -> Vec<u8> {
    let mut assembler = CodeAssembler::new(64).expect("assembler");
    let mut top = assembler.create_label();
    assembler.mov(rax, 0u64).unwrap();
    assembler.mov(rbx, 0x2000u64).unwrap();
    assembler.mov(rdx, 0u64).unwrap();
    assembler.set_label(&mut top).unwrap();
    assembler.add(rax, rcx).unwrap();
    assembler.xor(rax, 0x55i32).unwrap();
    assembler.mov(qword_ptr(rbx), rax).unwrap();
    assembler.mov(rdx, qword_ptr(rbx)).unwrap();
    assembler.add(rax, rdx).unwrap();
    assembler.dec(rcx).unwrap();
    assembler.jne(top).unwrap();
    assembler.ret().unwrap();
    assembler.assemble(BASE).expect("assemble")
}

/// Variant A: fetch and decode every instruction, every time.
#[no_mangle]
pub extern "C" fn run_decode(iterations: u64) -> u64 {
    let code = guest();
    let mut machine = Machine::new(iterations);
    let mut decoder = Decoder::with_ip(64, &code, BASE, DecoderOptions::NONE);
    loop {
        decoder
            .set_position((machine.rip - BASE) as usize)
            .expect("rip inside the guest");
        decoder.set_ip(machine.rip);
        let instruction = decoder.decode();
        if !machine.step(&instruction) {
            break;
        }
    }
    CHECK.with(|check| check.set(machine.regs[0]));
    machine.retired
}

/// Variant B: decode once into a flat cache, execute from it — a block
/// cache's hit path.
#[no_mangle]
pub extern "C" fn run_cached(iterations: u64) -> u64 {
    let code = guest();
    let mut cache: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
    let mut instructions: Vec<Instruction> = Vec::new();
    let mut decoder = Decoder::with_ip(64, &code, BASE, DecoderOptions::NONE);
    while decoder.can_decode() {
        cache.insert(decoder.ip(), instructions.len());
        instructions.push(decoder.decode());
    }
    let mut machine = Machine::new(iterations);
    let mut index = 0usize;
    loop {
        let instruction = &instructions[index];
        let sequential = instruction.next_ip();
        if !machine.step(instruction) {
            break;
        }
        // Fall through cheaply; pay the map only on a taken branch.
        index = if machine.rip == sequential {
            index + 1
        } else {
            cache[&machine.rip]
        };
    }
    CHECK.with(|check| check.set(machine.regs[0]));
    machine.retired
}

thread_local! {
    static CHECK: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The final `rax`, so native and wasm runs can be compared exactly.
#[no_mangle]
pub extern "C" fn checksum() -> u64 {
    CHECK.with(|check| check.get())
}
