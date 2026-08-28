//! The host-FPU oracle: the machine under the test suite has a real x87,
//! so the emulation is held to bit-identical results and exception flags
//! against hardware, across random operands, all four rounding modes and
//! all three precision-control settings.
//!
//! The f64-backed transcendental tier compares in ulps instead — Intel and
//! AMD hardware disagree in those ops' low bits, so bit-matching is not a
//! coherent target there; the divergence is measured, not assumed.
//!
//! Each #[test] runs on its own thread, and the OS context-switches the
//! FPU per thread, so the parallel harness is safe.

#![cfg(target_arch = "x86_64")]

use std::arch::asm;

use x87::compare::{self, NanPolicy};
use x87::convert;
use x87::f80::{BIAS, Class, F80};
use x87::ops::Binary;
use x87::{Precision, Rounding, arith, flags, transcendental};

/// Exception flags plus C1 — what every arithmetic comparison checks.
const ARITHMETIC_MASK: u16 = 0x3F | flags::STACK_FAULT | flags::C1;

// Sized so the whole oracle stays around a second (measured 0.53s serial
// at 60k/40k on 2026-08-28); raise only with a fresh measurement.
const BINARY_CASES: usize = 150_000;
const UNARY_CASES: usize = 100_000;

// --- deterministic generation ---

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        // xorshift64*: cheap, seedable, good enough to fill significands.
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn value(&mut self) -> F80 {
        let sign = self.next() & 1 != 0;
        match self.below(100) {
            // Normals, biased toward the middle of the range where
            // arithmetic actually lands, with tails at the extremes.
            0..=49 => {
                let exponent = match self.below(10) {
                    0..=5 => (BIAS as u64 + self.below(129)) as u16 - 64,
                    6..=7 => 1 + self.below(0x7FFE) as u16,
                    8 => 1 + self.below(64) as u16,
                    _ => 0x7FFE - self.below(64) as u16,
                };
                F80::new(sign, exponent, (1 << 63) | self.next())
            }
            // Small integers and simple dyadics: where carries, exact
            // results and ties live.
            50..=59 => {
                let small = convert::from_i64(self.below(64) as i64 - 32);
                if sign { small.negate() } else { small }
            }
            // Denormals, with a sliver of pseudo-denormals.
            60..=69 => F80::new(sign, 0, self.next() | 1),
            70..=77 => F80::new(sign, 0, 0),
            78..=85 => F80::new(sign, 0x7FFF, 1 << 63),
            // NaNs, quiet and signalling, random payloads.
            86..=95 => {
                let payload = (1 << 63) | self.next() | 1;
                F80::new(sign, 0x7FFF, payload)
            }
            // The 8087's leftovers: unnormals and pseudo-NaN/infinity.
            _ => {
                let exponent = if self.next() & 1 != 0 {
                    0x7FFF
                } else {
                    1 + self.below(0x7FFE) as u16
                };
                F80::new(sign, exponent, self.next() >> 1)
            }
        }
    }

    fn control(&mut self) -> u16 {
        let rounding = self.below(4) as u16;
        // Skip the reserved PC encoding 01.
        let precision = [0b00u16, 0b10, 0b11][self.below(3) as usize];
        0x003F | (precision << 8) | (rounding << 10)
    }
}

fn modes(control: u16) -> (Rounding, Precision) {
    (
        Rounding::from_control(control),
        Precision::from_control(control),
    )
}

// --- hardware wrappers ---

macro_rules! hw_binary {
    ($name:ident, $insn:literal) => {
        fn $name(a: [u8; 10], b: [u8; 10], control: u16) -> ([u8; 10], u16) {
            let mut out = [0u8; 10];
            let mut status: u16 = 0;
            unsafe {
                asm!(
                    "fninit",
                    "fldcw [{control}]",
                    "fld tbyte ptr [{b}]",
                    "fld tbyte ptr [{a}]",
                    $insn,
                    "fnstsw [{status}]",
                    "fstp tbyte ptr [{out}]",
                    "fninit",
                    a = in(reg) a.as_ptr(),
                    b = in(reg) b.as_ptr(),
                    control = in(reg) &control,
                    status = in(reg) &mut status,
                    out = in(reg) out.as_mut_ptr(),
                );
            }
            (out, status)
        }
    };
}

hw_binary!(hw_add, "faddp st(1), st(0)");
hw_binary!(hw_sub, "fsubrp st(1), st(0)"); // st1 ← st0 − st1 = a − b
hw_binary!(hw_mul, "fmulp st(1), st(0)");
hw_binary!(hw_div, "fdivrp st(1), st(0)"); // st1 ← st0 ÷ st1 = a ÷ b
hw_binary!(hw_prem, "fprem");
hw_binary!(hw_prem1, "fprem1");
hw_binary!(hw_scale, "fscale");

macro_rules! hw_unary {
    ($name:ident, $insn:literal) => {
        fn $name(a: [u8; 10], control: u16) -> ([u8; 10], u16) {
            let mut out = [0u8; 10];
            let mut status: u16 = 0;
            unsafe {
                asm!(
                    "fninit",
                    "fldcw [{control}]",
                    "fld tbyte ptr [{a}]",
                    $insn,
                    "fnstsw [{status}]",
                    "fstp tbyte ptr [{out}]",
                    "fninit",
                    a = in(reg) a.as_ptr(),
                    control = in(reg) &control,
                    status = in(reg) &mut status,
                    out = in(reg) out.as_mut_ptr(),
                );
            }
            (out, status)
        }
    };
}

hw_unary!(hw_sqrt, "fsqrt");
hw_unary!(hw_rndint, "frndint");
hw_unary!(hw_abs, "fabs");
hw_unary!(hw_chs, "fchs");
hw_unary!(hw_f2xm1, "f2xm1");

hw_binary!(hw_fyl2x_raw, "fyl2x");
hw_binary!(hw_fpatan_raw, "fpatan");

fn hw_from_f64(bits: u64, control: u16) -> ([u8; 10], u16) {
    let mut out = [0u8; 10];
    let mut status: u16 = 0;
    unsafe {
        asm!(
            "fninit",
            "fldcw [{control}]",
            "fld qword ptr [{a}]",
            "fnstsw [{status}]",
            "fstp tbyte ptr [{out}]",
            "fninit",
            a = in(reg) &bits,
            control = in(reg) &control,
            status = in(reg) &mut status,
            out = in(reg) out.as_mut_ptr(),
        );
    }
    (out, status)
}

fn hw_to_f64(a: [u8; 10], control: u16) -> (u64, u16) {
    let mut out: u64 = 0;
    let mut status: u16 = 0;
    unsafe {
        asm!(
            "fninit",
            "fldcw [{control}]",
            "fld tbyte ptr [{a}]",
            "fstp qword ptr [{out}]",
            "fnstsw [{status}]",
            "fninit",
            a = in(reg) a.as_ptr(),
            control = in(reg) &control,
            status = in(reg) &mut status,
            out = in(reg) &mut out,
        );
    }
    (out, status)
}

fn hw_to_f32(a: [u8; 10], control: u16) -> (u32, u16) {
    let mut out: u32 = 0;
    let mut status: u16 = 0;
    unsafe {
        asm!(
            "fninit",
            "fldcw [{control}]",
            "fld tbyte ptr [{a}]",
            "fstp dword ptr [{out}]",
            "fnstsw [{status}]",
            "fninit",
            a = in(reg) a.as_ptr(),
            control = in(reg) &control,
            status = in(reg) &mut status,
            out = in(reg) &mut out,
        );
    }
    (out, status)
}

macro_rules! hw_to_int {
    ($name:ident, $ty:ty, $insn:literal) => {
        fn $name(a: [u8; 10], control: u16) -> ($ty, u16) {
            let mut out: $ty = 0;
            let mut status: u16 = 0;
            unsafe {
                asm!(
                    "fninit",
                    "fldcw [{control}]",
                    "fld tbyte ptr [{a}]",
                    $insn,
                    "fnstsw [{status}]",
                    "fninit",
                    a = in(reg) a.as_ptr(),
                    control = in(reg) &control,
                    status = in(reg) &mut status,
                    out = in(reg) &mut out,
                );
            }
            (out, status)
        }
    };
}

hw_to_int!(hw_to_i16, i16, "fistp word ptr [{out}]");
hw_to_int!(hw_to_i32, i32, "fistp dword ptr [{out}]");
hw_to_int!(hw_to_i64, i64, "fistp qword ptr [{out}]");

/// `fucomi`/`fcomi`: the relation lands in EFLAGS.
fn hw_comi(a: [u8; 10], b: [u8; 10], quiet: bool) -> (u32, u16) {
    let mut eflags: u64;
    let mut status: u16 = 0;
    unsafe {
        if quiet {
            asm!(
                "fninit",
                "fld tbyte ptr [{b}]",
                "fld tbyte ptr [{a}]",
                "fucomi st(0), st(1)",
                "fnstsw [{status}]",
                "pushfq",
                "pop {eflags}",
                "fninit",
                a = in(reg) a.as_ptr(),
                b = in(reg) b.as_ptr(),
                status = in(reg) &mut status,
                eflags = out(reg) eflags,
            );
        } else {
            asm!(
                "fninit",
                "fld tbyte ptr [{b}]",
                "fld tbyte ptr [{a}]",
                "fcomi st(0), st(1)",
                "fnstsw [{status}]",
                "pushfq",
                "pop {eflags}",
                "fninit",
                a = in(reg) a.as_ptr(),
                b = in(reg) b.as_ptr(),
                status = in(reg) &mut status,
                eflags = out(reg) eflags,
            );
        }
    }
    (eflags as u32 & 0x45, status)
}

/// `fucom`/`fcom`: the relation lands in C3/C2/C0.
fn hw_com(a: [u8; 10], b: [u8; 10], quiet: bool) -> u16 {
    let mut status: u16 = 0;
    unsafe {
        if quiet {
            asm!(
                "fninit",
                "fld tbyte ptr [{b}]",
                "fld tbyte ptr [{a}]",
                "fucom st(1)",
                "fnstsw [{status}]",
                "fninit",
                a = in(reg) a.as_ptr(),
                b = in(reg) b.as_ptr(),
                status = in(reg) &mut status,
            );
        } else {
            asm!(
                "fninit",
                "fld tbyte ptr [{b}]",
                "fld tbyte ptr [{a}]",
                "fcom st(1)",
                "fnstsw [{status}]",
                "fninit",
                a = in(reg) a.as_ptr(),
                b = in(reg) b.as_ptr(),
                status = in(reg) &mut status,
            );
        }
    }
    status
}

fn hw_fxam(a: [u8; 10]) -> u16 {
    let mut status: u16 = 0;
    unsafe {
        asm!(
            "fninit",
            "fld tbyte ptr [{a}]",
            "fxam",
            "fnstsw [{status}]",
            "fninit",
            a = in(reg) a.as_ptr(),
            status = in(reg) &mut status,
        );
    }
    status
}

fn hw_fxtract(a: [u8; 10], control: u16) -> ([u8; 10], [u8; 10], u16) {
    let mut mantissa = [0u8; 10];
    let mut exponent = [0u8; 10];
    let mut status: u16 = 0;
    unsafe {
        asm!(
            "fninit",
            "fldcw [{control}]",
            "fld tbyte ptr [{a}]",
            "fxtract",
            "fnstsw [{status}]",
            "fstp tbyte ptr [{mantissa}]",
            "fstp tbyte ptr [{exponent}]",
            "fninit",
            a = in(reg) a.as_ptr(),
            control = in(reg) &control,
            status = in(reg) &mut status,
            mantissa = in(reg) mantissa.as_mut_ptr(),
            exponent = in(reg) exponent.as_mut_ptr(),
        );
    }
    (mantissa, exponent, status)
}

fn hw_constant(index: u32, control: u16) -> [u8; 10] {
    let mut out = [0u8; 10];
    unsafe {
        match index {
            0 => asm!("fninit", "fldcw [{c}]", "fld1", "fstp tbyte ptr [{o}]", "fninit", c = in(reg) &control, o = in(reg) out.as_mut_ptr()),
            1 => asm!("fninit", "fldcw [{c}]", "fldl2t", "fstp tbyte ptr [{o}]", "fninit", c = in(reg) &control, o = in(reg) out.as_mut_ptr()),
            2 => asm!("fninit", "fldcw [{c}]", "fldl2e", "fstp tbyte ptr [{o}]", "fninit", c = in(reg) &control, o = in(reg) out.as_mut_ptr()),
            3 => asm!("fninit", "fldcw [{c}]", "fldpi", "fstp tbyte ptr [{o}]", "fninit", c = in(reg) &control, o = in(reg) out.as_mut_ptr()),
            4 => asm!("fninit", "fldcw [{c}]", "fldlg2", "fstp tbyte ptr [{o}]", "fninit", c = in(reg) &control, o = in(reg) out.as_mut_ptr()),
            5 => asm!("fninit", "fldcw [{c}]", "fldln2", "fstp tbyte ptr [{o}]", "fninit", c = in(reg) &control, o = in(reg) out.as_mut_ptr()),
            _ => asm!("fninit", "fldcw [{c}]", "fldz", "fstp tbyte ptr [{o}]", "fninit", c = in(reg) &control, o = in(reg) out.as_mut_ptr()),
        }
    }
    out
}

// --- the comparisons ---

fn describe(a: F80) -> String {
    format!(
        "{:04x}:{:016x} ({:?})",
        a.sign_exponent,
        a.significand,
        a.classify()
    )
}

#[track_caller]
fn assert_bit_exact(
    label: &str,
    a: F80,
    b: Option<F80>,
    control: u16,
    hardware: ([u8; 10], u16),
    emulated: arith::Outcome,
) {
    let hw_value = F80::from_bytes(hardware.0);
    let hw_flags = hardware.1 & ARITHMETIC_MASK;
    let em_flags = emulated.flags & ARITHMETIC_MASK;
    assert!(
        hw_value == emulated.value && hw_flags == em_flags,
        "{label}: a={} b={} cw={control:04x}\n  hardware {} flags {hw_flags:04x}\n  emulated {} flags {em_flags:04x}",
        describe(a),
        b.map(describe).unwrap_or_default(),
        describe(hw_value),
        describe(emulated.value),
    );
}

#[test]
fn binary_arithmetic_is_bit_exact() {
    let mut rng = Rng::new(0x5EED_0001);
    let ops: [(&str, Binary, fn([u8; 10], [u8; 10], u16) -> ([u8; 10], u16)); 4] = [
        ("fadd", Binary::Add, hw_add),
        ("fsub", Binary::Sub, hw_sub),
        ("fmul", Binary::Mul, hw_mul),
        ("fdiv", Binary::Div, hw_div),
    ];
    for _ in 0..BINARY_CASES {
        let a = rng.value();
        let b = rng.value();
        let control = rng.control();
        let (rounding, precision) = modes(control);
        for (label, op, hardware) in ops {
            let hw = hardware(a.to_bytes(), b.to_bytes(), control);
            let em = match op {
                Binary::Add => arith::add(a, b, rounding, precision),
                Binary::Sub => arith::sub(a, b, rounding, precision),
                Binary::Mul => arith::mul(a, b, rounding, precision),
                _ => arith::div(a, b, rounding, precision),
            };
            assert_bit_exact(label, a, Some(b), control, hw, em);
        }
    }
}

#[test]
fn square_root_and_rounding_are_bit_exact() {
    let mut rng = Rng::new(0x5EED_0002);
    for _ in 0..UNARY_CASES {
        let a = rng.value();
        let control = rng.control();
        let (rounding, precision) = modes(control);
        let hw = hw_sqrt(a.to_bytes(), control);
        assert_bit_exact("fsqrt", a, None, control, hw, arith::sqrt(a, rounding, precision));
        let hw = hw_rndint(a.to_bytes(), control);
        assert_bit_exact(
            "frndint",
            a,
            None,
            control,
            hw,
            arith::round_to_int(a, rounding),
        );
    }
}

#[test]
fn sign_operations_are_bit_exact() {
    let mut rng = Rng::new(0x5EED_0003);
    for _ in 0..UNARY_CASES {
        let a = rng.value();
        let control = rng.control();
        // fchs/fabs are pure sign surgery: any pattern, no flags.
        let (value, status) = hw_chs(a.to_bytes(), control);
        assert_eq!(F80::from_bytes(value), a.negate(), "fchs {}", describe(a));
        assert_eq!(status & ARITHMETIC_MASK, 0);
        let (value, status) = hw_abs(a.to_bytes(), control);
        assert_eq!(F80::from_bytes(value), a.abs(), "fabs {}", describe(a));
        assert_eq!(status & ARITHMETIC_MASK, 0);
    }
}

#[test]
fn partial_remainder_is_bit_exact_with_its_protocol() {
    let mut rng = Rng::new(0x5EED_0004);
    let complete_mask = ARITHMETIC_MASK | flags::CONDITIONS;
    let partial_mask = 0x3F | flags::STACK_FAULT | flags::C2;
    for _ in 0..BINARY_CASES {
        let a = rng.value();
        let b = rng.value();
        // RC/PC are irrelevant to the exact remainder; still vary them to
        // prove that.
        let control = rng.control();
        for (label, nearest, hardware) in [
            ("fprem", false, hw_prem as fn(_, _, _) -> _),
            ("fprem1", true, hw_prem1),
        ] {
            let (hw_value, hw_status) = hardware(a.to_bytes(), b.to_bytes(), control);
            let em = arith::partial_remainder(a, b, nearest);
            // C0/C1/C3 are architecturally undefined while C2 says
            // incomplete, so the mask narrows there.
            let mask = if hw_status & flags::C2 != 0 || em.flags & flags::C2 != 0 {
                partial_mask
            } else {
                complete_mask
            };
            assert!(
                F80::from_bytes(hw_value) == em.value
                    && hw_status & mask == em.flags & mask,
                "{label}: a={} b={} cw={control:04x}\n  hardware {} status {:04x}\n  emulated {} flags {:04x}",
                describe(a),
                describe(b),
                describe(F80::from_bytes(hw_value)),
                hw_status & mask,
                describe(em.value),
                em.flags & mask,
            );
        }
    }
}

#[test]
fn scale_and_extract_are_bit_exact() {
    let mut rng = Rng::new(0x5EED_0005);
    for _ in 0..BINARY_CASES {
        let a = rng.value();
        let b = rng.value();
        let control = rng.control();
        let (rounding, precision) = modes(control);
        let hw = hw_scale(a.to_bytes(), b.to_bytes(), control);
        assert_bit_exact(
            "fscale",
            a,
            Some(b),
            control,
            hw,
            arith::scale(a, b, rounding, precision),
        );
    }
    for _ in 0..UNARY_CASES {
        let a = rng.value();
        let control = rng.control();
        let (mantissa, exponent, status) = hw_fxtract(a.to_bytes(), control);
        let (em_mantissa, em_exponent, em_flags) = arith::extract(a);
        assert!(
            F80::from_bytes(mantissa) == em_mantissa
                && F80::from_bytes(exponent) == em_exponent
                && status & ARITHMETIC_MASK == em_flags & ARITHMETIC_MASK,
            "fxtract {}: hardware ({}, {}, {:04x}) emulated ({}, {}, {:04x})",
            describe(a),
            describe(F80::from_bytes(mantissa)),
            describe(F80::from_bytes(exponent)),
            status & ARITHMETIC_MASK,
            describe(em_mantissa),
            describe(em_exponent),
            em_flags & ARITHMETIC_MASK,
        );
    }
}

#[test]
fn conversions_are_bit_exact() {
    let mut rng = Rng::new(0x5EED_0006);
    for _ in 0..UNARY_CASES {
        let control = rng.control();
        let (rounding, _) = modes(control);
        // Widening from f64: exact by construction, flags included.
        let bits = rng.next();
        let (hw_value, hw_status) = hw_from_f64(bits, control);
        let em = convert::from_f64(bits);
        assert!(
            F80::from_bytes(hw_value) == em.value
                && hw_status & ARITHMETIC_MASK == em.flags & ARITHMETIC_MASK,
            "fld m64 of {bits:016x}: hardware {} {:04x}, emulated {} {:04x}",
            describe(F80::from_bytes(hw_value)),
            hw_status & ARITHMETIC_MASK,
            describe(em.value),
            em.flags & ARITHMETIC_MASK,
        );
        // Narrowing: RC-honoring, overflow/underflow/precision flagged.
        let a = rng.value();
        let (hw_bits, hw_status) = hw_to_f64(a.to_bytes(), control);
        let (em_bits, em_flags) = convert::to_f64(a, rounding);
        assert!(
            hw_bits == em_bits && hw_status & ARITHMETIC_MASK == em_flags & ARITHMETIC_MASK,
            "fstp m64 of {}: cw={control:04x} hardware {hw_bits:016x} {:04x}, emulated {em_bits:016x} {:04x}",
            describe(a),
            hw_status & ARITHMETIC_MASK,
            em_flags & ARITHMETIC_MASK,
        );
        let (hw_bits, hw_status) = hw_to_f32(a.to_bytes(), control);
        let (em_bits, em_flags) = convert::to_f32(a, rounding);
        assert!(
            hw_bits == em_bits && hw_status & ARITHMETIC_MASK == em_flags & ARITHMETIC_MASK,
            "fstp m32 of {}: cw={control:04x} hardware {hw_bits:08x} {:04x}, emulated {em_bits:08x} {:04x}",
            describe(a),
            hw_status & ARITHMETIC_MASK,
            em_flags & ARITHMETIC_MASK,
        );
    }
}

#[test]
fn integer_stores_are_bit_exact() {
    let mut rng = Rng::new(0x5EED_0007);
    for i in 0..UNARY_CASES {
        let control = rng.control();
        let (rounding, _) = modes(control);
        // Mostly in-range values, so the interesting rounding paths get
        // exercised rather than drowned in invalid-operand cases; every
        // fourth case is a fully random pattern for exactly those.
        let a = if i % 4 == 0 {
            rng.value()
        } else {
            let scale = F80::new(false, (BIAS - 8 + rng.below(24) as i32) as u16, 1 << 63);
            arith::mul(
                convert::from_i64(rng.next() as i64 >> rng.below(40)),
                scale,
                Rounding::Nearest,
                Precision::Extended,
            )
            .value
        };
        let (hw16, hw_status) = hw_to_i16(a.to_bytes(), control);
        let (em, em_flags) = convert::to_int(a, rounding, 16);
        assert!(
            hw16 as i64 == em && hw_status & ARITHMETIC_MASK == em_flags & ARITHMETIC_MASK,
            "fistp m16 of {}: cw={control:04x} hardware {hw16} {:04x}, emulated {em} {:04x}",
            describe(a),
            hw_status & ARITHMETIC_MASK,
            em_flags & ARITHMETIC_MASK,
        );
        let (hw32, hw_status) = hw_to_i32(a.to_bytes(), control);
        let (em, em_flags) = convert::to_int(a, rounding, 32);
        assert!(
            hw32 as i64 == em && hw_status & ARITHMETIC_MASK == em_flags & ARITHMETIC_MASK,
            "fistp m32 of {}: cw={control:04x} hardware {hw32} {:04x}, emulated {em} {:04x}",
            describe(a),
            hw_status & ARITHMETIC_MASK,
            em_flags & ARITHMETIC_MASK,
        );
        let (hw64, hw_status) = hw_to_i64(a.to_bytes(), control);
        let (em, em_flags) = convert::to_int(a, rounding, 64);
        assert!(
            hw64 == em && hw_status & ARITHMETIC_MASK == em_flags & ARITHMETIC_MASK,
            "fistp m64 of {}: cw={control:04x} hardware {hw64} {:04x}, emulated {em} {:04x}",
            describe(a),
            hw_status & ARITHMETIC_MASK,
            em_flags & ARITHMETIC_MASK,
        );
    }
}

#[test]
fn comparisons_are_bit_exact() {
    let mut rng = Rng::new(0x5EED_0008);
    for _ in 0..BINARY_CASES {
        let a = rng.value();
        let b = rng.value();
        for quiet in [false, true] {
            let policy = if quiet { NanPolicy::Quiet } else { NanPolicy::Signalling };
            let (relation, em_flags) = compare::compare(a, b, policy);
            let (hw_eflags, hw_status) = hw_comi(a.to_bytes(), b.to_bytes(), quiet);
            assert!(
                hw_eflags == relation.eflags()
                    && hw_status & 0x7F == em_flags & 0x7F,
                "fcomi(quiet={quiet}) {} vs {}: hardware eflags {hw_eflags:02x} status {:04x}, emulated {:02x} flags {:04x}",
                describe(a),
                describe(b),
                hw_status & 0x7F,
                relation.eflags(),
                em_flags & 0x7F,
            );
            let hw_status = hw_com(a.to_bytes(), b.to_bytes(), quiet);
            let condition_mask = flags::C0 | flags::C2 | flags::C3;
            assert!(
                hw_status & condition_mask == relation.condition_codes() & condition_mask
                    && hw_status & 0x7F == em_flags & 0x7F,
                "fcom(quiet={quiet}) {} vs {}: hardware status {hw_status:04x}, emulated codes {:04x} flags {:04x}",
                describe(a),
                describe(b),
                relation.condition_codes(),
                em_flags,
            );
        }
    }
}

#[test]
fn examination_is_bit_exact() {
    let mut rng = Rng::new(0x5EED_0009);
    for _ in 0..UNARY_CASES {
        let a = rng.value();
        let hw_status = hw_fxam(a.to_bytes());
        let em = compare::examine(a);
        assert_eq!(
            hw_status & flags::CONDITIONS,
            em & flags::CONDITIONS,
            "fxam {}",
            describe(a)
        );
    }
}

#[test]
fn constants_are_bit_exact_in_every_mode() {
    for index in 0..7 {
        for rc in 0..4u16 {
            for pc in [0b00u16, 0b10, 0b11] {
                let control = 0x003F | (pc << 8) | (rc << 10);
                let hw = F80::from_bytes(hw_constant(index, control));
                let em = transcendental::constant(index, Rounding::from_control(control));
                assert_eq!(
                    (hw.sign_exponent, hw.significand),
                    (em.sign_exponent, em.significand),
                    "constant {index} cw={control:04x}"
                );
            }
        }
    }
}

// --- the f64-backed tier: measured in ulps, not assumed ---

/// Distance in units of the last place between two finite extendeds of the
/// same sign, treating (exponent, significand) as one ordered integer.
fn ulp_distance(a: F80, b: F80) -> u128 {
    let key = |v: F80| ((v.exponent() as u128) << 64) | v.significand as u128;
    key(a).abs_diff(key(b))
}

fn measure_ulps(
    label: &str,
    cases: usize,
    seed: u64,
    generate: impl Fn(&mut Rng) -> (F80, F80),
    hardware: fn([u8; 10], [u8; 10], u16) -> ([u8; 10], u16),
    emulate: impl Fn(F80, F80) -> arith::Outcome,
    tolerance: u128,
) {
    let mut rng = Rng::new(seed);
    let mut worst: u128 = 0;
    let control = 0x033F; // nearest, extended: the mode the asm libraries run in
    for _ in 0..cases {
        let (x, y) = generate(&mut rng);
        let (hw_bytes, _) = hardware(x.to_bytes(), y.to_bytes(), control);
        let hw = F80::from_bytes(hw_bytes);
        let em = emulate(x, y).value;
        match (hw.classify(), em.classify()) {
            (Class::Normal | Class::Subnormal, Class::Normal | Class::Subnormal) => {
                assert_eq!(hw.sign(), em.sign(), "{label}: sign of {} vs {}", describe(hw), describe(em));
                let distance = ulp_distance(hw, em);
                worst = worst.max(distance);
                assert!(
                    distance <= tolerance,
                    "{label}: x={} y={} hardware {} emulated {} distance {distance}",
                    describe(x),
                    describe(y),
                    describe(hw),
                    describe(em),
                );
            }
            (hw_class, em_class) => assert_eq!(
                hw_class,
                em_class,
                "{label}: x={} y={} hardware {} emulated {}",
                describe(x),
                describe(y),
                describe(hw),
                describe(em),
            ),
        }
    }
    println!("{label}: worst distance {worst} ulps over {cases} cases");
}

#[test]
fn f2xm1_tracks_hardware_within_tolerance() {
    // Domain [−1, 1]: uniform dyadics across it.
    measure_ulps(
        "f2xm1",
        20_000,
        0x5EED_000A,
        |rng| {
            // Strictly inside [−1, 1]: exponents below zero.
            let x = F80::new(
                rng.next() & 1 != 0,
                (BIAS as u64 - 1 - rng.below(65)) as u16,
                (1 << 63) | rng.next(),
            );
            (x, F80::ZERO)
        },
        |a, _b, control| hw_f2xm1(a, control),
        |x, _| transcendental::f2xm1(x),
        1 << 14,
    );
}

#[test]
fn fyl2x_tracks_hardware_within_tolerance() {
    measure_ulps(
        "fyl2x",
        20_000,
        0x5EED_000B,
        |rng| {
            // Positive x across the whole range, including the near-one
            // neighborhood where the log1p path earns its keep.
            let x = match rng.below(3) {
                0 => F80::new(false, (BIAS as u64 + rng.below(129)) as u16 - 64, (1 << 63) | rng.next()),
                1 => F80::new(false, 1 + rng.below(0x7FFE) as u16, (1 << 63) | rng.next()),
                _ => {
                    let tiny = F80::new(rng.next() & 1 != 0, (BIAS as u64 - 2 - rng.below(60)) as u16, (1 << 63) | rng.next());
                    arith::add(F80::ONE, tiny, Rounding::Nearest, Precision::Extended).value
                }
            };
            let y = F80::new(rng.next() & 1 != 0, (BIAS as u64 + rng.below(65)) as u16 - 32, (1 << 63) | rng.next());
            (x, y)
        },
        hw_fyl2x_raw,
        |x, y| transcendental::fyl2x(x, y, Rounding::Nearest, Precision::Extended),
        1 << 14,
    );
}

#[test]
fn fpatan_tracks_hardware_within_tolerance() {
    measure_ulps(
        "fpatan",
        20_000,
        0x5EED_000C,
        |rng| {
            let make = |rng: &mut Rng| {
                F80::new(
                    rng.next() & 1 != 0,
                    (BIAS as u64 + rng.below(65)) as u16 - 32,
                    (1 << 63) | rng.next(),
                )
            };
            (make(rng), make(rng))
        },
        hw_fpatan_raw,
        |x, y| transcendental::fpatan(x, y, Rounding::Nearest, Precision::Extended),
        1 << 14,
    );
}

#[test]
fn fpatan_special_cases_are_bit_exact() {
    // The π-multiple table, against hardware, in all four rounding modes.
    let zero = F80::ZERO;
    let infinity = F80::new(false, 0x7FFF, 1 << 63);
    let one = F80::ONE;
    let specials = [
        (zero, zero),
        (zero.negate(), zero),
        (zero, zero.negate()),
        (zero.negate(), zero.negate()),
        (one, zero),
        (one.negate(), zero),
        (one, zero.negate()),
        (one.negate(), zero.negate()),
        (zero, one),
        (zero, one.negate()),
        (infinity, one),
        (infinity.negate(), one),
        (infinity, one.negate()),
        (infinity.negate(), one.negate()),
        (one, infinity),
        (one, infinity.negate()),
        (infinity, infinity),
        (infinity.negate(), infinity),
        (infinity, infinity.negate()),
        (infinity.negate(), infinity.negate()),
    ];
    for (x, y) in specials {
        for rc in 0..4u16 {
            let control = 0x033F | (rc << 10);
            let (hw_bytes, _) = hw_fpatan_raw(x.to_bytes(), y.to_bytes(), control);
            let em = transcendental::fpatan(
                x,
                y,
                Rounding::from_control(control),
                Precision::Extended,
            );
            assert_eq!(
                F80::from_bytes(hw_bytes),
                em.value,
                "fpatan special x={} y={} cw={control:04x}",
                describe(x),
                describe(y),
            );
        }
    }
}
