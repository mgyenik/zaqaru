//! Differential execution: the backbone of the test suite.
//!
//! Every corpus function is run twice — once as the natively compiled
//! machine code it was lifted from, once as the transpiled, linked and
//! instantiated wasm module — and the two must agree exactly. Fixed inputs
//! cover the interesting boundaries; the pseudorandom sweep covers the rest
//! from a fixed seed, so a failure is reproducible from the test alone.

mod support;

use support::{DifferentialFixture, Pseudorandom, native_function};

/// Inputs worth trying for any 32-bit integer function: the identities, the
/// signed boundaries, and the values that make carries and overflows happen.
const INTERESTING_I32: [i32; 10] = [
    0,
    1,
    -1,
    2,
    -2,
    i32::MAX,
    i32::MIN,
    i32::MAX - 1,
    i32::MIN + 1,
    0x5555_5555,
];

const RANDOM_ITERATIONS: usize = 1000;

type Nullary = unsafe extern "C" fn() -> i32;
type Unary = unsafe extern "C" fn(i32) -> i32;
type Binary = unsafe extern "C" fn(i32, i32) -> i32;
type Ternary = unsafe extern "C" fn(i32, i32, i32) -> i32;

/// Compares one call against every control-flow translation, reporting the
/// arguments and the mode on a mismatch so a failure names the case rather
/// than just the function.
fn expect_agreement(
    transpiled: &mut [(String, support::LinkedModule)],
    name: &str,
    arguments: [i64; 6],
    reported: &[i32],
    expected: i32,
) {
    for (variant, module) in transpiled.iter_mut() {
        let observed = module.call_guest(name, arguments) as i32;
        assert_eq!(
            observed, expected,
            "{name}{reported:?} [{variant}]: transpiled gives {observed}, \
             native gives {expected}"
        );
    }
}

fn check_nullary(fixture: &mut DifferentialFixture, name: &str, calls: usize) {
    let native = unsafe { native_function::<Nullary>(&fixture.native, name) };
    for _ in 0..calls {
        let expected = unsafe { native() };
        expect_agreement(&mut fixture.transpiled, name, [0; 6], &[], expected);
    }
}

fn check_unary(fixture: &mut DifferentialFixture, name: &str, inputs: &[i32]) {
    let native = unsafe { native_function::<Unary>(&fixture.native, name) };
    for &value in inputs {
        let expected = unsafe { native(value) };
        expect_agreement(
            &mut fixture.transpiled,
            name,
            [value as i64, 0, 0, 0, 0, 0],
            &[value],
            expected,
        );
    }
}

fn check_binary(fixture: &mut DifferentialFixture, name: &str, inputs: &[(i32, i32)]) {
    let native = unsafe { native_function::<Binary>(&fixture.native, name) };
    for &(left, right) in inputs {
        let expected = unsafe { native(left, right) };
        expect_agreement(
            &mut fixture.transpiled,
            name,
            [left as i64, right as i64, 0, 0, 0, 0],
            &[left, right],
            expected,
        );
    }
}

fn check_ternary(fixture: &mut DifferentialFixture, name: &str, inputs: &[(i32, i32, i32)]) {
    let native = unsafe { native_function::<Ternary>(&fixture.native, name) };
    for &(first, second, third) in inputs {
        let expected = unsafe { native(first, second, third) };
        expect_agreement(
            &mut fixture.transpiled,
            name,
            [first as i64, second as i64, third as i64, 0, 0, 0],
            &[first, second, third],
            expected,
        );
    }
}

/// Checks a function over a generic pair of arguments, with the caller
/// supplying how to call each side and how to compare. Widths narrower than
/// a register need this: the native side returns in `al` or `ax`, and only
/// those bits are meaningful.
fn check_pairs<Left, Right, Value>(
    transpiled: &mut [(String, support::LinkedModule)],
    name: &str,
    inputs: &[(Left, Right)],
    call_native: impl Fn(Left, Right) -> Value,
    to_register: impl Fn(Left, Right) -> [i64; 6],
    from_register: impl Fn(i64) -> Value,
) where
    Left: Copy + std::fmt::Debug,
    Right: Copy + std::fmt::Debug,
    Value: PartialEq + std::fmt::Debug,
{
    for &(left, right) in inputs {
        let expected = call_native(left, right);
        let arguments = to_register(left, right);
        for (variant, module) in transpiled.iter_mut() {
            let observed = from_register(module.call_guest(name, arguments));
            assert_eq!(
                observed, expected,
                "{name}({left:?}, {right:?}) [{variant}]: transpiled gives \
                 {observed:?}, native gives {expected:?}"
            );
        }
    }
}

/// The single-argument counterpart of [`check_pairs`], for return types that
/// are not `int`.
fn check_singles<Argument, Value>(
    transpiled: &mut [(String, support::LinkedModule)],
    name: &str,
    inputs: &[Argument],
    call_native: impl Fn(Argument) -> Value,
    to_register: impl Fn(Argument) -> i64,
    from_register: impl Fn(i64) -> Value,
) where
    Argument: Copy + std::fmt::Debug,
    Value: PartialEq + std::fmt::Debug,
{
    for &value in inputs {
        let expected = call_native(value);
        let arguments = [to_register(value), 0, 0, 0, 0, 0];
        for (variant, module) in transpiled.iter_mut() {
            let observed = from_register(module.call_guest(name, arguments));
            assert_eq!(
                observed, expected,
                "{name}({value:?}) [{variant}]: transpiled gives {observed:?}, \
                 native gives {expected:?}"
            );
        }
    }
}

fn boundary_pairs() -> Vec<(i32, i32)> {
    let mut pairs = Vec::new();
    for left in INTERESTING_I32 {
        for right in INTERESTING_I32 {
            pairs.push((left, right));
        }
    }
    pairs
}

fn random_pairs(seed: u64, count: usize) -> Vec<(i32, i32)> {
    let mut generator = Pseudorandom::new(seed);
    (0..count)
        .map(|_| (generator.next_i32(), generator.next_i32()))
        .collect()
}

#[test]
fn add_matches_native() {
    let mut fixture = DifferentialFixture::build("add", &["add.c"]);

    // The canonical case from the design document.
    check_binary(&mut fixture, "add", &[(2, 3)]);
    check_binary(&mut fixture, "add", &boundary_pairs());
    check_binary(
        &mut fixture,
        "add",
        &random_pairs(0x2a12_0d5e_1234_5678, RANDOM_ITERATIONS),
    );
}

#[test]
fn control_flow_matches_native() {
    let mut fixture = DifferentialFixture::build("control-flow", &["control_flow.c"]);

    // `gcd` here is the subtractive form, defined for positive inputs.
    let mut generator = Pseudorandom::new(0x51ee_d0f0_0d15_ea5e);
    let mut gcd_inputs: Vec<(i32, i32)> = vec![(1, 1), (12, 18), (17, 5), (1000, 1000), (1, 9973)];
    for _ in 0..RANDOM_ITERATIONS {
        let left = (generator.next_i32().unsigned_abs() % 4096) as i32 + 1;
        let right = (generator.next_i32().unsigned_abs() % 4096) as i32 + 1;
        gcd_inputs.push((left, right));
    }
    check_binary(&mut fixture, "gcd", &gcd_inputs);

    let mut fibonacci_inputs: Vec<i32> = (-3..40).collect();
    fibonacci_inputs.extend([100, 1000, i32::MIN]);
    check_unary(&mut fixture, "fibonacci", &fibonacci_inputs);

    let mut absolute_inputs: Vec<i32> = INTERESTING_I32.to_vec();
    let mut generator = Pseudorandom::new(0xab50_1a7e_0000_0001);
    for _ in 0..RANDOM_ITERATIONS {
        absolute_inputs.push(generator.next_i32());
    }
    check_unary(&mut fixture, "absolute", &absolute_inputs);

    check_binary(&mut fixture, "compare", &boundary_pairs());
    check_binary(
        &mut fixture,
        "compare",
        &random_pairs(0xc0d9_0000_0000_0001, RANDOM_ITERATIONS),
    );

    let mut clamp_inputs: Vec<(i32, i32, i32)> = Vec::new();
    for value in INTERESTING_I32 {
        for low in [-100, 0, 100] {
            for high in [-100, 0, 100, i32::MAX] {
                clamp_inputs.push((value, low, high));
            }
        }
    }
    let mut generator = Pseudorandom::new(0x0c1a_3b7d_0000_0001);
    for _ in 0..RANDOM_ITERATIONS {
        clamp_inputs.push((
            generator.next_i32(),
            generator.next_i32(),
            generator.next_i32(),
        ));
    }
    check_ternary(&mut fixture, "clamp", &clamp_inputs);
}

#[test]
fn calls_match_native() {
    let mut fixture = DifferentialFixture::build("calls", &["calls.c"]);

    let mut inputs: Vec<i32> = INTERESTING_I32.to_vec();
    let mut generator = Pseudorandom::new(0xca11_ed00_0000_0001);
    for _ in 0..RANDOM_ITERATIONS {
        inputs.push(generator.next_i32());
    }
    check_unary(&mut fixture, "apply_helper", &inputs);
    check_unary(&mut fixture, "spill_heavy", &inputs);

    // Recursion needs a real stack: every level pushes a return-address slot
    // and the callee-saved registers its prologue spills.
    check_unary(
        &mut fixture,
        "recursive_fibonacci",
        &(-2..25).collect::<Vec<_>>(),
    );
}

/// The payoff for emitting relocatable objects: two sources transpiled
/// *separately*, linked together by stock `wasm-ld`, calling each other
/// across the object boundary and sharing one emulated register file.
#[test]
fn separately_transpiled_objects_call_each_other() {
    let mut fixture = DifferentialFixture::build(
        "cross-object",
        &["cross_object_ping.c", "cross_object_pong.c"],
    );

    let steps: Vec<i32> = (-2..30).collect();
    check_unary(&mut fixture, "ping", &steps);
    check_unary(&mut fixture, "pong", &steps);

    // Both objects also read and write one shared global, defined by the
    // first and undefined in the second until the linker resolves it.
    check_nullary(&mut fixture, "read_shared_total", 1);
}

#[test]
fn data_matches_native() {
    let mut fixture = DifferentialFixture::build("data", &["data.c"]);

    let mut indices: Vec<i32> = (-4..20).collect();
    let mut generator = Pseudorandom::new(0xda7a_0000_0000_0001);
    for _ in 0..RANDOM_ITERATIONS {
        indices.push(generator.next_i32());
    }
    check_unary(&mut fixture, "table_lookup", &indices);
    check_unary(&mut fixture, "through_pointer", &indices);

    check_nullary(&mut fixture, "fifth_element", 1);
    check_nullary(&mut fixture, "greeting_length", 1);

    // A global that is read, modified and written back: both sides start
    // from the same initialiser and are stepped in lockstep.
    let mut deltas: Vec<i32> = vec![0, 1, -1, 1000];
    for _ in 0..RANDOM_ITERATIONS {
        deltas.push(generator.next_i32());
    }
    check_unary(&mut fixture, "bump_counter", &deltas);

    // Stores into `.bss`, read back at a different index.
    let mut store_inputs: Vec<(i32, i32)> = Vec::new();
    for index in -2..20 {
        for value in [0, 1, -1, i32::MAX, i32::MIN] {
            store_inputs.push((index, value));
        }
    }
    store_inputs.extend(random_pairs(0xb55_0000_0000_0001, RANDOM_ITERATIONS));
    check_binary(&mut fixture, "store_then_load", &store_inputs);
}

/// The irreducible graph the structured translation cannot express: it must
/// hand the function to the dispatcher and still get the right answers.
#[test]
fn an_irreducible_function_still_matches_native() {
    let mut fixture = DifferentialFixture::build("irreducible", &["irreducible.s"]);

    let mut inputs: Vec<(i32, i32)> = Vec::new();
    for selector in [0, 1, -1, 7] {
        for count in -3..24 {
            inputs.push((selector, count));
        }
    }
    inputs.extend(
        random_pairs(0x1_44ed_0000_0001, 200)
            .into_iter()
            .map(|(selector, count)| (selector, count.rem_euclid(64))),
    );
    check_binary(&mut fixture, "irreducible", &inputs);
}

/// Division, and the sign extension that builds the double-width dividend it
/// consumes.
///
/// Every input has a non-zero divisor, and the signed cases avoid
/// `INT_MIN / -1`: both raise a divide error on the machine, which would take
/// the test process down rather than produce a value to compare. The
/// transpiled code traps on exactly the same inputs, which is the point of
/// checking the quotient's width before storing it.
#[test]
fn division_matches_native() {
    let mut fixture = DifferentialFixture::build("division", &["division.c"]);
    let mut generator = Pseudorandom::new(0xd1_1de0_0000_0001);

    let divisible = |left: i32, right: i32| right != 0 && !(left == i32::MIN && right == -1);
    let mut signed_pairs: Vec<(i32, i32)> = boundary_pairs()
        .into_iter()
        .filter(|(left, right)| divisible(*left, *right))
        .collect();
    while signed_pairs.len() < RANDOM_ITERATIONS {
        let (left, right) = (generator.next_i32(), generator.next_i32());
        if divisible(left, right) {
            signed_pairs.push((left, right));
        }
    }
    check_binary(&mut fixture, "signed_quotient", &signed_pairs);
    check_binary(&mut fixture, "signed_remainder", &signed_pairs);

    let unsigned_pairs: Vec<(u32, u32)> = signed_pairs
        .iter()
        .map(|(left, right)| (*left as u32, *right as u32))
        .filter(|(_, right)| *right != 0)
        .collect();
    for name in ["unsigned_quotient", "unsigned_remainder"] {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(u32, u32) -> u32>(&fixture.native, name)
        };
        check_pairs(
            &mut fixture.transpiled,
            name,
            &unsigned_pairs,
            |left, right| unsafe { native(left, right) },
            |left, right| [left as i32 as i64, right as i32 as i64, 0, 0, 0, 0],
            |value| value as u32,
        );
    }

    // The eight-byte forms, whose dividend `cqo` or a zeroed `rdx` produces.
    let mut quad_pairs: Vec<(i64, i64)> = vec![
        (0, 1),
        (1, 1),
        (-1, 1),
        (i64::MAX, 3),
        (i64::MIN, 3),
        (i64::MIN, -1_000_003),
        (i64::MAX, i64::MAX),
    ];
    while quad_pairs.len() < RANDOM_ITERATIONS {
        let (left, right) = (generator.next_i64(), generator.next_i64());
        if right != 0 && !(left == i64::MIN && right == -1) {
            quad_pairs.push((left, right));
        }
    }
    for name in ["quad_quotient", "quad_remainder"] {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(i64, i64) -> i64>(&fixture.native, name)
        };
        check_pairs(
            &mut fixture.transpiled,
            name,
            &quad_pairs,
            |left, right| unsafe { native(left, right) },
            |left, right| [left, right, 0, 0, 0, 0],
            |value| value,
        );
    }

    let quad_unsigned: Vec<(u64, u64)> = quad_pairs
        .iter()
        .map(|(left, right)| (*left as u64, *right as u64))
        .filter(|(_, right)| *right != 0)
        .collect();
    let native = unsafe {
        native_function::<unsafe extern "C" fn(u64, u64) -> u64>(
            &fixture.native,
            "quad_unsigned_quotient",
        )
    };
    check_pairs(
        &mut fixture.transpiled,
        "quad_unsigned_quotient",
        &quad_unsigned,
        |left, right| unsafe { native(left, right) },
        |left, right| [left as i64, right as i64, 0, 0, 0, 0],
        |value| value as u64,
    );

    // Narrow divides: a sixteen-bit one, and a byte-wide one whose remainder
    // lands in `ah` rather than in the data register.
    let word_pairs: Vec<(i16, i16)> = signed_pairs
        .iter()
        .map(|(left, right)| (*left as i16, *right as i16))
        .filter(|(left, right)| *right != 0 && !(*left == i16::MIN && *right == -1))
        .collect();
    let native = unsafe {
        native_function::<unsafe extern "C" fn(i16, i16) -> i16>(&fixture.native, "word_quotient")
    };
    check_pairs(
        &mut fixture.transpiled,
        "word_quotient",
        &word_pairs,
        |left, right| unsafe { native(left, right) },
        |left, right| [left as i64, right as i64, 0, 0, 0, 0],
        |value| value as i16,
    );

    let mut byte_pairs: Vec<(u8, u8)> = Vec::new();
    for left in 0..=255u8 {
        for right in [1u8, 2, 3, 7, 128, 255] {
            byte_pairs.push((left, right));
        }
    }
    for name in ["byte_quotient", "byte_remainder"] {
        let native =
            unsafe { native_function::<unsafe extern "C" fn(u8, u8) -> u8>(&fixture.native, name) };
        check_pairs(
            &mut fixture.transpiled,
            name,
            &byte_pairs,
            |left, right| unsafe { native(left, right) },
            |left, right| [left as i64, right as i64, 0, 0, 0, 0],
            |value| value as u8,
        );
    }

    let mut widen_inputs: Vec<i32> = INTERESTING_I32.to_vec();
    for _ in 0..RANDOM_ITERATIONS {
        widen_inputs.push(generator.next_i32());
    }
    let native =
        unsafe { native_function::<unsafe extern "C" fn(i32) -> i64>(&fixture.native, "widen") };
    check_singles(
        &mut fixture.transpiled,
        "widen",
        &widen_inputs,
        |value| unsafe { native(value) },
        |value| value as i64,
        |value| value,
    );
}

/// Indirect calls: the C-style interface and every other shape a function
/// pointer takes.
///
/// The pointer *values* differ from the native ones — a table slot is not an
/// address — so what is compared is what the program does with them: calls
/// through them, comparisons between them, and checks against null.
#[test]
fn function_pointers_match_native() {
    let mut fixture = DifferentialFixture::build("function-pointers", &["function_pointers.c"]);

    let mut values: Vec<i32> = INTERESTING_I32.to_vec();
    let mut generator = Pseudorandom::new(0xf_0177_0000_0001);
    for _ in 0..RANDOM_ITERATIONS {
        values.push(generator.next_i32());
    }
    for name in [
        "apply_doubled",
        "apply_negated",
        "guarded_present",
        "guarded_absent",
        "run_installed",
    ] {
        check_unary(&mut fixture, name, &values);
    }
    check_unary(&mut fixture, "is_doubled", &(-2..8).collect::<Vec<_>>());

    let mut selected: Vec<(i32, i32)> = Vec::new();
    for which in -1..4 {
        for value in INTERESTING_I32 {
            selected.push((which, value));
        }
    }
    selected.extend(random_pairs(0xf_0177_0000_0002, RANDOM_ITERATIONS));
    check_binary(&mut fixture, "apply_transform", &selected);
    check_binary(&mut fixture, "dispatch", &selected);
    check_binary(&mut fixture, "tail_apply", &selected);
    check_binary(&mut fixture, "same_handler", &selected);

    let mut triples: Vec<(i32, i32, i32)> = Vec::new();
    for which in 0..2 {
        for left in INTERESTING_I32 {
            for right in [0, 1, -1, 7] {
                triples.push((which, left, right));
            }
        }
    }
    check_ternary(&mut fixture, "apply_combine", &triples);

    // A pointer stored in mutable data, chosen at run time: both sides are
    // stepped in lockstep so the installed handler stays in agreement.
    for which in [0, 1, 0, 1, 1, 0] {
        {
            let native_install =
                unsafe { native_function::<unsafe extern "C" fn(i32)>(&fixture.native, "install") };
            unsafe { native_install(which) };
        }
        for (_, module) in fixture.transpiled.iter_mut() {
            module.call_guest("install", [which as i64, 0, 0, 0, 0, 0]);
        }
        check_unary(&mut fixture, "run_installed", &[3, -4, 100]);
    }
}

/// `switch` statements dense enough to compile to a jump table, whose entries
/// are code addresses and so cannot be translated at all — only recognised
/// and turned into a `br_table`.
#[test]
fn switch_dispatch_matches_native() {
    let mut fixture = DifferentialFixture::build("switch-dispatch", &["switch_dispatch.c"]);

    let mut selectors: Vec<(i32, i32)> = Vec::new();
    for selector in -3..20 {
        for value in [0, 1, -1, 7, -7, 1000, i32::MAX, i32::MIN] {
            selectors.push((selector, value));
        }
    }
    selectors.extend(random_pairs(0x5217_0000_0001, RANDOM_ITERATIONS));
    for name in ["classify", "accumulate", "twice", "nested"] {
        check_binary(&mut fixture, name, &selectors);
    }

    // `fold` runs its dispatch inside a loop, so the `br_table` is nested in a
    // `loop` and taken many times.
    let mut folds: Vec<(i32, i32)> = Vec::new();
    for seed in -4..12 {
        for steps in 0..20 {
            folds.push((seed, steps));
        }
    }
    check_binary(&mut fixture, "fold", &folds);

    let mut bytes: Vec<(u8, i32)> = Vec::new();
    for selector in 0..=255u8 {
        bytes.push((selector, 41));
    }
    let native = unsafe {
        native_function::<unsafe extern "C" fn(u8, i32) -> i32>(&fixture.native, "from_byte")
    };
    check_pairs(
        &mut fixture.transpiled,
        "from_byte",
        &bytes,
        |selector, value| unsafe { native(selector, value) },
        |left, right| [left as i64, right as i64, 0, 0, 0, 0],
        |value| value as i32,
    );
}

/// A function pointer taken in one object and called from another: the slot
/// it names is assigned by the linker, so neither side knows it in advance.
#[test]
fn function_pointers_cross_object_boundaries() {
    let mut fixture = DifferentialFixture::build(
        "cross-object-handlers",
        &["cross_object_handler.c", "cross_object_caller.c"],
    );

    let mut inputs: Vec<(i32, i32)> = Vec::new();
    for which in 0..2 {
        for value in [0, 1, -1, 2, -2, 99, -99, i32::MAX, i32::MIN + 1] {
            inputs.push((which, value));
        }
    }
    check_binary(&mut fixture, "run_installed", &inputs);
}

/// A small stack machine: a dispatch `switch` inside a loop, a table of
/// operator handlers called indirectly, a program in read-only data and a
/// stack in `.bss`, all in one function. Each of those is tested on its own
/// elsewhere; this is where they meet.
#[test]
fn an_interpreter_matches_native() {
    let mut fixture = DifferentialFixture::build("interpreter", &["interpreter.c"]);

    let mut seeds: Vec<i32> = (-8..24).collect();
    seeds.extend(INTERESTING_I32);
    let mut generator = Pseudorandom::new(0x147e_0000_0001);
    for _ in 0..RANDOM_ITERATIONS {
        seeds.push(generator.next_i32());
    }
    check_unary(&mut fixture, "run_sample", &seeds);
}

/// How a floating-point result is compared.
///
/// **The NaN rule, and why it is the only exception to comparing raw bits.**
/// Where the guest *generates* a NaN, x86 produces the negative quiet NaN
/// `0xfff8…`, while the wasm specification lets an engine choose any NaN
/// payload for a generated result — so comparing those bits would be testing
/// the engine's canonicalization, not the translation. NaNs are therefore
/// compared as a class. Everything else is compared as raw bits, signed
/// zeroes and subnormals included: no tolerances anywhere.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ResultBits {
    /// A double-precision value, or its NaN class.
    Double,
    /// A single-precision value in the low half, or its NaN class.
    Single,
    /// An integer: always compared exactly, NaN inputs included, because
    /// x86's answer for an unrepresentable conversion is one fixed value.
    Exact,
}

impl ResultBits {
    fn agree(self, observed: u64, expected: u64) -> bool {
        if observed == expected {
            return true;
        }
        match self {
            ResultBits::Exact => false,
            ResultBits::Double => is_double_nan(observed) && is_double_nan(expected),
            ResultBits::Single => is_single_nan(observed as u32) && is_single_nan(expected as u32),
        }
    }
}

fn is_double_nan(bits: u64) -> bool {
    bits & 0x7ff0_0000_0000_0000 == 0x7ff0_0000_0000_0000 && bits & 0x000f_ffff_ffff_ffff != 0
}

fn is_single_nan(bits: u32) -> bool {
    bits & 0x7f80_0000 == 0x7f80_0000 && bits & 0x007f_ffff != 0
}

/// Runs one floating-point corpus function on both sides. Arguments cross as
/// raw bits in the integer argument registers, which is what keeps the
/// comparison exact by construction while the host wrapper is still
/// integer-only.
fn check_float_call(
    transpiled: &mut [(String, support::LinkedModule)],
    name: &str,
    inputs: &[(u64, u64)],
    call_native: impl Fn(u64, u64) -> u64,
    result: ResultBits,
) {
    for &(left, right) in inputs {
        let expected = call_native(left, right);
        for (variant, module) in transpiled.iter_mut() {
            let observed = module.call_guest(name, [left as i64, right as i64, 0, 0, 0, 0]) as u64;
            assert!(
                result.agree(observed, expected),
                "{name}({left:#018x}, {right:#018x}) [{variant}]: transpiled \
                 gives {observed:#018x}, native gives {expected:#018x}"
            );
        }
    }
}

/// Double-precision bit patterns worth trying: the identities and signed
/// zeroes, both infinities, quiet and signalling NaNs, the subnormal
/// boundary, and values astride every conversion limit.
fn interesting_double_bits() -> Vec<u64> {
    let mut values: Vec<u64> = vec![
        0x0000_0000_0000_0000, // +0
        0x8000_0000_0000_0000, // -0
        0x7ff0_0000_0000_0000, // +infinity
        0xfff0_0000_0000_0000, // -infinity
        0x7ff8_0000_0000_0000, // quiet NaN
        0xfff8_0000_0000_0000, // the negative quiet NaN x86 generates
        0x7ff0_0000_0000_0001, // signalling NaN
        0x0000_0000_0000_0001, // smallest subnormal
        0x000f_ffff_ffff_ffff, // largest subnormal
        0x0010_0000_0000_0000, // smallest normal
        0x7fef_ffff_ffff_ffff, // largest finite
        0xffef_ffff_ffff_ffff,
    ];
    for value in [
        0.0f64,
        1.0,
        -1.0,
        0.5,
        -0.5,
        1.5,
        -1.5,
        2.5,
        -2.5,
        3.0,
        -3.0,
        1e-300,
        1e300,
        // Astride the 32-bit conversion limits.
        2147483646.0,
        2147483647.0,
        2147483647.5,
        2147483648.0,
        -2147483647.0,
        -2147483648.0,
        -2147483648.5,
        -2147483649.0,
        // Astride the 64-bit ones. The largest finite value below 2^63 is
        // 2^63 - 1024, the next double down from the limit itself.
        9223372036854774784.0,
        9223372036854775808.0,
        -9223372036854775808.0,
        -9223372036854777856.0,
    ] {
        values.push(value.to_bits());
    }
    let mut generator = Pseudorandom::new(0xf10a_0000_0000_0001);
    for _ in 0..256 {
        // Whole-width random patterns reach the exponent extremes; the
        // second family keeps the exponent moderate so ordinary arithmetic
        // is exercised too.
        values.push(generator.next_u64());
        values.push((generator.next_u64() & 0x800f_ffff_ffff_ffff) | 0x3fd0_0000_0000_0000);
    }
    values
}

/// The same for single precision, with the limits of both integer widths.
fn interesting_single_bits() -> Vec<u32> {
    let mut values: Vec<u32> = vec![
        0x0000_0000,
        0x8000_0000,
        0x7f80_0000,
        0xff80_0000,
        0x7fc0_0000,
        0xffc0_0000,
        0x7f80_0001,
        0x0000_0001,
        0x007f_ffff,
        0x0080_0000,
        0x7f7f_ffff,
        0xff7f_ffff,
    ];
    for value in [
        0.0f32,
        1.0,
        -1.0,
        0.5,
        -0.5,
        1.5,
        -1.5,
        2.5,
        -2.5,
        1e-38,
        1e38,
        2147483520.0,
        2147483648.0,
        -2147483648.0,
        -2147483904.0,
        9223371487098961920.0,
        9223372036854775808.0,
        -9223372036854775808.0,
        -9223373136366403584.0,
    ] {
        values.push(value.to_bits());
    }
    let mut generator = Pseudorandom::new(0xf10a_0000_0000_0002);
    for _ in 0..256 {
        values.push(generator.next_u64() as u32);
        values.push((generator.next_u64() as u32 & 0x807f_ffff) | 0x3e80_0000);
    }
    values
}

/// Pairs each value with a handful of others, plus a diagonal, rather than
/// the full product — which for these lists would be a hundred thousand calls
/// per function for no extra coverage.
///
/// The first `distinguished` entries are additionally paired with each other
/// exhaustively. Those are the ones whose *combinations* matter rather than
/// their individual values: the two zeroes against each other are what tells
/// `minsd` apart from wasm's `f64.min`, and a NaN on either side is what the
/// unordered row of the compare table needs.
fn pair_up<Value: Copy>(
    values: &[Value],
    partners: usize,
    distinguished: usize,
) -> Vec<(Value, Value)> {
    let mut pairs = Vec::new();
    for (index, &left) in values.iter().enumerate() {
        pairs.push((left, left));
        for step in 1..=partners {
            pairs.push((left, values[(index + step * 7 + 1) % values.len()]));
        }
    }
    for &left in &values[..distinguished.min(values.len())] {
        for &right in &values[..distinguished.min(values.len())] {
            pairs.push((left, right));
        }
    }
    pairs
}

/// How many of the interesting-value lists are ordered so that pairing them
/// against each other is worth doing exhaustively: the zeroes, the
/// infinities, the NaNs, and the first few ordinary values after them.
const DISTINGUISHED_FLOATS: usize = 16;

/// Scalar floating-point arithmetic, compares and conversions.
///
/// Everything crosses the boundary as bits in integer registers — the host
/// wrapper cannot carry floats yet — so every comparison here is exact,
/// with the NaN class the only exception. The fixed inputs cover the places
/// the naive mapping would be wrong: ties and NaNs for `min`/`max`, the
/// unordered case for compares, and the values astride every conversion
/// limit.
#[test]
fn scalar_float_matches_native() {
    let mut fixture = DifferentialFixture::build("scalar-float", &["scalar_float.c"]);

    let doubles = interesting_double_bits();
    let double_pairs = pair_up(&doubles, 3, DISTINGUISHED_FLOATS);
    let singles = interesting_single_bits();
    let single_pairs: Vec<(u64, u64)> = pair_up(&singles, 3, DISTINGUISHED_FLOATS)
        .into_iter()
        .map(|(left, right)| (u64::from(left), u64::from(right)))
        .collect();

    for name in [
        "double_add",
        "double_subtract",
        "double_multiply",
        "double_divide",
        "double_chain",
        "double_minimum",
        "double_maximum",
    ] {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(u64, u64) -> u64>(&fixture.native, name)
        };
        check_float_call(
            &mut fixture.transpiled,
            name,
            &double_pairs,
            |left, right| unsafe { native(left, right) },
            ResultBits::Double,
        );
    }

    for name in [
        "float_add",
        "float_subtract",
        "float_multiply",
        "float_divide",
        "float_minimum",
        "float_maximum",
    ] {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(u32, u32) -> u64>(&fixture.native, name)
        };
        check_float_call(
            &mut fixture.transpiled,
            name,
            &single_pairs,
            |left, right| unsafe { native(left as u32, right as u32) },
            ResultBits::Single,
        );
    }

    let double_singles: Vec<(u64, u64)> = doubles.iter().map(|bits| (*bits, 0)).collect();
    let single_singles: Vec<(u64, u64)> =
        singles.iter().map(|bits| (u64::from(*bits), 0)).collect();

    for (name, result) in [
        ("double_square_root", ResultBits::Double),
        ("double_to_float", ResultBits::Single),
        ("resize_round_trip", ResultBits::Double),
    ] {
        let native =
            unsafe { native_function::<unsafe extern "C" fn(u64) -> u64>(&fixture.native, name) };
        check_float_call(
            &mut fixture.transpiled,
            name,
            &double_singles,
            |value, _| unsafe { native(value) },
            result,
        );
    }

    for (name, result) in [
        ("float_square_root", ResultBits::Single),
        ("float_to_double", ResultBits::Double),
    ] {
        let native =
            unsafe { native_function::<unsafe extern "C" fn(u32) -> u64>(&fixture.native, name) };
        check_float_call(
            &mut fixture.transpiled,
            name,
            &single_singles,
            |value, _| unsafe { native(value as u32) },
            result,
        );
    }

    // Compares report through the zero, parity and carry flags; these fold
    // every relation into one integer, so a single wrong row of the table
    // shows up.
    for (name, pairs) in [
        ("double_relations", &double_pairs),
        ("float_relations", &single_pairs),
    ] {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(u64, u64) -> i32>(&fixture.native, name)
        };
        check_float_call(
            &mut fixture.transpiled,
            name,
            pairs,
            |left, right| unsafe { native(left, right) as u32 as u64 },
            ResultBits::Exact,
        );
    }
    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(u64, u64) -> u64>(
                &fixture.native,
                "double_ordered_sum",
            )
        };
        check_float_call(
            &mut fixture.transpiled,
            "double_ordered_sum",
            &double_pairs,
            |left, right| unsafe { native(left, right) },
            ResultBits::Double,
        );
    }

    // Truncating conversions, where an unrepresentable value is the integer
    // indefinite on the machine but a trap or a saturation in wasm.
    for name in ["double_to_int", "double_to_long"] {
        let native =
            unsafe { native_function::<unsafe extern "C" fn(u64) -> i64>(&fixture.native, name) };
        let mask = if name == "double_to_int" {
            0xffff_ffffu64
        } else {
            u64::MAX
        };
        check_float_call(
            &mut fixture.transpiled,
            name,
            &double_singles,
            |value, _| unsafe { native(value) as u64 & mask },
            ResultBits::Exact,
        );
    }
    for name in ["float_to_int", "float_to_long"] {
        let native =
            unsafe { native_function::<unsafe extern "C" fn(u32) -> i64>(&fixture.native, name) };
        let mask = if name == "float_to_int" {
            0xffff_ffffu64
        } else {
            u64::MAX
        };
        check_float_call(
            &mut fixture.transpiled,
            name,
            &single_singles,
            |value, _| unsafe { native(value as u32) as u64 & mask },
            ResultBits::Exact,
        );
    }

    // And back the other way, from every integer worth converting.
    let mut integers: Vec<i64> = vec![
        0,
        1,
        -1,
        2,
        -2,
        i32::MAX as i64,
        i32::MIN as i64,
        i64::MAX,
        i64::MIN,
        i64::MAX - 1,
        1 << 52,
        1 << 53,
        (1 << 53) + 1,
        1 << 62,
    ];
    let mut generator = Pseudorandom::new(0xc0_47e7_0000_0001);
    for _ in 0..RANDOM_ITERATIONS {
        integers.push(generator.next_i64());
    }
    let integer_inputs: Vec<(u64, u64)> = integers.iter().map(|value| (*value as u64, 0)).collect();

    for (name, result) in [
        ("int_to_double", ResultBits::Double),
        ("int_to_float", ResultBits::Single),
    ] {
        let native =
            unsafe { native_function::<unsafe extern "C" fn(i32) -> u64>(&fixture.native, name) };
        check_float_call(
            &mut fixture.transpiled,
            name,
            &integer_inputs,
            |value, _| unsafe { native(value as i32) },
            result,
        );
    }
    for (name, result) in [
        ("long_to_double", ResultBits::Double),
        ("long_to_float", ResultBits::Single),
    ] {
        let native =
            unsafe { native_function::<unsafe extern "C" fn(i64) -> u64>(&fixture.native, name) };
        check_float_call(
            &mut fixture.transpiled,
            name,
            &integer_inputs,
            |value, _| unsafe { native(value as i64) },
            result,
        );
    }
}

/// One corpus function's arguments, laid out in the SysV slots its C
/// signature puts them in.
///
/// This is the harness's knowledge, deliberately: the transpiler's wrapper
/// fills every argument register of both files without knowing any
/// function's real signature, and it is the caller — which does know the
/// signature it is testing — that decides which slots to set.
#[derive(Clone, Copy, Default)]
struct Arguments {
    integers: [i64; 6],
    floats: [f64; 8],
    next_integer: usize,
    next_float: usize,
}

impl Arguments {
    fn integer(mut self, value: i64) -> Self {
        self.integers[self.next_integer] = value;
        self.next_integer += 1;
        self
    }

    fn double(mut self, value: f64) -> Self {
        self.floats[self.next_float] = value;
        self.next_float += 1;
        self
    }

    /// A `float` occupies only the low half of its register, so its four
    /// bytes go into the low half of the `f64` that carries it — reinterpreted
    /// rather than converted, which is what makes the value arrive unchanged.
    fn single(self, value: f32) -> Self {
        self.double(f64::from_bits(u64::from(value.to_bits())))
    }
}

/// Calls one function on every transpiled variant and compares the result the
/// caller asks for.
fn check_natural_call<Value: PartialEq + std::fmt::Debug>(
    transpiled: &mut [(String, support::LinkedModule)],
    name: &str,
    arguments: Arguments,
    described: &str,
    expected: Value,
    from_result: impl Fn(i64, f64) -> Value,
    agree: impl Fn(&Value, &Value) -> bool,
) {
    for (variant, module) in transpiled.iter_mut() {
        let (integer, float) = module.call_guest_fully(name, arguments.integers, arguments.floats);
        let observed = from_result(integer, float);
        assert!(
            agree(&observed, &expected),
            "{name}({described}) [{variant}]: transpiled gives {observed:?}, \
             native gives {expected:?}"
        );
    }
}

fn doubles_agree(observed: &f64, expected: &f64) -> bool {
    observed.to_bits() == expected.to_bits() || (observed.is_nan() && expected.is_nan())
}

fn singles_agree(observed: &f32, expected: &f32) -> bool {
    observed.to_bits() == expected.to_bits() || (observed.is_nan() && expected.is_nan())
}

/// Reads a `double` result out of the float half of the wrapper's answer.
fn double_result(_: i64, float: f64) -> f64 {
    float
}

/// A `float` result lives in the low half of `xmm0`, so only those four bytes
/// are meaningful; whatever sits above them is whatever the register happened
/// to hold.
fn single_result(_: i64, float: f64) -> f32 {
    f32::from_bits(float.to_bits() as u32)
}

/// Functions with natural floating-point signatures, called the way a host
/// actually calls them.
///
/// Milestone 3's corpus passed everything as bits in the integer registers
/// because the wrapper had nowhere else to put a `double`. Now that it fills
/// both register files and returns both result registers, these are ordinary
/// C functions and the SysV slot assignment lives in the test.
#[test]
fn natural_float_signatures_match_native() {
    let mut fixture = DifferentialFixture::build("float-boundary", &["float_boundary.c"]);

    let doubles: Vec<f64> = interesting_double_bits()
        .into_iter()
        .map(f64::from_bits)
        .collect();
    let singles: Vec<f32> = interesting_single_bits()
        .into_iter()
        .map(f32::from_bits)
        .collect();

    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f64) -> f64>(&fixture.native, "double_identity")
        };
        for &value in &doubles {
            check_natural_call(
                &mut fixture.transpiled,
                "double_identity",
                Arguments::default().double(value),
                &format!("{value:?}"),
                unsafe { native(value) },
                double_result,
                doubles_agree,
            );
        }
    }

    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f64, f64) -> f64>(&fixture.native, "double_sum")
        };
        for (left, right) in pair_up(&doubles, 3, DISTINGUISHED_FLOATS) {
            check_natural_call(
                &mut fixture.transpiled,
                "double_sum",
                Arguments::default().double(left).double(right),
                &format!("{left:?}, {right:?}"),
                unsafe { native(left, right) },
                double_result,
                doubles_agree,
            );
        }
    }

    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f64, f64, f64, f64) -> f64>(
                &fixture.native,
                "double_blend",
            )
        };
        let mut generator = Pseudorandom::new(0xb1e_d000_0000_0001);
        for index in 0..512 {
            let pick = |generator: &mut Pseudorandom| {
                doubles[(generator.next_u64() % doubles.len() as u64) as usize]
            };
            let (a, b, c, d) = if index < doubles.len() {
                let value = doubles[index];
                (value, value, value, value)
            } else {
                (
                    pick(&mut generator),
                    pick(&mut generator),
                    pick(&mut generator),
                    pick(&mut generator),
                )
            };
            check_natural_call(
                &mut fixture.transpiled,
                "double_blend",
                Arguments::default().double(a).double(b).double(c).double(d),
                &format!("{a:?}, {b:?}, {c:?}, {d:?}"),
                unsafe { native(a, b, c, d) },
                double_result,
                doubles_agree,
            );
        }
    }

    // Every floating-point argument register at once, in both widths.
    {
        type Eight = unsafe extern "C" fn(f64, f64, f64, f64, f64, f64, f64, f64) -> f64;
        let native = unsafe { native_function::<Eight>(&fixture.native, "double_eight") };
        let mut generator = Pseudorandom::new(0xe1_9847_0000_0001);
        for _ in 0..256 {
            let mut values = [0.0f64; 8];
            for slot in &mut values {
                *slot = doubles[(generator.next_u64() % doubles.len() as u64) as usize];
            }
            let mut arguments = Arguments::default();
            for value in values {
                arguments = arguments.double(value);
            }
            check_natural_call(
                &mut fixture.transpiled,
                "double_eight",
                arguments,
                &format!("{values:?}"),
                unsafe {
                    native(
                        values[0], values[1], values[2], values[3], values[4], values[5],
                        values[6], values[7],
                    )
                },
                double_result,
                doubles_agree,
            );
        }
    }
    {
        type Eight = unsafe extern "C" fn(f32, f32, f32, f32, f32, f32, f32, f32) -> f32;
        let native = unsafe { native_function::<Eight>(&fixture.native, "float_eight") };
        let mut generator = Pseudorandom::new(0xe1_9847_0000_0002);
        for _ in 0..256 {
            let mut values = [0.0f32; 8];
            for slot in &mut values {
                *slot = singles[(generator.next_u64() % singles.len() as u64) as usize];
            }
            let mut arguments = Arguments::default();
            for value in values {
                arguments = arguments.single(value);
            }
            check_natural_call(
                &mut fixture.transpiled,
                "float_eight",
                arguments,
                &format!("{values:?}"),
                unsafe {
                    native(
                        values[0], values[1], values[2], values[3], values[4], values[5],
                        values[6], values[7],
                    )
                },
                single_result,
                singles_agree,
            );
        }
    }

    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f32) -> f32>(&fixture.native, "float_identity")
        };
        for &value in &singles {
            check_natural_call(
                &mut fixture.transpiled,
                "float_identity",
                Arguments::default().single(value),
                &format!("{value:?}"),
                unsafe { native(value) },
                single_result,
                singles_agree,
            );
        }
    }
    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f32, f32) -> f32>(&fixture.native, "float_sum")
        };
        for (left, right) in pair_up(&singles, 3, DISTINGUISHED_FLOATS) {
            check_natural_call(
                &mut fixture.transpiled,
                "float_sum",
                Arguments::default().single(left).single(right),
                &format!("{left:?}, {right:?}"),
                unsafe { native(left, right) },
                single_result,
                singles_agree,
            );
        }
    }

    // Both register files at once, with the arguments interleaved so that the
    // slot each lands in is not the order it was written in.
    {
        type Mixed = unsafe extern "C" fn(i32, f64, i64, f64, f32, i32) -> f64;
        let native = unsafe { native_function::<Mixed>(&fixture.native, "mixed_arguments") };
        let mut generator = Pseudorandom::new(0x11_6ed0_0000_0001);
        for _ in 0..512 {
            let first = generator.next_i32();
            let second = doubles[(generator.next_u64() % doubles.len() as u64) as usize];
            let third = generator.next_i64();
            let fourth = doubles[(generator.next_u64() % doubles.len() as u64) as usize];
            let fifth = singles[(generator.next_u64() % singles.len() as u64) as usize];
            let sixth = generator.next_i32();
            check_natural_call(
                &mut fixture.transpiled,
                "mixed_arguments",
                Arguments::default()
                    .integer(i64::from(first))
                    .double(second)
                    .integer(third)
                    .double(fourth)
                    .single(fifth)
                    .integer(i64::from(sixth)),
                &format!("{first}, {second:?}, {third}, {fourth:?}, {fifth:?}, {sixth}"),
                unsafe { native(first, second, third, fourth, fifth, sixth) },
                double_result,
                doubles_agree,
            );
        }
    }

    // Floating-point arguments with an integer result, which comes back
    // through the other half of the answer.
    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f64, f64) -> i32>(
                &fixture.native,
                "compare_doubles",
            )
        };
        for (left, right) in pair_up(&doubles, 3, DISTINGUISHED_FLOATS) {
            check_natural_call(
                &mut fixture.transpiled,
                "compare_doubles",
                Arguments::default().double(left).double(right),
                &format!("{left:?}, {right:?}"),
                unsafe { native(left, right) },
                |integer, _| integer as i32,
                |observed, expected| observed == expected,
            );
        }
    }
    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f64) -> i64>(
                &fixture.native,
                "double_to_long_natural",
            )
        };
        for &value in &doubles {
            check_natural_call(
                &mut fixture.transpiled,
                "double_to_long_natural",
                Arguments::default().double(value),
                &format!("{value:?}"),
                unsafe { native(value) },
                |integer, _| integer,
                |observed, expected| observed == expected,
            );
        }
    }
    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(i64) -> f64>(
                &fixture.native,
                "long_to_double_natural",
            )
        };
        let mut generator = Pseudorandom::new(0x10_47d0_0000_0001);
        let mut integers: Vec<i64> = vec![0, 1, -1, i64::MAX, i64::MIN, 1 << 53, (1 << 53) + 1];
        for _ in 0..512 {
            integers.push(generator.next_i64());
        }
        for value in integers {
            check_natural_call(
                &mut fixture.transpiled,
                "long_to_double_natural",
                Arguments::default().integer(value),
                &format!("{value}"),
                unsafe { native(value) },
                double_result,
                doubles_agree,
            );
        }
    }

    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f64) -> f32>(&fixture.native, "double_narrow")
        };
        for &value in &doubles {
            check_natural_call(
                &mut fixture.transpiled,
                "double_narrow",
                Arguments::default().double(value),
                &format!("{value:?}"),
                unsafe { native(value) },
                single_result,
                singles_agree,
            );
        }
    }
    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f32) -> f64>(&fixture.native, "float_widen")
        };
        for &value in &singles {
            check_natural_call(
                &mut fixture.transpiled,
                "float_widen",
                Arguments::default().single(value),
                &format!("{value:?}"),
                unsafe { native(value) },
                double_result,
                doubles_agree,
            );
        }
    }

    // A guest-to-guest call carrying floats, and a loop keeping one in a
    // register across iterations.
    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f64, f64, f64) -> f64>(
                &fixture.native,
                "apply_weights",
            )
        };
        let mut generator = Pseudorandom::new(0x9e_1687_0000_0001);
        for _ in 0..512 {
            let pick = |generator: &mut Pseudorandom| {
                doubles[(generator.next_u64() % doubles.len() as u64) as usize]
            };
            let (value, first, second) = (
                pick(&mut generator),
                pick(&mut generator),
                pick(&mut generator),
            );
            check_natural_call(
                &mut fixture.transpiled,
                "apply_weights",
                Arguments::default()
                    .double(value)
                    .double(first)
                    .double(second),
                &format!("{value:?}, {first:?}, {second:?}"),
                unsafe { native(value, first, second) },
                double_result,
                doubles_agree,
            );
        }
    }
    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f64, f64, i32) -> f64>(
                &fixture.native,
                "accumulate",
            )
        };
        let mut generator = Pseudorandom::new(0xacc0_0000_0000_0001);
        for count in [-1i32, 0, 1, 2, 7, 63, 64, 100] {
            for _ in 0..24 {
                let seed = doubles[(generator.next_u64() % doubles.len() as u64) as usize];
                let step = doubles[(generator.next_u64() % doubles.len() as u64) as usize];
                check_natural_call(
                    &mut fixture.transpiled,
                    "accumulate",
                    Arguments::default()
                        .integer(i64::from(count))
                        .double(seed)
                        .double(step),
                    &format!("{seed:?}, {step:?}, {count}"),
                    unsafe { native(seed, step, count) },
                    double_result,
                    doubles_agree,
                );
            }
        }
    }
}

/// Auto-vectorised loops and the bit idioms.
///
/// Packed arithmetic is not something the corpus asks for: it is what
/// compilers produce from ordinary loops at `-O2` and above, over arrays of
/// values nobody described as vectors. The idioms are the other half —
/// `fabs`, negation, `copysign` and branchless selection are bitwise
/// operations against masks in read-only data, not arithmetic.
#[test]
fn packed_operations_match_native() {
    let mut fixture = DifferentialFixture::build("vector-packed", &["vector_packed.c"]);

    let doubles: Vec<f64> = interesting_double_bits()
        .into_iter()
        .map(f64::from_bits)
        .collect();
    let singles: Vec<f32> = interesting_single_bits()
        .into_iter()
        .map(f32::from_bits)
        .collect();

    // Counts around every boundary the loops have: empty, one, the vector
    // width, the array's length, and past both ends.
    let counts: Vec<i32> = vec![-1, 0, 1, 2, 3, 4, 5, 7, 8, 15, 16, 17, 31, 63, 64, 65, 1000];

    let mut generator = Pseudorandom::new(0x9ac4_ed00_0000_0001);
    let mut seeds: Vec<f64> = doubles.iter().copied().take(64).collect();
    for _ in 0..64 {
        seeds.push(doubles[(generator.next_u64() % doubles.len() as u64) as usize]);
    }

    // The reductions and element-wise passes, whose results come back in
    // whichever register their C return type lives in.
    for name in [
        "sum_doubles",
        "scale_doubles",
        "largest_double",
        "sum_of_magnitudes",
    ] {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f64, i32) -> f64>(&fixture.native, name)
        };
        for &value in &seeds {
            for &count in &counts {
                check_natural_call(
                    &mut fixture.transpiled,
                    name,
                    Arguments::default().integer(i64::from(count)).double(value),
                    &format!("{value:?}, {count}"),
                    unsafe { native(value, count) },
                    double_result,
                    doubles_agree,
                );
            }
        }
    }
    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f64, i32) -> f32>(&fixture.native, "sum_singles")
        };
        for &value in &seeds {
            for &count in &counts {
                check_natural_call(
                    &mut fixture.transpiled,
                    "sum_singles",
                    Arguments::default().integer(i64::from(count)).double(value),
                    &format!("{value:?}, {count}"),
                    unsafe { native(value, count) },
                    single_result,
                    singles_agree,
                );
            }
        }
    }
    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f64, i32) -> i32>(&fixture.native, "sum_words")
        };
        for &value in &seeds {
            for &count in &counts {
                check_natural_call(
                    &mut fixture.transpiled,
                    "sum_words",
                    Arguments::default().integer(i64::from(count)).double(value),
                    &format!("{value:?}, {count}"),
                    unsafe { native(value, count) },
                    |integer, _| integer as i32,
                    |observed, expected| observed == expected,
                );
            }
        }
    }
    for name in ["sum_quads", "combine_words"] {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f64, i32) -> i64>(&fixture.native, name)
        };
        for &value in &seeds {
            for &count in &counts {
                check_natural_call(
                    &mut fixture.transpiled,
                    name,
                    Arguments::default().integer(i64::from(count)).double(value),
                    &format!("{value:?}, {count}"),
                    unsafe { native(value, count) },
                    |integer, _| integer,
                    |observed, expected| observed == expected,
                );
            }
        }
    }

    // The sign bit gathered into an integer register, both from a single
    // value and from a vectorised loop.
    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f64) -> i32>(&fixture.native, "sign_of_double")
        };
        for &value in &doubles {
            check_natural_call(
                &mut fixture.transpiled,
                "sign_of_double",
                Arguments::default().double(value),
                &format!("{value:?}"),
                unsafe { native(value) },
                |integer, _| integer as i32,
                |observed, expected| observed == expected,
            );
        }
    }
    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f32) -> i32>(&fixture.native, "sign_of_single")
        };
        for &value in &singles {
            check_natural_call(
                &mut fixture.transpiled,
                "sign_of_single",
                Arguments::default().single(value),
                &format!("{value:?}"),
                unsafe { native(value) },
                |integer, _| integer as i32,
                |observed, expected| observed == expected,
            );
        }
    }
    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f64, i32) -> i32>(
                &fixture.native,
                "any_negative",
            )
        };
        for &value in &seeds {
            for &count in &counts {
                check_natural_call(
                    &mut fixture.transpiled,
                    "any_negative",
                    Arguments::default().integer(i64::from(count)).double(value),
                    &format!("{value:?}, {count}"),
                    unsafe { native(value, count) },
                    |integer, _| integer as i32,
                    |observed, expected| observed == expected,
                );
            }
        }
    }

    // The bit idioms, where the sign of a zero and the payload of a NaN are
    // exactly what is under test — so the inputs are the whole interesting
    // list rather than a sample of it.
    for name in ["absolute_double", "negated_double"] {
        let native =
            unsafe { native_function::<unsafe extern "C" fn(f64) -> f64>(&fixture.native, name) };
        for &value in &doubles {
            check_natural_call(
                &mut fixture.transpiled,
                name,
                Arguments::default().double(value),
                &format!("{value:?}"),
                unsafe { native(value) },
                double_result,
                // A sign flip and a mask are bit operations, not arithmetic,
                // so even a NaN's payload has to survive them unchanged.
                |observed, expected| observed.to_bits() == expected.to_bits(),
            );
        }
    }
    for name in ["absolute_single", "negated_single"] {
        let native =
            unsafe { native_function::<unsafe extern "C" fn(f32) -> f32>(&fixture.native, name) };
        for &value in &singles {
            check_natural_call(
                &mut fixture.transpiled,
                name,
                Arguments::default().single(value),
                &format!("{value:?}"),
                unsafe { native(value) },
                single_result,
                |observed, expected| observed.to_bits() == expected.to_bits(),
            );
        }
    }
    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f64, f64) -> f64>(&fixture.native, "copied_sign")
        };
        for (magnitude, sign) in pair_up(&doubles, 3, DISTINGUISHED_FLOATS) {
            check_natural_call(
                &mut fixture.transpiled,
                "copied_sign",
                Arguments::default().double(magnitude).double(sign),
                &format!("{magnitude:?}, {sign:?}"),
                unsafe { native(magnitude, sign) },
                double_result,
                |observed, expected| observed.to_bits() == expected.to_bits(),
            );
        }
    }
    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f32, f32) -> f32>(
                &fixture.native,
                "copied_sign_single",
            )
        };
        for (magnitude, sign) in pair_up(&singles, 3, DISTINGUISHED_FLOATS) {
            check_natural_call(
                &mut fixture.transpiled,
                "copied_sign_single",
                Arguments::default().single(magnitude).single(sign),
                &format!("{magnitude:?}, {sign:?}"),
                unsafe { native(magnitude, sign) },
                single_result,
                |observed, expected| observed.to_bits() == expected.to_bits(),
            );
        }
    }
    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f64, f64) -> f64>(
                &fixture.native,
                "absolute_difference",
            )
        };
        for (left, right) in pair_up(&doubles, 3, DISTINGUISHED_FLOATS) {
            check_natural_call(
                &mut fixture.transpiled,
                "absolute_difference",
                Arguments::default().double(left).double(right),
                &format!("{left:?}, {right:?}"),
                unsafe { native(left, right) },
                double_result,
                doubles_agree,
            );
        }
    }
    {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(f64, f64, f64) -> f64>(
                &fixture.native,
                "branchless_select",
            )
        };
        let mut generator = Pseudorandom::new(0xb2a_c400_0000_0001);
        for _ in 0..1024 {
            let pick = |generator: &mut Pseudorandom| {
                doubles[(generator.next_u64() % doubles.len() as u64) as usize]
            };
            let (left, right, threshold) = (
                pick(&mut generator),
                pick(&mut generator),
                pick(&mut generator),
            );
            check_natural_call(
                &mut fixture.transpiled,
                "branchless_select",
                Arguments::default()
                    .double(left)
                    .double(right)
                    .double(threshold),
                &format!("{left:?}, {right:?}, {threshold:?}"),
                unsafe { native(left, right, threshold) },
                double_result,
                doubles_agree,
            );
        }
    }
}

/// Every packed operation, one lane width at a time.
///
/// The vectorised corpus covers whichever members of each family that day's
/// vectoriser happened to want, which leaves the rest untested — and a
/// translation that used the wrong lane width, or shifted the wrong way,
/// passes every test that never reaches it. These are hand-written so that
/// each family is covered on purpose.
#[test]
fn packed_lanes_match_native() {
    let mut fixture = DifferentialFixture::build("vector-lanes", &["vector_lanes.s"]);

    let mut patterns: Vec<(i64, i64)> = Vec::new();
    for low in [
        0x0000_0000_0000_0000u64 as i64,
        0x0123_4567_89ab_cdefu64 as i64,
        0xffff_ffff_ffff_ffffu64 as i64,
        0x8000_0000_8000_0000u64 as i64,
        0x7fff_ffff_7fff_ffffu64 as i64,
        0x0001_0002_0003_0004u64 as i64,
        // A pair of ordinary doubles, and a pair of ordinary floats, so the
        // packed float cases see something other than infinities.
        0x3ff0_0000_4000_0000u64 as i64,
        0x4048_0000_c010_0000u64 as i64,
    ] {
        for high in [
            0x0000_0000_0000_0000u64 as i64,
            0xfedc_ba98_7654_3210u64 as i64,
            0xffff_ffff_0000_0000u64 as i64,
            0x5555_5555_aaaa_aaaau64 as i64,
            0x0000_0001_ffff_fffeu64 as i64,
            0x3fe0_0000_bf80_0000u64 as i64,
        ] {
            patterns.push((low, high));
        }
    }
    let mut generator = Pseudorandom::new(0x1a4e_0000_0000_0001);
    for _ in 0..96 {
        patterns.push((generator.next_i64(), generator.next_i64()));
    }

    // The families whose answer is exact: integer arithmetic, comparisons,
    // shifts, the compare masks and the sign-mask gathers.
    let mut exact: Vec<String> = vec![
        "lane_paddb",
        "lane_paddw",
        "lane_paddd",
        "lane_paddq",
        "lane_paddd_memory",
        "lane_psubb",
        "lane_psubw",
        "lane_psubd",
        "lane_psubq",
        "lane_pmullw",
        "lane_pmulld",
        "lane_pmuludq",
        "lane_pmuludq_memory",
        "lane_pcmpeqb",
        "lane_pcmpeqw",
        "lane_pcmpeqd",
        "lane_pcmpeqq",
        "lane_pcmpgtb",
        "lane_pcmpgtw",
        "lane_pcmpgtd",
        "lane_pcmpgtq",
        "lane_psllw_3",
        "lane_psllw_15",
        "lane_psllw_16",
        "lane_pslld_5",
        "lane_pslld_31",
        "lane_pslld_32",
        "lane_psllq_9",
        "lane_psllq_63",
        "lane_psllq_64",
        "lane_psrlw_3",
        "lane_psrlw_16",
        "lane_psrld_5",
        "lane_psrld_32",
        "lane_psrlq_9",
        "lane_psrlq_100",
        "lane_psraw_3",
        "lane_psraw_20",
        "lane_psrad_5",
        "lane_psrad_31",
        "lane_psrad_40",
        "lane_psrldq_0",
        "lane_psrldq_1",
        "lane_psrldq_4",
        "lane_psrldq_7",
        "lane_psrldq_8",
        "lane_psrldq_9",
        "lane_psrldq_15",
        "lane_psrldq_16",
        "lane_pslldq_1",
        "lane_pslldq_4",
        "lane_pslldq_7",
        "lane_pslldq_8",
        "lane_pslldq_9",
        "lane_pslldq_15",
        "lane_pslldq_16",
        "lane_movmskpd",
        "lane_movmskps",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    for predicate in 0..8 {
        for family in ["cmpsd", "cmpss", "cmppd", "cmpps"] {
            exact.push(format!("lane_{family}_{predicate}"));
        }
    }

    for name in &exact {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(i64, i64) -> i64>(&fixture.native, name)
        };
        check_pairs(
            &mut fixture.transpiled,
            name,
            &patterns,
            |low, high| unsafe { native(low, high) },
            |low, high| [low, high, 0, 0, 0, 0],
            |value| value,
        );
    }

    // Packed floating-point arithmetic, whose result can be a NaN the engine
    // is free to spell however it likes — so each lane comes back on its own
    // and is compared as a class.
    for name in [
        "lane_addpd",
        "lane_addpd_memory",
        "lane_subpd",
        "lane_mulpd",
        "lane_divpd",
        "lane_sqrtpd",
        "lane_cvtdq2pd",
    ] {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(i64, i64, i32) -> f64>(&fixture.native, name)
        };
        for &(low, high) in &patterns {
            for lane in 0..2 {
                check_natural_call(
                    &mut fixture.transpiled,
                    name,
                    Arguments::default()
                        .integer(low)
                        .integer(high)
                        .integer(lane),
                    &format!("{low:#018x}, {high:#018x}, lane {lane}"),
                    unsafe { native(low, high, lane as i32) },
                    double_result,
                    doubles_agree,
                );
            }
        }
    }
    for name in [
        "lane_addps",
        "lane_subps",
        "lane_mulps",
        "lane_divps",
        "lane_sqrtps",
        "lane_cvtdq2ps",
    ] {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(i64, i64, i32) -> f32>(&fixture.native, name)
        };
        for &(low, high) in &patterns {
            for lane in 0..4 {
                check_natural_call(
                    &mut fixture.transpiled,
                    name,
                    Arguments::default()
                        .integer(low)
                        .integer(high)
                        .integer(lane),
                    &format!("{low:#018x}, {high:#018x}, lane {lane}"),
                    unsafe { native(low, high, lane as i32) },
                    single_result,
                    singles_agree,
                );
            }
        }
    }
}

/// The parity flag, consumed after integer arithmetic.
///
/// It joins the machine model because floating-point compares report
/// *unordered* through it, but its rule — the parity of the result's low
/// byte, set when that byte holds an *even* number of one bits — is worth
/// pinning down on integer operations first, where the result is not also
/// under test.
#[test]
fn the_parity_flag_matches_native() {
    let mut fixture = DifferentialFixture::build("parity-flag", &["parity_flag.s"]);

    // Low bytes are what parity is computed from, so the fixed inputs walk
    // every one of them against a handful of second operands, and the random
    // sweep covers the rest of the width.
    let mut inputs: Vec<(i64, i64)> = Vec::new();
    for low in 0..256i64 {
        for right in [0i64, 1, -1, 0x0f, 0x55, 0x7f, 0x80] {
            inputs.push((low, right));
            inputs.push((low << 8 | 0x33, right));
        }
    }
    let mut generator = Pseudorandom::new(0x9a_217e_0000_0001);
    for _ in 0..RANDOM_ITERATIONS {
        inputs.push((generator.next_i64(), generator.next_i64()));
    }
    // Shift counts, including the zero that must leave every flag alone.
    for count in 0..40i64 {
        inputs.push((0x1234_5678_9abc_def0u64 as i64, count));
    }

    for name in [
        "parity_after_add_long",
        "parity_after_add_int",
        "parity_after_add_short",
        "parity_after_add_byte",
        "parity_after_subtract",
        "parity_after_and",
        "parity_after_or",
        "parity_after_exclusive_or",
        "parity_after_test",
        "parity_after_test_byte",
        "parity_after_compare",
        "parity_after_compare_long",
        "parity_after_compare_byte",
        "parity_after_increment",
        "parity_after_decrement",
        "parity_after_negate",
        "not_parity_after_add_long",
        "not_parity_after_and",
        "not_parity_after_compare",
        "parity_after_shift_left",
        "parity_after_shift_right",
        "parity_after_variable_shift",
        "parity_branch",
        "parity_branch_complement",
        "parity_conditional_move",
        "parity_survives_a_call",
    ] {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(i64, i64) -> i64>(&fixture.native, name)
        };
        check_pairs(
            &mut fixture.transpiled,
            name,
            &inputs,
            |left, right| unsafe { native(left, right) },
            |left, right| [left, right, 0, 0, 0, 0],
            |value| value,
        );
    }
}

/// Struct copies and neighbour swaps, which compilers spell with SSE register
/// moves even though there is no floating point in them anywhere.
///
/// The XMM register file exists for this before it exists for arithmetic: at
/// `-O1` and above a sixteen-byte assignment is `movdqa`/`movaps`, an
/// eight-byte one `movq`, and an adjacent-slot swap a wide move plus a lane
/// shuffle.
#[test]
fn vector_moves_match_native() {
    let mut fixture = DifferentialFixture::build("vector-moves", &["vector_moves.c"]);

    let mut inputs: Vec<(i32, i32)> = Vec::new();
    for value in INTERESTING_I32 {
        for index in -2..10 {
            inputs.push((value, index));
        }
    }
    inputs.extend(random_pairs(0x5e_c700_0000_0001, RANDOM_ITERATIONS));

    for name in [
        "copy_quad",
        "rotate_quads",
        "copy_pair",
        "swap_neighbours",
        "swap_pair_fields",
        "copy_unaligned",
        "copied_field",
    ] {
        check_binary(&mut fixture, name, &inputs);
    }
}

/// The write mask of every form of the move family, one function each.
///
/// Compiler output covers whichever masks that day's compiler happened to
/// need; these are hand-written so that every form is covered on purpose, and
/// each is set up so a wrong mask changes the answer rather than hiding in
/// bits nothing reads back.
#[test]
fn vector_write_masks_match_native() {
    let mut fixture = DifferentialFixture::build("vector-masks", &["vector_masks.s"]);

    let mut patterns: Vec<(i64, i64)> = Vec::new();
    for low in [
        0x0000_0000_0000_0000u64 as i64,
        0x0123_4567_89ab_cdefu64 as i64,
        0xffff_ffff_ffff_ffffu64 as i64,
        0x8000_0000_8000_0000u64 as i64,
        0x0000_0001_ffff_fffeu64 as i64,
    ] {
        for high in [
            0x0000_0000_0000_0000u64 as i64,
            0xfedc_ba98_7654_3210u64 as i64,
            0xffff_ffff_0000_0000u64 as i64,
            0x0000_0000_ffff_ffffu64 as i64,
            0x5555_5555_aaaa_aaaau64 as i64,
        ] {
            patterns.push((low, high));
        }
    }
    let mut generator = Pseudorandom::new(0x3a5c_0000_0000_0001);
    for _ in 0..64 {
        patterns.push((generator.next_i64(), generator.next_i64()));
    }

    for name in [
        "mask_movsd_register",
        "mask_movsd_memory",
        "mask_movss_register",
        "mask_movss_memory",
        "mask_movsd_store",
        "mask_movss_store",
        "mask_movq_register",
        "mask_movq_memory",
        "mask_movq_from_general",
        "mask_movd_from_general",
        "mask_movd_memory",
        "mask_movq_store",
        "mask_movd_store",
        "mask_movq_to_general",
        "mask_movd_to_general",
        "mask_movaps_register",
        "mask_movaps_memory",
        "mask_movups_memory",
        "mask_movdqa_register",
        "mask_movdqu_memory",
        "mask_movaps_store",
        "mask_movdqu_store",
        "mask_movlpd_load",
        "mask_movhpd_load",
        "mask_movhlps",
        "mask_movlhps",
        "mask_movlpd_store",
        "mask_movhpd_store",
        "mask_pxor_self",
        "mask_xorps_self",
        "mask_pxor_register",
        "mask_pand_register",
        "mask_por_register",
        "mask_pandn_register",
        "mask_andpd_memory",
        "mask_orps_memory",
        "mask_pshufd",
        "mask_pshufd_broadcast",
        "mask_pshufd_memory",
        "mask_punpckldq",
        "mask_punpckhdq",
        "mask_punpcklqdq",
        "mask_punpckhqdq",
        "mask_unpcklps",
        "mask_unpckhpd",
        "mask_shufps",
        "mask_shufpd",
    ] {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(i64, i64) -> i64>(&fixture.native, name)
        };
        check_pairs(
            &mut fixture.transpiled,
            name,
            &patterns,
            |low, high| unsafe { native(low, high) },
            |low, high| [low, high, 0, 0, 0, 0],
            |value| value,
        );
    }
}

#[test]
fn arithmetic_matches_native_at_every_width() {
    let mut fixture = DifferentialFixture::build("arithmetic", &["arithmetic.c"]);
    let mut generator = Pseudorandom::new(0xa817_4d3e_0000_0001);

    // Byte width: sub-register writes must leave the rest of the register
    // alone, and every result must be masked back to eight bits.
    let byte_edges: [u8; 8] = [0, 1, 2, 0x7f, 0x80, 0x81, 0xfe, 0xff];
    let mut byte_inputs: Vec<(u8, u8)> = Vec::new();
    for left in byte_edges {
        for right in byte_edges {
            byte_inputs.push((left, right));
        }
    }
    for _ in 0..RANDOM_ITERATIONS {
        byte_inputs.push((generator.next_u64() as u8, generator.next_u64() as u8));
    }
    let byte_native = unsafe {
        native_function::<unsafe extern "C" fn(u8, u8) -> u8>(&fixture.native, "byte_mix")
    };
    check_pairs(
        &mut fixture.transpiled,
        "byte_mix",
        &byte_inputs,
        |left, right| unsafe { byte_native(left, right) },
        |left, right| [left as i64, right as i64, 0, 0, 0, 0],
        |value| value as u8,
    );

    let word_edges: [i16; 8] = [0, 1, -1, i16::MAX, i16::MIN, i16::MAX - 1, 0x5555, -0x5555];
    let mut word_inputs: Vec<(i16, i16)> = Vec::new();
    for left in word_edges {
        for right in word_edges {
            word_inputs.push((left, right));
        }
    }
    for _ in 0..RANDOM_ITERATIONS {
        word_inputs.push((generator.next_u64() as i16, generator.next_u64() as i16));
    }
    let word_native = unsafe {
        native_function::<unsafe extern "C" fn(i16, i16) -> i16>(&fixture.native, "word_mix")
    };
    check_pairs(
        &mut fixture.transpiled,
        "word_mix",
        &word_inputs,
        |left, right| unsafe { word_native(left, right) },
        |left, right| [left as i64, right as i64, 0, 0, 0, 0],
        |value| value as i16,
    );

    // Quad width, including the 64-bit multiply whose overflow detection is
    // the most intricate piece of flag arithmetic in the translator.
    let quad_edges: [i64; 8] = [
        0,
        1,
        -1,
        i64::MAX,
        i64::MIN,
        i64::MAX - 1,
        1 << 32,
        -(1i64 << 32),
    ];
    let mut quad_inputs: Vec<(i64, i64)> = Vec::new();
    for left in quad_edges {
        for right in quad_edges {
            quad_inputs.push((left, right));
        }
    }
    for _ in 0..RANDOM_ITERATIONS {
        quad_inputs.push((generator.next_i64(), generator.next_i64()));
    }
    // Products that land just either side of the 64-bit boundary.
    for shift in 0..64 {
        quad_inputs.push((1i64 << shift, 1i64 << (63 - shift)));
        quad_inputs.push(((1i64 << shift).wrapping_neg(), 1i64 << (63 - shift)));
    }

    for (name, native) in [
        ("quad_mix", "quad_mix"),
        ("quad_multiply_overflows", "quad_multiply_overflows"),
    ] {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(i64, i64) -> i64>(&fixture.native, native)
        };
        check_pairs(
            &mut fixture.transpiled,
            name,
            &quad_inputs,
            |left, right| unsafe { native(left, right) },
            |left, right| [left, right, 0, 0, 0, 0],
            |value| value,
        );
    }

    let unsigned_edges: [u32; 8] = [
        0,
        1,
        2,
        0x7fff_ffff,
        0x8000_0000,
        0x8000_0001,
        u32::MAX - 1,
        u32::MAX,
    ];
    let mut unsigned_inputs: Vec<(u32, u32)> = Vec::new();
    for left in unsigned_edges {
        for right in unsigned_edges {
            unsigned_inputs.push((left, right));
        }
    }
    for _ in 0..RANDOM_ITERATIONS {
        unsigned_inputs.push((generator.next_u64() as u32, generator.next_u64() as u32));
    }
    for name in ["unsigned_order", "has_bits"] {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(u32, u32) -> i32>(&fixture.native, name)
        };
        check_pairs(
            &mut fixture.transpiled,
            name,
            &unsigned_inputs,
            |left, right| unsafe { native(left, right) },
            |left, right| [left as i32 as i64, right as i32 as i64, 0, 0, 0, 0],
            |value| value as i32,
        );
    }

    let mut pairs = boundary_pairs();
    pairs.extend(random_pairs(0x5417_c0de_0000_0001, RANDOM_ITERATIONS));
    check_binary(&mut fixture, "signed_order", &pairs);
    check_binary(&mut fixture, "multiply_overflows", &pairs);

    // Shift counts, including zero, where every flag keeps its old value.
    let mut shift_inputs: Vec<(i32, i32)> = Vec::new();
    for value in INTERESTING_I32 {
        for count in 0..32 {
            shift_inputs.push((value, count));
        }
    }
    shift_inputs.extend(random_pairs(0x5417_5417_0000_0001, RANDOM_ITERATIONS));
    check_binary(&mut fixture, "variable_shifts", &shift_inputs);
}

/// The one-operand `mul` and `imul`, whose product is twice as wide as its
/// operands and lands in a register pair.
///
/// Compilers reach for exactly one member of this family — the 64-bit signed
/// form, behind a division by a constant — so the corpus is hand-written and
/// covers every width in both signednesses. Each case is checked three ways:
/// the low half, the high half, and the two flags the instruction actually
/// defines. Splitting them is what makes a failure say which half is wrong;
/// checking only the flags, or only the low half, would have let the 64-bit
/// partial-product arithmetic pass on nothing.
#[test]
fn wide_multiplies_match_native() {
    let mut fixture = DifferentialFixture::build("wide-multiply", &["wide_multiply.s"]);

    // Values astride the point where each width's product stops fitting, in
    // both signs — which is where the carry and overflow rules live, and
    // where a high half computed by the wrong shift stops agreeing.
    const EDGES: [i64; 18] = [
        0,
        1,
        -1,
        2,
        0x7f,
        0x80,
        0xff,
        0x7fff,
        0x8000,
        0xffff,
        0x7fff_ffff,
        0x8000_0000,
        0xffff_ffff,
        0x1_0000_0000,
        i64::MAX,
        i64::MIN,
        0x5555_5555_5555_5555,
        -0x5555_5555_5555_5555,
    ];

    let mut inputs: Vec<(i64, i64)> = Vec::new();
    for &left in &EDGES {
        for &right in &EDGES {
            inputs.push((left, right));
        }
    }
    let mut generator = Pseudorandom::new(0x6d75_6c77_0000_0001);
    for _ in 0..RANDOM_ITERATIONS {
        inputs.push((generator.next_i64(), generator.next_i64()));
    }

    for name in [
        "mul_byte_low",
        "mul_byte_high",
        "mul_byte_flags",
        "imul_byte_low",
        "imul_byte_high",
        "imul_byte_flags",
        "mul_word_low",
        "mul_word_high",
        "mul_word_flags",
        "imul_word_low",
        "imul_word_high",
        "imul_word_flags",
        "mul_dword_low",
        "mul_dword_high",
        "mul_dword_flags",
        "imul_dword_low",
        "imul_dword_high",
        "imul_dword_flags",
        "mul_qword_low",
        "mul_qword_high",
        "mul_qword_flags",
        "imul_qword_low",
        "imul_qword_high",
        "imul_qword_flags",
    ] {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(i64, i64) -> i64>(&fixture.native, name)
        };
        check_pairs(
            &mut fixture.transpiled,
            name,
            &inputs,
            |left, right| unsafe { native(left, right) },
            |left, right| [left, right, 0, 0, 0, 0],
            |value| value,
        );
    }
}

/// `adc` and `sbb`, with the carry flag both set and clear.
///
/// The carry *out* is what needs the coverage: a sum that lands exactly on its
/// left operand has wrapped if something was carried in and has not if nothing
/// was, and that one case separates a correct translation from an ordinary
/// `add` with a stray increment. Every width appears because compilers reach
/// for whichever one suits — clang produced the 32-bit form from a mutual
/// recursion, nowhere near the multi-word arithmetic these are named for.
#[test]
fn carrying_arithmetic_matches_native() {
    let mut fixture = DifferentialFixture::build("carry-arithmetic", &["carry_arithmetic.s"]);

    // Operand pairs astride every width's wrap point, so the carry-out rule is
    // exercised at the boundary rather than only in the middle.
    const EDGES: [i64; 12] = [
        0,
        1,
        -1,
        0x7f,
        0x80,
        0xff,
        0x7fff,
        0x8000,
        0xffff,
        0x7fff_ffff,
        0xffff_ffff,
        i64::MIN,
    ];

    let mut inputs: Vec<(i64, i64, i64)> = Vec::new();
    for &left in &EDGES {
        for &right in &EDGES {
            // The third argument chooses the incoming carry, so every pair is
            // tried both ways.
            inputs.push((left, right, 0));
            inputs.push((left, right, 1));
        }
    }
    let mut generator = Pseudorandom::new(0x6361_7272_0000_0001);
    for _ in 0..RANDOM_ITERATIONS {
        inputs.push((
            generator.next_i64(),
            generator.next_i64(),
            (generator.next_u64() & 1) as i64,
        ));
    }

    for name in [
        "adc_byte",
        "adc_byte_flags",
        "sbb_byte",
        "sbb_byte_flags",
        "adc_word",
        "adc_word_flags",
        "sbb_word",
        "sbb_word_flags",
        "adc_dword",
        "adc_dword_flags",
        "sbb_dword",
        "sbb_dword_flags",
        "adc_qword",
        "adc_qword_flags",
        "sbb_qword",
        "sbb_qword_flags",
    ] {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(i64, i64, i64) -> i64>(&fixture.native, name)
        };
        for &(left, right, carry) in &inputs {
            let expected = unsafe { native(left, right, carry) };
            for (variant, module) in &mut fixture.transpiled {
                assert_eq!(
                    module.call_guest(name, [left, right, carry, 0, 0, 0]),
                    expected,
                    "{name}({left}, {right}, carry={carry}) disagreed with native in {variant}"
                );
            }
        }
    }
}

/// `xchg`, `xadd` and `cmpxchg` against native, at every width and through
/// memory.
///
/// This family is what every libc locks with, and it is why a static glibc
/// `hello` could not be translated: 43 of the functions reachable from
/// `_start` use one of them, `__pthread_mutex_lock` and `__calloc` among
/// them. The three are checked together because they are one shape — read
/// the destination, compute, write one or both operands back — and what
/// separates them is exactly what a differential can see: which operand
/// receives what, and which flags survive.
#[test]
fn atomic_read_modify_write_matches_native() {
    let mut fixture = DifferentialFixture::build("atomic-rmw", &["atomic_rmw.s"]);

    // Values astride every width's wrap point, so the carry and overflow
    // rules are exercised at the boundary rather than only in the middle.
    const EDGES: [i64; 12] = [
        0,
        1,
        -1,
        0x7f,
        0x80,
        0xff,
        0x7fff,
        0x8000,
        0xffff,
        0x7fff_ffff,
        0xffff_ffff,
        i64::MIN,
    ];

    let mut inputs: Vec<(i64, i64, i64)> = Vec::new();
    for &left in &EDGES {
        for &right in &EDGES {
            // The third argument is `cmpxchg`'s replacement, and for the
            // others is ignored — except that equal first and second
            // arguments are the case `cmpxchg` is *about*, so they are
            // reached by `left == right` above rather than by luck.
            inputs.push((left, right, 0x5a5a_5a5a_5a5a_5a5a_u64 as i64));
            inputs.push((left, left, right));
        }
    }
    let mut generator = Pseudorandom::new(0x7863_6867_0000_0001);
    for _ in 0..RANDOM_ITERATIONS {
        inputs.push((
            generator.next_i64(),
            generator.next_i64(),
            generator.next_i64(),
        ));
    }

    for name in [
        "xchg_qword",
        "xchg_qword_other",
        "xchg_dword",
        "xchg_dword_upper",
        "xchg_byte",
        "xchg_memory",
        "xchg_same",
        "xadd_qword_sum",
        "xadd_qword_old",
        "xadd_qword_flags",
        "xadd_dword_sum",
        "xadd_byte_flags",
        "xadd_memory",
        "cmpxchg_qword_destination",
        "cmpxchg_qword_accumulator",
        "cmpxchg_qword_flags",
        "cmpxchg_dword_destination",
        "cmpxchg_byte_flags",
        "cmpxchg_memory",
    ] {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(i64, i64, i64) -> i64>(&fixture.native, name)
        };
        for &(first, second, third) in &inputs {
            let expected = unsafe { native(first, second, third) };
            for (variant, module) in &mut fixture.transpiled {
                assert_eq!(
                    module.call_guest(name, [first, second, third, 0, 0, 0]),
                    expected,
                    "{name}({first}, {second}, {third}) disagreed with native in {variant}"
                );
            }
        }
    }
}

/// `rol`/`ror` and the `bt` family against native.
///
/// Bit motion that is not a shift, and the difficulty is entirely in the
/// flags: a shift writes five, a rotate writes two and must leave the other
/// four exactly as it found them, and `bt` writes only the carry. A flag
/// written that should not have been diverges on the *next* instruction
/// rather than this one, so several of these set the flags to a known state
/// first and read back only the ones that are supposed to have survived.
#[test]
fn rotates_and_bit_tests_match_native() {
    let mut fixture = DifferentialFixture::build("rotate-bits", &["rotate_bits.s"]);

    let mut inputs: Vec<(i64, i64)> = Vec::new();
    const VALUES: [i64; 10] = [
        0,
        1,
        -1,
        0x80,
        0xff,
        0x8000,
        0x0123_4567_89ab_cdef,
        i64::MIN,
        i64::MAX,
        0x5555_5555_5555_5555,
    ];
    for &value in &VALUES {
        // Every count from none through past the widest operand, so the
        // masking and the reduction modulo the operand's width are both
        // exercised — including the counts that mask to a multiple of a
        // narrow width, which rotate nothing and still write flags.
        for count in 0..=72i64 {
            inputs.push((value, count));
        }
    }
    let mut generator = Pseudorandom::new(0x726f_7461_0000_0001);
    for _ in 0..RANDOM_ITERATIONS {
        inputs.push((generator.next_i64(), generator.next_i64() & 0x7f));
    }

    for name in [
        "rol_qword",
        "ror_qword",
        "rol_dword",
        "ror_dword",
        "rol_word",
        "ror_word",
        "rol_byte",
        "ror_byte",
        "rol_qword_flags",
        "ror_qword_flags",
        "rol_word_flags",
        "ror_word_flags",
        "rol_byte_flags",
        "rol_preserves_flags",
        "ror_preserves_flags",
        "ror_qword_immediate",
        "rol_word_immediate",
        "bt_qword",
        "bt_dword",
        "bt_qword_immediate",
        "bt_preserves_flags",
        "bts_qword",
        "btr_qword",
        "btc_qword",
        "bts_qword_carry",
        "btr_qword_carry",
        "btc_qword_carry",
        "bts_dword",
    ] {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(i64, i64) -> i64>(&fixture.native, name)
        };
        for &(value, count) in &inputs {
            let expected = unsafe { native(value, count) };
            for (variant, module) in &mut fixture.transpiled {
                assert_eq!(
                    module.call_guest(name, [value, count, 0, 0, 0, 0]),
                    expected,
                    "{name}({value:#x}, {count}) disagreed with native in {variant}"
                );
            }
        }
    }
}

/// Branching past a `lock` prefix, which is two instruction streams sharing
/// bytes.
///
/// glibc's allocator avoids a locked read-modify-write when the process has
/// no second thread by jumping one byte into the instruction that carries
/// the prefix. A linear decode has nothing at that byte, so the branch lands
/// in the middle of an instruction — normally out of scope, and here not,
/// because the two streams differ only in a prefix this translation does not
/// model. Both paths have to compute the same thing, which is exactly what a
/// differential can say.
#[test]
fn a_branch_past_a_lock_prefix_matches_native() {
    let mut fixture = DifferentialFixture::build("lock-elision", &["lock_elision.s"]);

    let mut inputs: Vec<(i64, i64, i64, i64)> = Vec::new();
    const VALUES: [i64; 6] = [0, 1, -1, 0x7fff_ffff, i64::MIN, 0x0123_4567_89ab_cdef];
    for &expected in &VALUES {
        for &current in &VALUES {
            for &replacement in &VALUES {
                // Both ways through: locked and elided. The whole point is
                // that they agree.
                inputs.push((expected, current, replacement, 0));
                inputs.push((expected, current, replacement, 1));
            }
        }
    }
    let mut generator = Pseudorandom::new(0x6c6f_636b_0000_0001);
    for _ in 0..RANDOM_ITERATIONS {
        inputs.push((
            generator.next_i64(),
            generator.next_i64(),
            generator.next_i64(),
            (generator.next_u64() & 1) as i64,
        ));
    }

    for name in ["elided_compare_and_swap", "elided_exchange"] {
        let native = unsafe {
            native_function::<unsafe extern "C" fn(i64, i64, i64, i64) -> i64>(
                &fixture.native,
                name,
            )
        };
        for &(first, second, third, path) in &inputs {
            let expected = unsafe { native(first, second, third, path) };
            for (variant, module) in &mut fixture.transpiled {
                assert_eq!(
                    module.call_guest(name, [first, second, third, path, 0, 0]),
                    expected,
                    "{name}({first}, {second}, {third}, path={path}) disagreed with \
                     native in {variant}"
                );
            }
        }
    }
}
