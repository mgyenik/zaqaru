//! The helper symbols translated code calls, over the one static FPU.
//!
//! Only built for wasm32: natively the tests drive [`X87State`] instances,
//! and a process-global mutable FPU would fight the parallel test harness.
//!
//! Safety of the single static: the container model is one instance, one
//! linear memory, threads that switch only at syscalls — and these helpers
//! are intrinsics that never call anything, so no two activations can
//! overlap. The state is process state, exactly like the register-file
//! globals; the context-switch pair below is how kisal swaps it per
//! thread.
//!
//! Memory-operand convention: f32/f64/integer operands arrive **by value**
//! (the translator does the load/store with its own addressing machinery);
//! only the 80-bit and environment images arrive **by address**, as guest
//! addresses this module reads through shared linear memory.

use core::cell::UnsafeCell;

use crate::compare::NanPolicy;
use crate::ops::Binary;
use crate::state::{ENVIRONMENT_SIZE, IMAGE_SIZE, SAVE_SIZE, X87State};

struct Global(UnsafeCell<X87State>);

// One instance, cooperative switching at syscalls only: nothing observes
// this concurrently.
unsafe impl Sync for Global {}

static STATE: Global = Global(UnsafeCell::new(X87State::new()));

fn state() -> &'static mut X87State {
    unsafe { &mut *STATE.0.get() }
}

unsafe fn guest<const N: usize>(address: u32) -> &'static mut [u8; N] {
    unsafe { &mut *(address as usize as *mut [u8; N]) }
}

// --- loads ---

#[unsafe(no_mangle)]
pub extern "C" fn x87_fld32(bits: u32) {
    state().fld_m32(bits);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fld64(bits: u64) {
    state().fld_m64(bits);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fld80(address: u32) {
    let bytes = *unsafe { guest::<10>(address) };
    state().fld_m80(bytes);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fld_sti(index: u32) {
    state().fld_sti(index);
}

/// `fld1`/`fldl2t`/`fldl2e`/`fldpi`/`fldlg2`/`fldln2`/`fldz`, indexed in
/// opcode order.
#[unsafe(no_mangle)]
pub extern "C" fn x87_fld_const(index: u32) {
    state().fld_constant(index);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fild16(value: i32) {
    state().fild(value as i16 as i64);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fild32(value: i32) {
    state().fild(value as i64);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fild64(value: i64) {
    state().fild(value);
}

// --- stores ---

#[unsafe(no_mangle)]
pub extern "C" fn x87_fst32(pop: u32) -> u32 {
    state().fst_m32(pop != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fst64(pop: u32) -> u64 {
    state().fst_m64(pop != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fstp80(address: u32) {
    let bytes = state().fstp_m80();
    *unsafe { guest::<10>(address) } = bytes;
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fst_sti(index: u32, pop: u32) {
    state().fst_sti(index, pop != 0);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fist16(pop: u32) -> i32 {
    state().fist(16, pop != 0) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fist32(pop: u32) -> i32 {
    state().fist(32, pop != 0) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fist64(pop: u32) -> i64 {
    state().fist(64, pop != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fisttp16() -> i32 {
    state().fisttp(16) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fisttp32() -> i32 {
    state().fisttp(32) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fisttp64() -> i64 {
    state().fisttp(64)
}

// --- arithmetic ---

fn binary_op(op: u32) -> Binary {
    match op {
        0 => Binary::Add,
        1 => Binary::Sub,
        2 => Binary::SubReverse,
        3 => Binary::Mul,
        4 => Binary::Div,
        _ => Binary::DivReverse,
    }
}

/// The register forms: ST(dst) = ST(dst) op ST(src), with `faddp`-family
/// popping.
#[unsafe(no_mangle)]
pub extern "C" fn x87_arith_sti(op: u32, dst: u32, src: u32, pop: u32) {
    state().binary_sti(binary_op(op), dst, src, pop != 0);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_arith32(op: u32, bits: u32) {
    state().binary_m32(binary_op(op), bits);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_arith64(op: u32, bits: u64) {
    state().binary_m64(binary_op(op), bits);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_arith_i16(op: u32, value: i32) {
    state().binary_int(binary_op(op), value as i16 as i64);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_arith_i32(op: u32, value: i32) {
    state().binary_int(binary_op(op), value as i64);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fchs() {
    state().fchs();
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fabs() {
    state().fabs();
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fsqrt() {
    state().fsqrt();
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_frndint() {
    state().frndint();
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fprem() {
    state().fprem(false);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fprem1() {
    state().fprem(true);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fscale() {
    state().fscale();
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fxtract() {
    state().fxtract();
}

// --- transcendentals ---

#[unsafe(no_mangle)]
pub extern "C" fn x87_f2xm1() {
    state().f2xm1();
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fyl2x() {
    state().fyl2x();
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fyl2xp1() {
    state().fyl2xp1();
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fpatan() {
    state().fpatan();
}

// --- stack manipulation ---

#[unsafe(no_mangle)]
pub extern "C" fn x87_fxch(index: u32) {
    state().fxch(index);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_ffree(index: u32, pop: u32) {
    state().ffree(index, pop != 0);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fincstp() {
    state().fincstp();
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fdecstp() {
    state().fdecstp();
}

/// The predicate is evaluated translator-side from the promoted flags.
#[unsafe(no_mangle)]
pub extern "C" fn x87_fcmov(index: u32, take: u32) {
    state().fcmov(index, take != 0);
}

// --- comparison ---

fn policy(quiet: u32) -> NanPolicy {
    if quiet != 0 {
        NanPolicy::Quiet
    } else {
        NanPolicy::Signalling
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fcom_sti(index: u32, quiet: u32, pops: u32) {
    state().fcom_sti(index, policy(quiet), pops);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fcom32(bits: u32, pops: u32) {
    state().fcom_m32(bits, pops);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fcom64(bits: u64, pops: u32) {
    state().fcom_m64(bits, pops);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_ficom16(value: i32, pop: u32) {
    state().ficom(value as i16 as i64, pop);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_ficom32(value: i32, pop: u32) {
    state().ficom(value as i64, pop);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_ftst() {
    state().ftst();
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fxam() {
    state().fxam();
}

/// Returns packed EFLAGS for the translator: CF bit 0, PF bit 2, ZF bit 6.
#[unsafe(no_mangle)]
pub extern "C" fn x87_fcomi(index: u32, quiet: u32, pop: u32) -> u32 {
    state().fcomi(index, policy(quiet), pop != 0)
}

// --- control ---

#[unsafe(no_mangle)]
pub extern "C" fn x87_fnstcw() -> u32 {
    state().control() as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fldcw(value: u32) {
    state().set_control(value as u16);
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fnstsw() -> u32 {
    state().status_word() as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fnclex() {
    state().clear_exceptions();
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_finit() {
    state().reset();
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fnstenv(address: u32) {
    state().store_environment(unsafe { guest::<ENVIRONMENT_SIZE>(address) });
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fldenv(address: u32) {
    state().load_environment(unsafe { guest::<ENVIRONMENT_SIZE>(address) });
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_fnsave(address: u32) {
    state().store_and_reinitialize(unsafe { guest::<SAVE_SIZE>(address) });
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_frstor(address: u32) {
    state().load_saved(unsafe { guest::<SAVE_SIZE>(address) });
}

// --- the context switch, execve, and fwait ---

#[unsafe(no_mangle)]
pub extern "C" fn x87_image_size() -> u32 {
    IMAGE_SIZE as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_save(address: u32) {
    state().save_image(unsafe { guest::<IMAGE_SIZE>(address) });
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_load(address: u32) {
    state().load_image(unsafe { guest::<IMAGE_SIZE>(address) });
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_reset() {
    state().reset();
}

/// `fwait`/`fnop`: nothing to deliver yet. When unmasked-exception
/// delivery exists, this is where the pending ES check goes.
#[unsafe(no_mangle)]
pub extern "C" fn x87_fwait() {}
