//! Recovering signatures from machine code.
//!
//! The transpiler deliberately needs no calling-convention knowledge, which
//! is what makes it correct. Interop needs exactly that knowledge, and the
//! binaries this project aims at are stripped — so it has to be recovered
//! from the code rather than read out of debug information. DWARF, where it
//! happens to survive, is one more piece of evidence to agree with, never the
//! plan of record.
//!
//! **The analysis.** SysV passes integer arguments in rdi, rsi, rdx, rcx, r8,
//! r9 and floating-point ones in xmm0–7, always in that order. So an argument
//! is a register the function *reads before writing*, which is textbook
//! backward liveness — computed over the control-flow graph, kept to the
//! argument registers, and closed under SysV's in-order assignment: if rdx
//! holds an argument then rdi and rsi do too, whether or not the body ever
//! looks at them.
//!
//! **Why it cannot be one function at a time.** `int f(int x) { return g(x); }`
//! compiles at `-O2` to a single `jmp g`. `f` never touches rdi, so on its own
//! it looks like it takes nothing. Liveness therefore has to flow through
//! calls — whatever is live-in to `g` is live at the call, hence live-in to
//! `f` — which makes this a fixpoint over the call graph rather than a pass
//! over each function.
//!
//! **What it refuses to guess.** A call to something with no known signature
//! and no body to look at leaves the caller's own liveness unknowable, and a
//! wrong signature at a boundary is worse than no signature: no signature
//! keeps the uniform wrapper, which always works. So that condition is
//! tracked and propagated, and a function it reaches gets no signature at all
//! rather than a plausible one.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;

use crate::abi::effects::{self, Effects, Location, LocationSet, WidthEvidence};
use crate::abi::{
    ARGUMENT_REGISTERS, AbiType, FLOAT_ARGUMENT_REGISTERS, FLOAT_RETURN_VALUE_REGISTER,
    RETURN_VALUE_REGISTER, Signature, SignatureTable,
};
use crate::cfg::ControlFlowGraph;
use crate::lifter::{self, LiftedFunction};
use crate::reader::{ObjectFile, SymbolBinding};

/// General-purpose registers a call may destroy, by the SysV convention:
/// everything but rbx, rsp, rbp and r12–r15. Used for calls whose callee is
/// not in this object; a callee we can see gets its own measured set, since
/// interprocedural register allocation lets a compiler preserve more than the
/// convention requires for a function only it can name.
const CALLER_SAVED_INTEGERS: [usize; 9] = [0, 1, 2, 6, 7, 8, 9, 10, 11];

/// What a call instruction transfers to.
#[derive(Clone, Debug)]
enum CallTarget {
    /// A function defined in this object, by index.
    Local(usize),
    /// A symbol defined elsewhere.
    External(String),
    /// Through a register or a table: nothing can be said about it.
    Unknown,
}

/// One function's recovered signature, with what stopped it if it has none.
#[derive(Clone, Debug)]
pub struct InferredFunction {
    pub name: String,
    /// Whether anything outside this object could name it. Only these need a
    /// signature: a local symbol's contract is between the compiler and
    /// itself, and after interprocedural register allocation it is often not
    /// SysV at all.
    pub is_global: bool,
    pub signature: Option<Signature>,
    /// Why there is no signature, when there is none.
    pub obstacle: Option<String>,
    /// The argument registers found live at entry, before the prefix rule
    /// filled in the gaps — kept for the report, where the difference between
    /// "read" and "implied by position" is worth seeing.
    live_in: LocationSet,
}

/// What one call site says about the function it calls.
///
/// The caller-side counterpart of a function's own liveness: an argument
/// register that the caller deliberately filled before a call is an argument
/// of the callee, whether the caller computed the value or is passing its own
/// argument straight through. Both cases are visible in the provenance
/// tracker, and a register holding neither is leftover garbage.
#[derive(Clone, Debug)]
pub struct CallSite {
    pub callee: String,
    pub caller: String,
    pub signature: Signature,
    /// Whether the caller set the accumulator to a constant immediately
    /// before the call. That is SysV's vector-count protocol and means the
    /// callee is variadic — which is a reason to refuse, not to guess.
    pub variadic: bool,
}

#[derive(Debug)]
pub struct Inference {
    pub functions: Vec<InferredFunction>,
    /// Evidence about functions this object calls but does not define.
    pub call_sites: Vec<CallSite>,
}

impl Inference {
    /// The signatures that were recovered, in the form everything else
    /// consumes.
    pub fn signatures(&self) -> SignatureTable {
        let mut table = SignatureTable::new();
        for function in &self.functions {
            if let Some(signature) = &function.signature {
                table.insert(function.name.clone(), signature.clone());
            }
        }
        table
    }

    /// A human-readable account of what was decided and on what evidence.
    ///
    /// Inference that cannot be audited is inference that has to be trusted,
    /// which is the opposite of the point.
    pub fn report(&self) -> String {
        let mut text = String::new();
        for function in &self.functions {
            let scope = if function.is_global {
                "global"
            } else {
                "local "
            };
            match (&function.signature, &function.obstacle) {
                (Some(signature), _) => {
                    text.push_str(&format!("{scope} {}\n", signature.render(&function.name)));
                }
                (None, Some(obstacle)) => {
                    text.push_str(&format!("{scope} {}: {obstacle}\n", function.name));
                }
                (None, None) => {
                    text.push_str(&format!("{scope} {}: no signature\n", function.name));
                }
            }
            let _ = function.live_in;
        }

        let foreign = merge_call_sites(&self.call_sites);
        for (name, signature) in foreign.signatures.iter() {
            text.push_str(&format!("extern {}\n", signature.render(name)));
        }
        for (name, reason) in &foreign.refusals {
            text.push_str(&format!("extern {name}: {reason}\n"));
        }
        text
    }
}

/// Everything the fixpoint needs about one function, computed once.
struct Body {
    name: String,
    is_global: bool,
    graph: ControlFlowGraph,
    /// Per instruction, its own register effects.
    effects: Vec<Effects>,
    /// Per instruction, what it calls, if it calls or tail-jumps anywhere.
    calls: HashMap<usize, CallTarget>,
    /// Index into the lifted functions.
    lifted: usize,
}

/// What the fixpoint carries for each function.
#[derive(Clone, Copy, Default)]
struct State {
    /// Argument registers live at entry.
    live_in: LocationSet,
    /// Everything the function or anything it calls may destroy.
    clobbers: LocationSet,
    /// Whether anything in the computation depended on an unknown callee.
    unknown: bool,
}

/// Recovers what it can about every function an object defines.
///
/// `known` seeds the analysis with signatures already established — from a
/// declaration file, or from an earlier pass — which is how a call to
/// something outside the object stops being a dead end.
pub fn infer(object: &ObjectFile, known: &SignatureTable) -> Result<Inference> {
    let lifted = lifter::lift_object(object)?;
    let bodies = build_bodies(object, &lifted)?;

    let mut states: Vec<State> = vec![State::default(); bodies.len()];
    // The call graph can have cycles, so this iterates to a fixpoint rather
    // than walking a topological order. Liveness only ever grows, so it
    // terminates.
    loop {
        let mut changed = false;
        for index in 0..bodies.len() {
            let next = analyse(&bodies[index], &bodies, &states, known);
            let previous = states[index];
            if next.live_in != previous.live_in
                || next.clobbers != previous.clobbers
                || next.unknown != previous.unknown
            {
                states[index] = next;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Signatures need a second fixpoint of their own: an argument's *width*
    // can come from the callee it is passed on to, and that callee's
    // signature is itself being inferred. Seeded with what was declared, so a
    // declaration is never overwritten by a guess.
    let mut estimates = known.clone();
    loop {
        let mut changed = false;
        for (index, body) in bodies.iter().enumerate() {
            if states[index].unknown || known.get(&body.name).is_some() {
                continue;
            }
            let function = &lifted[body.lifted];
            if let Ok(signature) =
                build_signature(body, function, &bodies, states[index].live_in, &estimates)
                && estimates.get(&body.name) != Some(&signature)
            {
                estimates.insert(body.name.clone(), signature);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut call_sites = Vec::new();
    for body in &bodies {
        let parameters = estimates
            .get(&body.name)
            .map(parameter_locations)
            .unwrap_or_default();
        gather_call_sites(body, &lifted[body.lifted], &parameters, &mut call_sites);
    }

    let mut functions = Vec::new();
    for (index, body) in bodies.iter().enumerate() {
        let state = states[index];
        let (signature, obstacle) = if state.unknown {
            (
                None,
                Some(
                    "calls something with no known signature, so what it reads \
                     cannot be established"
                        .to_string(),
                ),
            )
        } else {
            match build_signature(
                body,
                &lifted[body.lifted],
                &bodies,
                state.live_in,
                &estimates,
            ) {
                Ok(signature) => (Some(signature), None),
                Err(reason) => (None, Some(reason)),
            }
        };
        functions.push(InferredFunction {
            name: body.name.clone(),
            is_global: body.is_global,
            signature,
            obstacle,
            live_in: state.live_in,
        });
    }
    Ok(Inference {
        functions,
        call_sites,
    })
}

/// What a whole link set's call sites add up to, per foreign function.
#[derive(Default, Debug)]
pub struct ForeignSignatures {
    pub signatures: SignatureTable,
    /// Foreign functions no signature could be agreed for, and why.
    pub refusals: Vec<(String, String)>,
}

/// Merges every call site in a link set into one signature per callee.
///
/// Call sites must *agree*. A majority vote would be the wrong instinct here:
/// two sites disagreeing means the evidence is being misread somewhere, and
/// picking the popular reading would bury that under a signature that looks
/// authoritative. Disagreement names the callers instead, so the declaration
/// that resolves it can be written knowing what it is resolving.
pub fn merge_call_sites(sites: &[CallSite]) -> ForeignSignatures {
    let mut grouped: BTreeMap<&str, Vec<&CallSite>> = BTreeMap::new();
    for site in sites {
        grouped.entry(site.callee.as_str()).or_default().push(site);
    }

    let mut merged = ForeignSignatures::default();
    for (callee, sites) in grouped {
        if let Some(site) = sites.iter().find(|site| site.variadic) {
            merged.refusals.push((
                callee.to_string(),
                format!(
                    "`{}` sets the vector-count register immediately before the \
                     call, which means `{callee}` is variadic — its arguments \
                     do not all travel in registers and no thunk can carry \
                     them. Declare it only if it is not.",
                    site.caller
                ),
            ));
            continue;
        }

        // Arguments have to agree exactly. Results are allowed to be absent at
        // a site — a caller that ignores what came back has not contradicted
        // anything, it has simply not looked.
        let parameters = &sites[0].signature.parameters;
        if let Some(other) = sites
            .iter()
            .find(|site| &site.signature.parameters != parameters)
        {
            merged.refusals.push((
                callee.to_string(),
                format!(
                    "call sites disagree: `{}` passes {} and `{}` passes {}",
                    sites[0].caller,
                    Signature {
                        parameters: parameters.clone(),
                        result: None
                    }
                    .render(callee),
                    other.caller,
                    Signature {
                        parameters: other.signature.parameters.clone(),
                        result: None
                    }
                    .render(callee),
                ),
            ));
            continue;
        }

        let results: Vec<AbiType> = sites
            .iter()
            .filter_map(|site| site.signature.result)
            .collect();
        if let Some(disagreeing) = results.iter().find(|result| **result != results[0]) {
            merged.refusals.push((
                callee.to_string(),
                format!(
                    "call sites disagree about the result: {:?} and {:?}",
                    results[0], disagreeing
                ),
            ));
            continue;
        }

        merged.signatures.insert(
            callee.to_string(),
            Signature {
                parameters: parameters.clone(),
                result: results.first().copied(),
            },
        );
    }
    merged
}

/// A signature's parameters, keyed by the register each travels in.
fn parameter_locations(signature: &Signature) -> BTreeMap<Location, AbiType> {
    let mut map = BTreeMap::new();
    if let Ok(locations) = signature.argument_locations() {
        for (parameter, location) in signature.parameters.iter().zip(&locations) {
            map.insert(argument_register(*location), *parameter);
        }
    }
    map
}

/// The machine register a SysV argument slot names.
fn argument_register(location: crate::abi::ArgumentLocation) -> Location {
    match location {
        crate::abi::ArgumentLocation::Integer(slot) => Location::Integer(ARGUMENT_REGISTERS[slot]),
        crate::abi::ArgumentLocation::Float(slot) => {
            Location::Float(FLOAT_ARGUMENT_REGISTERS[slot])
        }
    }
}

/// Walks a function looking for what it tells us about the things it calls.
fn gather_call_sites(
    body: &Body,
    function: &LiftedFunction,
    caller_parameters: &BTreeMap<Location, AbiType>,
    sites: &mut Vec<CallSite>,
) {
    let mut factory = iced_x86::InstructionInfoFactory::new();
    let mut provenance = Provenance::holding(caller_parameters);
    // SysV's variadic protocol: `al` holds how many vector registers were
    // used, and compilers set it with `mov $N,%eax` or `xor %eax,%eax`
    // immediately before the call. The value would otherwise be dead, which
    // is what makes it a reliable signal rather than a coincidence.
    let mut accumulator_holds_a_constant = false;

    for (position, lifted) in function.instructions.iter().enumerate() {
        if let Some(CallTarget::External(name)) = body.calls.get(&position)
            && let Some(parameters) = arguments_at_call(&provenance, caller_parameters)
        {
            sites.push(CallSite {
                callee: name.clone(),
                caller: body.name.clone(),
                signature: Signature {
                    parameters,
                    result: result_of_call(function, position, &mut factory),
                },
                variadic: accumulator_holds_a_constant,
            });
        }

        let effects = effects::effects_of(&lifted.instruction, &mut factory);
        let accumulator = Location::Integer(RETURN_VALUE_REGISTER);
        if effects.writes.contains(accumulator) {
            accumulator_holds_a_constant = sets_a_constant(&lifted.instruction);
        } else if effects.reads.contains(accumulator) {
            accumulator_holds_a_constant = false;
        }

        provenance.step(&lifted.instruction, effects);
        // A call destroys the caller-saved registers, so nothing that was in
        // them describes the *next* call.
        if body.calls.contains_key(&position) {
            provenance.forget_caller_saved();
        }
    }
}

/// Whether an instruction loads a constant into the accumulator.
fn sets_a_constant(instruction: &iced_x86::Instruction) -> bool {
    use iced_x86::{Mnemonic, OpKind};
    match instruction.mnemonic() {
        // `xor %eax,%eax` is the zero case, which iced reports as a pure
        // write of a constant rather than as a read of the old value.
        Mnemonic::Xor => {
            instruction.op0_kind() == OpKind::Register
                && instruction.op1_kind() == OpKind::Register
                && instruction.op0_register() == instruction.op1_register()
        }
        Mnemonic::Mov => matches!(
            instruction.op1_kind(),
            OpKind::Immediate8
                | OpKind::Immediate16
                | OpKind::Immediate32
                | OpKind::Immediate32to64
                | OpKind::Immediate64
                | OpKind::Immediate8to32
                | OpKind::Immediate8to64
        ),
        _ => false,
    }
}

/// The arguments a call site appears to be passing.
///
/// SysV assigns in order, so what matters is how far up each register file
/// the caller has filled. A register with no provenance at all holds nothing
/// the caller put there, which is what separates an argument from whatever
/// happened to be left in a caller-saved register.
fn arguments_at_call(
    provenance: &Provenance,
    caller_parameters: &BTreeMap<Location, AbiType>,
) -> Option<Vec<AbiType>> {
    let width = |location: Location, fallback: AbiType| match provenance.of(location) {
        Some(Source::Computed(width)) => Some(width),
        // A value passed straight through is whatever the caller took.
        Some(Source::Argument(origin)) => Some(
            caller_parameters
                .get(&origin.location)
                .copied()
                .unwrap_or(fallback),
        ),
        None => None,
    };

    let integers: Vec<Option<AbiType>> = ARGUMENT_REGISTERS
        .iter()
        .map(|register| width(Location::Integer(*register), AbiType::I64))
        .collect();
    let floats: Vec<Option<AbiType>> = FLOAT_ARGUMENT_REGISTERS
        .iter()
        .map(|register| width(Location::Float(*register), AbiType::F64))
        .collect();

    let integer_count = integers
        .iter()
        .rposition(Option::is_some)
        .map_or(0, |n| n + 1);
    let float_count = floats
        .iter()
        .rposition(Option::is_some)
        .map_or(0, |n| n + 1);

    // The same unrecoverable ordering as on the callee side: SysV fills the
    // two files independently and nothing records how the source interleaved
    // them. A call site cannot say either, so it says nothing.
    if integer_count > 0 && float_count > 0 {
        return None;
    }

    let mut parameters = Vec::new();
    for slot in integers.iter().take(integer_count) {
        parameters.push(slot.unwrap_or(AbiType::I64));
    }
    for slot in floats.iter().take(float_count) {
        parameters.push(slot.unwrap_or(AbiType::F64));
    }
    Some(parameters)
}

/// Whether the caller reads a result register after the call, and as what.
///
/// A result the caller ignores is invisible here, which is the honest answer:
/// nothing at this site says the callee returned anything.
fn result_of_call(
    function: &LiftedFunction,
    call: usize,
    factory: &mut iced_x86::InstructionInfoFactory,
) -> Option<AbiType> {
    let integer = Location::Integer(RETURN_VALUE_REGISTER);
    let float = Location::Float(FLOAT_RETURN_VALUE_REGISTER);

    for lifted in function.instructions.iter().skip(call + 1) {
        let effects = effects::effects_of(&lifted.instruction, factory);
        if effects.reads.contains(float) {
            return Some(scalar_float_width(&lifted.instruction).unwrap_or(AbiType::F64));
        }
        if effects.reads.contains(integer) {
            return Some(read_integer_width(&lifted.instruction));
        }
        // Overwritten without being read: whatever came back was discarded,
        // and this site has nothing to say.
        if effects.writes.contains(integer) || effects.writes.contains(float) {
            return None;
        }
    }
    None
}

/// How wide the accumulator was when it was read.
fn read_integer_width(instruction: &iced_x86::Instruction) -> AbiType {
    for index in 0..instruction.op_count() {
        if instruction.op_kind(index) != iced_x86::OpKind::Register {
            continue;
        }
        let register = instruction.op_register(index);
        if effects::location_of(register) == Some(Location::Integer(RETURN_VALUE_REGISTER)) {
            return if register.is_gpr() && register.size() >= 8 {
                AbiType::I64
            } else {
                AbiType::I32
            };
        }
    }
    AbiType::I64
}

fn build_bodies(object: &ObjectFile, lifted: &[LiftedFunction]) -> Result<Vec<Body>> {
    // Where each function begins, so a direct call with no relocation — which
    // the assembler resolves itself within a section — can be matched to it.
    let mut by_location: HashMap<(usize, u64), usize> = HashMap::new();
    let mut by_symbol: HashMap<usize, usize> = HashMap::new();
    for (index, function) in lifted.iter().enumerate() {
        by_location.insert((function.section, function.offset), index);
        if let Some(symbol) = object.functions[function.function].symbol {
            by_symbol.insert(symbol, index);
        }
    }

    let mut factory = iced_x86::InstructionInfoFactory::new();
    let mut bodies = Vec::new();
    for function in lifted {
        let graph = ControlFlowGraph::build(function)?;
        let effects = function
            .instructions
            .iter()
            .map(|instruction| effects::effects_of(&instruction.instruction, &mut factory))
            .collect();

        let mut calls = HashMap::new();
        for (position, instruction) in function.instructions.iter().enumerate() {
            if let Some(target) = call_target(object, function, instruction, &by_location) {
                calls.insert(position, target);
            }
        }

        let symbol = object.functions[function.function]
            .symbol
            .map(|index| &object.symbols[index]);
        bodies.push(Body {
            name: function.name.clone(),
            // A function nothing named is one nothing outside can call, so
            // its signature is this object's business alone.
            is_global: symbol.is_some_and(|symbol| symbol.binding != SymbolBinding::Local),
            graph,
            effects,
            calls,
            lifted: bodies.len(),
        });
    }
    let _ = by_symbol;
    Ok(bodies)
}

/// What one instruction transfers control to, if it leaves the function.
///
/// Both a `call` and a tail `jmp` count, and for the same reason: whatever the
/// destination reads is live at this point. A conditional branch out of the
/// function — the shape a split cold path takes — counts too.
fn call_target(
    object: &ObjectFile,
    function: &LiftedFunction,
    instruction: &crate::lifter::LiftedInstruction,
    by_location: &HashMap<(usize, u64), usize>,
) -> Option<CallTarget> {
    use iced_x86::FlowControl;
    let flow = instruction.instruction.flow_control();
    let transfers = matches!(
        flow,
        FlowControl::Call | FlowControl::UnconditionalBranch | FlowControl::ConditionalBranch
    );
    if !transfers {
        return matches!(flow, FlowControl::IndirectCall).then_some(CallTarget::Unknown);
    }

    // A relocation names the callee outright.
    let named = instruction.immediate.or_else(|| {
        instruction
            .displacement
            .filter(|r| r.via_global_offset_table)
    });
    if let Some(reference) = named {
        let symbol = &object.symbols[reference.symbol];
        return Some(
            match by_location
                .iter()
                .find(|((section, offset), _)| {
                    object
                        .resolve(reference.symbol, 0)
                        .is_some_and(|(s, o)| s == *section && o == *offset as i64)
                })
                .map(|(_, index)| *index)
            {
                Some(index) => CallTarget::Local(index),
                None => CallTarget::External(symbol.name.clone()),
            },
        );
    }

    // No relocation: the assembler resolved it within this section, so the
    // target is an offset. A branch that stays inside the function is
    // ordinary control flow, not a call.
    let target = instruction.instruction.near_branch64();
    if function.contains(target) {
        return None;
    }
    Some(match by_location.get(&(function.section, target)) {
        Some(index) => CallTarget::Local(*index),
        None => CallTarget::Unknown,
    })
}

/// One function's liveness, given the current estimates for everything it
/// calls.
fn analyse(body: &Body, bodies: &[Body], states: &[State], known: &SignatureTable) -> State {
    let block_count = body.graph.blocks.len();
    let mut live_in: Vec<LocationSet> = vec![LocationSet::new(); block_count];
    let mut clobbers = LocationSet::new();
    let mut unknown = false;

    // Backward liveness to a fixpoint over the blocks. Loops mean a single
    // reverse pass is not enough.
    loop {
        let mut changed = false;
        for block in (0..block_count).rev() {
            let mut live = LocationSet::new();
            for successor in body.graph.successors(block) {
                live.union_with(live_in[successor]);
            }

            let range = body.graph.blocks[block].instructions.clone();
            for position in range.rev() {
                let effects = body.effects[position];
                if let Some(target) = body.calls.get(&position) {
                    let (arguments, callee_clobbers, callee_unknown) =
                        resolve_call(target, bodies, states, known);
                    unknown |= callee_unknown;
                    clobbers.union_with(callee_clobbers);
                    live.remove_all(callee_clobbers);
                    live.union_with(arguments);
                }
                clobbers.union_with(effects.kills);
                live.remove_all(effects.kills);
                live.union_with(effects.reads);
            }

            if live != live_in[block] {
                live_in[block] = live;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    State {
        live_in: live_in.first().copied().unwrap_or_default(),
        clobbers,
        unknown,
    }
}

/// What a call contributes: the arguments it reads, what it destroys, and
/// whether any of that was a guess.
fn resolve_call(
    target: &CallTarget,
    bodies: &[Body],
    states: &[State],
    known: &SignatureTable,
) -> (LocationSet, LocationSet, bool) {
    match target {
        CallTarget::Local(index) => {
            let state = states[*index];
            let _ = &bodies[*index];
            (state.live_in, state.clobbers, state.unknown)
        }
        CallTarget::External(name) => match known.get(name) {
            Some(signature) => (argument_locations(signature), sysv_caller_saved(), false),
            // Nothing to go on. Saying "reads nothing" would invent a
            // signature for the caller out of an absence, and saying "reads
            // everything" would invent a six-argument one. Neither is
            // knowledge, so the caller is marked unknowable instead.
            None => (LocationSet::new(), sysv_caller_saved(), true),
        },
        CallTarget::Unknown => (LocationSet::new(), sysv_caller_saved(), true),
    }
}

fn argument_locations(signature: &Signature) -> LocationSet {
    let mut set = LocationSet::new();
    if let Ok(locations) = signature.argument_locations() {
        for location in locations {
            set.insert(match location {
                crate::abi::ArgumentLocation::Integer(slot) => {
                    Location::Integer(ARGUMENT_REGISTERS[slot])
                }
                crate::abi::ArgumentLocation::Float(slot) => {
                    Location::Float(FLOAT_ARGUMENT_REGISTERS[slot])
                }
            });
        }
    }
    set
}

fn sysv_caller_saved() -> LocationSet {
    let mut set = LocationSet::new();
    for number in CALLER_SAVED_INTEGERS {
        set.insert(Location::Integer(number));
    }
    for number in 0..16 {
        set.insert(Location::Float(number));
    }
    set
}

/// Turns a live-in set into a signature: which slots, how wide, and what
/// comes back.
fn build_signature(
    body: &Body,
    function: &LiftedFunction,
    bodies: &[Body],
    live_in: LocationSet,
    estimates: &SignatureTable,
) -> std::result::Result<Signature, String> {
    // SysV assigns argument registers in order, so a live one implies every
    // earlier one is an argument too — even a parameter the body never reads.
    let integers = ARGUMENT_REGISTERS
        .iter()
        .rposition(|register| live_in.contains(Location::Integer(*register)))
        .map_or(0, |last| last + 1);
    let floats = FLOAT_ARGUMENT_REGISTERS
        .iter()
        .rposition(|register| live_in.contains(Location::Float(*register)))
        .map_or(0, |last| last + 1);

    // Where a signature uses both register files, the machine code does not
    // record how the two interleaved in the source: `f(int, double)` and
    // `f(double, int)` produce byte-identical register assignments. The order
    // is not recoverable, and a guessed order is a wrong signature — worse
    // than none, because no signature keeps the uniform wrapper, which always
    // works. So this refuses, and a declaration is the way to say what only
    // the source knew.
    if integers > 0 && floats > 0 {
        return Err(format!(
            "takes {integers} integer and {floats} floating-point arguments, \
             and their order in the source is not recoverable from the \
             register assignment — declare it"
        ));
    }

    let widths = measure_widths(body, function, bodies, estimates);

    let mut parameters = Vec::new();
    for register in &ARGUMENT_REGISTERS[..integers] {
        parameters.push(
            widths
                .get(&Location::Integer(*register))
                .copied()
                .unwrap_or(AbiType::I64),
        );
    }
    for register in &FLOAT_ARGUMENT_REGISTERS[..floats] {
        parameters.push(
            widths
                .get(&Location::Float(*register))
                .copied()
                .unwrap_or(AbiType::F64),
        );
    }

    Ok(Signature {
        parameters,
        result: infer_result(body, function, bodies, estimates, &widths),
    })
}

/// The signature a call transfers to, as far as it is currently known.
fn callee_signature<'a>(
    target: &CallTarget,
    bodies: &[Body],
    estimates: &'a SignatureTable,
) -> Option<&'a Signature> {
    match target {
        CallTarget::Local(index) => estimates.get(&bodies[*index].name),
        CallTarget::External(name) => estimates.get(name),
        CallTarget::Unknown => None,
    }
}

/// How wide each argument register's value is treated as.
///
/// Two sources, in this order of authority. Locally, the reads of a register
/// before anything overwrites it: a 64-bit register operand means the value
/// really is 64 bits, while a narrower operand or an address use means it is
/// not. Address use is the one that matters most — address arithmetic always
/// names the full register, so without separating it every `int` reached
/// through `lea` and every pointer would come out as `i64`.
///
/// And interprocedurally, the signature of anything the function passes the
/// register straight on to. `int f(int x) { return g(x) + 1; }` at `-O2`
/// never touches rdi at all, so there is no local evidence to have — but `g`
/// is right there saying what it expects.
fn measure_widths(
    body: &Body,
    function: &LiftedFunction,
    bodies: &[Body],
    estimates: &SignatureTable,
) -> BTreeMap<Location, AbiType> {
    let mut addressed = LocationSet::new();
    let mut quad = LocationSet::new();
    let mut narrow = LocationSet::new();
    let mut floats: BTreeMap<Location, AbiType> = BTreeMap::new();
    let mut from_calls: BTreeMap<Location, AbiType> = BTreeMap::new();

    let mut factory = iced_x86::InstructionInfoFactory::new();
    let mut provenance = Provenance::new();
    let mut evidence = Vec::new();

    for (position, lifted) in function.instructions.iter().enumerate() {
        // A call passes on whatever is still untouched, so the callee's own
        // parameter types describe those registers. This is the only evidence
        // there is for a function that hands its arguments straight through
        // and never looks at them.
        if let Some(target) = body.calls.get(&position)
            && let Some(signature) = callee_signature(target, bodies, estimates)
            && let Ok(locations) = signature.argument_locations()
        {
            for (parameter, location) in signature.parameters.iter().zip(&locations) {
                let location = match location {
                    crate::abi::ArgumentLocation::Integer(slot) => {
                        Location::Integer(ARGUMENT_REGISTERS[*slot])
                    }
                    crate::abi::ArgumentLocation::Float(slot) => {
                        Location::Float(FLOAT_ARGUMENT_REGISTERS[*slot])
                    }
                };
                if provenance
                    .argument_of(location)
                    .is_some_and(|origin| origin.exact && origin.location == location)
                {
                    from_calls.entry(location).or_insert(*parameter);
                }
            }
        }

        effects::width_evidence(&lifted.instruction, &mut factory, &mut evidence);
        for (holder, width) in &evidence {
            // Evidence is about the value, not the register holding it: after
            // `mov %rdi,%rax` a dereference of rax says rdi was a pointer.
            let Some(origin) = provenance.argument_of(*holder) else {
                continue;
            };
            // A value merely *derived* from an argument can only testify that
            // the argument was an address. `add %rdx,%rax` on a pointer makes
            // a pointer, and dereferencing the sum proves the base was one —
            // but the 64-bit add says nothing about the argument's own width,
            // because address arithmetic is 64-bit whatever it is indexing.
            if !origin.exact && *width != WidthEvidence::Address {
                continue;
            }
            let location = origin.location;
            match location {
                Location::Integer(_) => match width {
                    WidthEvidence::Address => addressed.insert(location),
                    WidthEvidence::Quad => quad.insert(location),
                    WidthEvidence::Narrow => narrow.insert(location),
                },
                Location::Float(_) => {
                    if let Some(kind) = scalar_float_width(&lifted.instruction) {
                        floats.insert(location, kind);
                    }
                }
            }
        }

        let effects = effects::effects_of(&lifted.instruction, &mut factory);
        provenance.step(&lifted.instruction, effects);
    }

    let mut widths = floats;
    for register in ARGUMENT_REGISTERS {
        let location = Location::Integer(register);
        // Address use wins outright. On a wasm32 target an address is 32 bits
        // however it was computed, and the computing is routinely done at 64
        // bits: gcc walks an array with `lea (%rdi,%rsi,4),%rdx`, which is
        // honest evidence that rdi is a 64-bit register and no evidence at all
        // that the *value* is a 64-bit integer.
        let measured = if addressed.contains(location) {
            Some(AbiType::I32)
        } else if quad.contains(location) {
            Some(AbiType::I64)
        } else if narrow.contains(location) {
            Some(AbiType::I32)
        } else {
            from_calls.get(&location).copied()
        };
        if let Some(measured) = measured {
            widths.insert(location, measured);
        }
    }
    for register in FLOAT_ARGUMENT_REGISTERS {
        let location = Location::Float(register);
        if let std::collections::btree_map::Entry::Vacant(entry) = widths.entry(location)
            && let Some(kind) = from_calls.get(&location)
        {
            entry.insert(*kind);
        }
    }
    widths
}

/// Whether a measured width belongs to the integer register file.
fn made_is_integer(made: Option<AbiType>) -> bool {
    matches!(made, Some(AbiType::I32) | Some(AbiType::I64))
}

/// Where the value in a register or stack slot came from.
///
/// One tracker serves both directions of inference, because both are asking
/// the same question at different moments. Looking at a *callee*, the useful
/// answer is "this is still argument N", so that evidence observed on a copy
/// counts against the argument. Looking at a *call site*, the useful answer
/// is "this holds something meaningful and it is this wide", so that an
/// argument register holding leftover garbage can be told from one the caller
/// deliberately filled.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// One of this function's own incoming arguments, possibly only derived
    /// from it. Address arithmetic produces the derived kind: a pointer plus
    /// an offset is not the pointer, but dereferencing it still proves the
    /// pointer was one.
    Argument(Origin),
    /// A value this function made for itself, and how wide it is.
    Computed(AbiType),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Origin {
    location: Location,
    exact: bool,
}

impl Source {
    fn argument(self) -> Option<Origin> {
        match self {
            Source::Argument(origin) => Some(origin),
            Source::Computed(_) => None,
        }
    }
}

/// Which incoming argument each register and stack slot currently holds.
///
/// Width evidence is about a *value*, but it is observed on whichever
/// register happens to be holding that value at the time — and a compiler
/// moves arguments around before using them. gcc at `-O1` copies a pointer
/// with `mov %rdi,%rax` and dereferences rax; at `-O0` both compilers spill
/// every argument to the stack and reload it. In both cases the evidence that
/// says "this is an address" lands on a register that is not the argument
/// register, and without following the copy it is lost.
///
/// This follows exactly one fact — where a value came from — through exactly
/// the moves that preserve it. It is deliberately not a general value
/// analysis. A sign- or zero-extending move is not followed at all, because
/// the extended value is a different value and evidence about it would
/// mistype the narrow argument it came from; and arithmetic is followed only
/// weakly, carrying enough to recognise a pointer and not enough to claim a
/// width.
struct Provenance {
    integers: [Option<Source>; 16],
    floats: [Option<Source>; 16],
    /// Stack slots, keyed by base register and displacement.
    slots: HashMap<(iced_x86::Register, i64), Source>,
}

impl Provenance {
    /// For a walk that only cares which registers hold a caller's own
    /// parameters, given what those are.
    ///
    /// Assuming every argument register holds an argument would be right for
    /// the callee-side walk and disastrous here: at a call site it would make
    /// every register look deliberately filled, so every call would appear to
    /// pass fourteen arguments.
    fn holding(parameters: &BTreeMap<Location, AbiType>) -> Self {
        let mut provenance = Self {
            integers: [None; 16],
            floats: [None; 16],
            slots: HashMap::new(),
        };
        for location in parameters.keys() {
            provenance.set(
                *location,
                Some(Source::Argument(Origin {
                    location: *location,
                    exact: true,
                })),
            );
        }
        provenance
    }

    fn new() -> Self {
        let mut provenance = Self {
            integers: [None; 16],
            floats: [None; 16],
            slots: HashMap::new(),
        };
        for register in ARGUMENT_REGISTERS {
            provenance.integers[register] = Some(Source::Argument(Origin {
                location: Location::Integer(register),
                exact: true,
            }));
        }
        for register in FLOAT_ARGUMENT_REGISTERS {
            provenance.floats[register] = Some(Source::Argument(Origin {
                location: Location::Float(register),
                exact: true,
            }));
        }
        provenance
    }

    fn of(&self, location: Location) -> Option<Source> {
        match location {
            Location::Integer(number) => self.integers[number],
            Location::Float(number) => self.floats[number],
        }
    }

    /// The incoming argument a location still holds, if it holds one.
    fn argument_of(&self, location: Location) -> Option<Origin> {
        self.of(location).and_then(Source::argument)
    }

    fn set(&mut self, location: Location, source: Option<Source>) {
        match location {
            Location::Integer(number) => self.integers[number] = source,
            Location::Float(number) => self.floats[number] = source,
        }
    }

    /// Forgets everything a call destroys, so that what one call left behind
    /// is not read as argument setup for the next.
    fn forget_caller_saved(&mut self) {
        for number in CALLER_SAVED_INTEGERS {
            self.integers[number] = None;
        }
        self.floats = [None; 16];
    }

    fn of_register(&self, register: iced_x86::Register) -> Option<Source> {
        effects::location_of(register).and_then(|location| self.of(location))
    }

    /// A stack slot's key, for the frame-relative addressing compilers spill
    /// through. Anything indexed, or based on a register that is not a frame
    /// pointer, is not a fixed slot and is not tracked.
    fn slot(instruction: &iced_x86::Instruction) -> Option<(iced_x86::Register, i64)> {
        use iced_x86::Register;
        let base = instruction.memory_base();
        if instruction.memory_index() != Register::None
            || !matches!(base, Register::RBP | Register::RSP)
        {
            return None;
        }
        Some((base, instruction.memory_displacement64() as i64))
    }

    /// Whether every register operand is a full-width one, so that a move
    /// between them preserves the whole value.
    fn full_width(instruction: &iced_x86::Instruction) -> bool {
        (0..instruction.op_count()).all(|index| {
            if instruction.op_kind(index) != iced_x86::OpKind::Register {
                return true;
            }
            let register = instruction.op_register(index);
            !register.is_gpr() || register.size() >= 8
        })
    }

    /// How wide a value this instruction leaves in whatever it writes.
    ///
    /// An address is the case worth separating: `lea 0xc(%rsp),%rdi` and a
    /// RIP-relative `lea` both produce a *pointer*, which on wasm32 is 32
    /// bits however wide the register is. Everything else is read off the
    /// destination — which is exactly right for `mov $0x5,%esi` and for
    /// `xor %eax,%eax`, the two shapes an argument setup mostly consists of.
    fn computed(instruction: &iced_x86::Instruction) -> Option<AbiType> {
        use iced_x86::{Mnemonic, Register};
        if instruction.mnemonic() == Mnemonic::Lea
            && instruction.memory_index() == Register::None
            && matches!(
                instruction.memory_base(),
                Register::RSP | Register::RBP | Register::RIP
            )
        {
            return Some(AbiType::I32);
        }
        if instruction.op_count() == 0 || instruction.op0_kind() != iced_x86::OpKind::Register {
            return None;
        }
        let destination = instruction.op0_register();
        if destination.is_gpr() {
            return Some(if destination.size() >= 8 {
                AbiType::I64
            } else {
                AbiType::I32
            });
        }
        if effects::location_of(destination).is_some() {
            // Only a *scalar* operation leaves something that could be a
            // floating-point argument. A packed one leaves a vectorisation
            // temporary, and a caller that vectorised a loop before making a
            // call would otherwise appear to be passing floats — which, since
            // a signature spanning both register files is refused, would lose
            // the call site altogether.
            return scalar_float_width(instruction);
        }
        None
    }

    fn step(&mut self, instruction: &iced_x86::Instruction, effects: Effects) {
        use iced_x86::{Mnemonic, OpKind};

        // Only value-preserving moves carry provenance across unchanged, and
        // only at full width. A 32-bit `mov %esi,%ecx` copies the low half and
        // zero-extends, so a later 64-bit use of rcx is a use of the *widened*
        // value — evidence about that would report a 64-bit argument where an
        // `int` was passed. The narrow read of esi has already been recorded
        // as evidence in its own right, so nothing is lost by stopping here.
        let moves = matches!(
            instruction.mnemonic(),
            Mnemonic::Mov | Mnemonic::Movq | Mnemonic::Movd | Mnemonic::Movaps | Mnemonic::Movapd
        );
        if moves && Self::full_width(instruction) && instruction.op_count() == 2 {
            match (instruction.op0_kind(), instruction.op1_kind()) {
                (OpKind::Register, OpKind::Register) => {
                    let source = self.of_register(instruction.op1_register());
                    if let Some(destination) = effects::location_of(instruction.op0_register()) {
                        self.set(destination, source);
                        return;
                    }
                }
                (OpKind::Register, OpKind::Memory) => {
                    let source =
                        Self::slot(instruction).and_then(|key| self.slots.get(&key).copied());
                    if let Some(destination) = effects::location_of(instruction.op0_register()) {
                        self.set(destination, source);
                        return;
                    }
                }
                (OpKind::Memory, OpKind::Register) => {
                    let source = self.of_register(instruction.op1_register());
                    if let Some(key) = Self::slot(instruction) {
                        match source {
                            Some(source) => self.slots.insert(key, source),
                            None => self.slots.remove(&key),
                        };
                    }
                    return;
                }
                _ => {}
            }
        }

        // Address arithmetic carries a weaker claim forward. At `-O0` an array
        // index is a plain 64-bit `add` of a scaled offset onto the pointer,
        // and the dereference that proves the argument was a pointer happens
        // on the sum rather than on the argument register.
        let derives = matches!(
            instruction.mnemonic(),
            Mnemonic::Add | Mnemonic::Sub | Mnemonic::Lea
        );
        if derives && Self::full_width(instruction) && instruction.op0_kind() == OpKind::Register {
            let inherited = if instruction.mnemonic() == Mnemonic::Lea {
                self.of_register(instruction.memory_base())
            } else {
                self.of_register(instruction.op0_register()).or_else(|| {
                    (instruction.op1_kind() == OpKind::Register)
                        .then(|| self.of_register(instruction.op1_register()))
                        .flatten()
                })
            };
            // Only an *argument* is worth carrying through arithmetic. This
            // branch exists so that dereferencing `pointer + offset` still
            // proves the pointer was one, and a value the function computed
            // for itself proves nothing of the sort — while inheriting one
            // would be actively wrong: `sub $0x18,%rsp` makes rsp a computed
            // 64-bit value, and `lea 0xc(%rsp),%rdi` would then carry that
            // width onto rdi instead of recognising it as an address.
            if let Some(Source::Argument(origin)) = inherited
                && let Some(destination) = effects::location_of(instruction.op0_register())
            {
                self.set(
                    destination,
                    Some(Source::Argument(Origin {
                        location: origin.location,
                        exact: false,
                    })),
                );
                return;
            }
        }

        // Anything else replaces whatever the registers it writes were
        // holding — a write that is not a full kill still changes the value,
        // so the old provenance ends here either way — and what it leaves
        // behind is a value the function made for itself.
        let made = Self::computed(instruction);
        for number in 0..16 {
            for location in [Location::Integer(number), Location::Float(number)] {
                if effects.writes.contains(location) {
                    self.set(
                        location,
                        made.filter(|_| {
                            matches!(location, Location::Integer(_)) == made_is_integer(made)
                        })
                        .map(Source::Computed),
                    );
                }
            }
        }
        // Moving the stack pointer moves every slot addressed through it.
        if effects.writes.contains(Location::Integer(4)) {
            self.slots
                .retain(|(base, _), _| *base != iced_x86::Register::RSP);
        }
    }
}

/// Whether an instruction works on single- or double-precision values.
///
/// The `ss`/`sd` split in the mnemonics is the only place a `float` and a
/// `double` differ once they are in a register, so it is the only place the
/// width of a floating-point value can be read off.
fn scalar_float_width(instruction: &iced_x86::Instruction) -> Option<AbiType> {
    use iced_x86::Mnemonic::*;
    match instruction.mnemonic() {
        Movss | Addss | Subss | Mulss | Divss | Sqrtss | Minss | Maxss | Comiss | Ucomiss
        | Cmpss | Cvtss2sd | Cvtsi2ss | Cvttss2si | Cvtss2si => Some(AbiType::F32),
        Movsd | Addsd | Subsd | Mulsd | Divsd | Sqrtsd | Minsd | Maxsd | Comisd | Ucomisd
        | Cmpsd | Cvtsd2ss | Cvtsi2sd | Cvttsd2si | Cvtsd2si => Some(AbiType::F64),
        _ => None,
    }
}

/// What the function leaves behind for its caller.
///
/// Read off the last write to a result register: a function that finishes by
/// writing xmm0 returns a floating-point value, one that finishes by writing
/// rax returns an integer, and one that writes neither returns nothing that
/// can be seen from here. A tail call is the exception — it returns whatever
/// its callee returns, and the callee is the one to ask.
///
/// The last case is a real limit rather than a gap: `void f(void) {}` and
/// `double f(double x) { return x; }` both compile to a bare `ret`, so
/// nothing distinguishes them from inside. `None` here means `void`, which is
/// the more common reading; a declaration is how the other one gets said.
fn infer_result(
    body: &Body,
    function: &LiftedFunction,
    bodies: &[Body],
    estimates: &SignatureTable,
    widths: &BTreeMap<Location, AbiType>,
) -> Option<AbiType> {
    let mut factory = iced_x86::InstructionInfoFactory::new();
    let integer = Location::Integer(RETURN_VALUE_REGISTER);
    let float = Location::Float(FLOAT_RETURN_VALUE_REGISTER);

    // A result register holds a result when the last thing that happened to
    // it was a *production*. Being written is not enough on its own: a `void`
    // function that finishes `add $1,%eax; mov %eax,counter(%rip)` writes eax
    // and then spends it, and the spending is what says the value was a
    // means rather than an end.
    let mut produced: BTreeMap<Location, (usize, AbiType)> = BTreeMap::new();
    let mut consumed: BTreeMap<Location, usize> = BTreeMap::new();
    let mut tail_call: Option<Option<AbiType>> = None;

    for (position, lifted) in function.instructions.iter().enumerate() {
        if let Some(target) = body.calls.get(&position) {
            let signature = callee_signature(target, bodies, estimates);
            if lifted.instruction.flow_control() != iced_x86::FlowControl::Call {
                // A tail call hands its callee's result straight back.
                if let Some(signature) = signature {
                    tail_call = Some(signature.result);
                }
            } else if let Some(result) = signature.and_then(|signature| signature.result) {
                // An ordinary call leaves its result in a result register. A
                // function that then returns without touching it is returning
                // it — which is what `return g(x);` looks like once the call
                // is not in tail position.
                let location = match result {
                    AbiType::F32 | AbiType::F64 => float,
                    AbiType::I32 | AbiType::I64 => integer,
                };
                produced.insert(location, (position, result));
            }
            continue;
        }
        // Stack housekeeping is not a computation. clang aligns the stack
        // with `push %rax` / `pop %rax`, and that `pop` lands *after* the
        // floating-point work — so counting it would report an integer
        // result for every float-returning function clang compiles at `-O1`.
        if is_stack_housekeeping(&lifted.instruction) {
            continue;
        }

        let effects = effects::effects_of(&lifted.instruction, &mut factory);
        for location in [integer, float] {
            if effects.writes.contains(location) {
                let width = if location == float {
                    scalar_float_width(&lifted.instruction)
                        .or_else(|| widths.get(&float).copied())
                        .unwrap_or(AbiType::F64)
                } else {
                    result_integer_width(&lifted.instruction)
                };
                produced.insert(location, (position, width));
            } else if effects.reads.contains(location) && !is_pure_predicate(&lifted.instruction) {
                consumed.insert(location, position);
            }
        }
    }

    let standing = produced
        .iter()
        .filter(|(location, (position, _))| {
            consumed.get(location).is_none_or(|spent| spent < position)
        })
        .max_by_key(|(_, (position, _))| *position)
        .map(|(_, (_, width))| *width);

    // Falling back rather than only checking `produced.is_empty()`: a
    // function can compute into a result register on one path and tail-call
    // on another, and if the computed value is spent before returning, the
    // tail call is the only thing left saying what comes back.
    standing.or(tail_call.flatten())
}

/// Whether an instruction only inspects its operands.
///
/// A comparison sets flags and leaves the register exactly as it found it, so
/// reading a result register in one says nothing about whether the value is
/// still meant for the caller. clang compiles a mutual recursion into a loop
/// that ends `cmp $0x1,%eax; ja ...; ret`, where eax is both the loop's test
/// and the returned value.
fn is_pure_predicate(instruction: &iced_x86::Instruction) -> bool {
    use iced_x86::Mnemonic::*;
    matches!(
        instruction.mnemonic(),
        Cmp | Test | Comiss | Comisd | Ucomiss | Ucomisd | Bt
    )
}

/// Whether an instruction is moving the stack around rather than computing
/// anything, so that what it leaves in a register says nothing about a
/// result.
fn is_stack_housekeeping(instruction: &iced_x86::Instruction) -> bool {
    use iced_x86::Mnemonic::*;
    matches!(instruction.mnemonic(), Push | Pop | Pushfq | Popfq | Leave)
}

/// Whether the instruction that produced a result wrote all sixty-four bits
/// of it or only the low half.
fn result_integer_width(instruction: &iced_x86::Instruction) -> AbiType {
    for index in 0..instruction.op_count() {
        if instruction.op_kind(index) != iced_x86::OpKind::Register {
            continue;
        }
        let register = instruction.op_register(index);
        if effects::location_of(register) == Some(Location::Integer(RETURN_VALUE_REGISTER)) {
            return if register.is_gpr() && register.size() >= 8 {
                AbiType::I64
            } else {
                AbiType::I32
            };
        }
    }
    AbiType::I64
}
