//! Assembling a relocatable wasm object from a native one: the stage that
//! decides what symbols exist, what shape they have, and what the linker will
//! be asked to resolve.
//!
//! Naming follows the design's export boundary: the translated body of a
//! guest function `foo` is `foo_guest` with wasm type `() -> ()`, and the
//! clean name `foo` belongs to a host-entry wrapper that marshals arguments
//! into the emulated registers. Cross-object guest calls therefore name
//! `_guest` symbols on both sides.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result, bail};

use crate::abi::{
    ARGUMENT_REGISTERS, FLOAT_ARGUMENT_REGISTERS, FLOAT_RETURN_VALUE_REGISTER,
    RETURN_VALUE_REGISTER, Signature, SignatureTable, marshal,
};
use crate::cfg::ControlFlowGraph;
use crate::emitter::code::{
    DataReference, FunctionBody, FunctionBodyBuilder, FunctionReference, TableReference,
};
use crate::emitter::data::DataSegment;
use crate::emitter::linking::{
    DataSymbolLocation, Relocation as WasmRelocation, RelocationKind as WasmRelocationKind, Symbol,
    SymbolTarget, symbol_flags,
};
use crate::emitter::{
    DefinedFunction, ENVIRONMENT_MODULE, FIRST_TABLE_INDEX, FunctionType, ImportedFunction,
    ValueType, WasmObject,
};
use crate::lifter::{self, LiftedFunction};
use crate::machine::{MachineState, RETURN_ADDRESS_SENTINEL, STACK_POINTER_REGISTER, VectorHalf};
use crate::reader::{ObjectFile, SectionRole, SymbolBinding, SymbolRole};
use crate::structurer;
use crate::translate::{
    FunctionTranslator, RED_ZONE_RESERVED, RESUME_ENTRY_MASK, ResumeSites, SYSCALL_RESERVATION,
    SymbolResolver, SymbolValue,
};

/// Suffix distinguishing a translated function, which runs on the emulated
/// register convention, from the host-entry wrapper that carries its name.
pub const GUEST_SUFFIX: &str = "_guest";

/// The checkpoint-resume driver: walks the chain of resume IDs on a restored
/// guest stack, re-entering each suspended frame's resume body in turn until
/// it pops the sentinel the entry wrapper planted at the bottom. Defined
/// weakly, so that objects transpiled separately link into one module with
/// one driver.
pub const RESUME_DRIVER: &str = "x86_resume";

/// The kernel seam: what a translated `syscall` calls.
///
/// Defined in the guest convention by the generated seam object, which reads
/// the Linux syscall ABI out of the register globals and makes it an
/// ordinary typed call into the kernel. Named rather than inferred, because
/// `syscall` carries no operand for a relocation to describe.
pub const SYSCALL_ENTRY: &str = "x86_syscall";

/// How many arguments of each kind a host-entry wrapper accepts when nothing
/// is known about the function's real signature.
///
/// The wrapper fills *every* argument register of both files and hands back
/// *both* result registers, so it needs no per-function type knowledge at
/// all: a caller sets the slots its function actually reads, leaves the rest
/// at zero, and ignores the half of the result it does not want — exactly as
/// an integer-only caller already ignores the argument registers its function
/// never touches. Arguments past these counts travel on the stack, which is a
/// separate piece of work from carrying floats.
const UNIFORM_WRAPPER_ARGUMENTS: usize = ARGUMENT_REGISTERS.len();
const UNIFORM_WRAPPER_FLOAT_ARGUMENTS: usize = FLOAT_ARGUMENT_REGISTERS.len();

/// Maps input-object symbols onto the wasm symbols that stand for them.
struct SymbolTable<'a> {
    object: &'a ObjectFile,
    functions: HashMap<usize, FunctionReference>,
    /// Functions by where they start, for calls the assembler already
    /// resolved into a bare offset.
    functions_by_location: HashMap<(usize, u64), FunctionReference>,
    /// Slots in the indirect function table, for functions whose address is
    /// taken. Keyed the same way as `functions_by_location`, plus by symbol
    /// for functions another object defines.
    table_slots_by_location: HashMap<(usize, u64), TableReference>,
    table_slots_by_symbol: HashMap<usize, TableReference>,
    /// The lookup that turns a virtual address into a table slot. Present
    /// only for a linked input, which is the only one that needs it.
    exec_map: Option<FunctionReference>,
    /// Data symbols standing for recovered jump tables, by where they begin.
    jump_tables: HashMap<(usize, u64), u32>,
    /// The imported [`UNTRANSLATED`], present only when the translation was
    /// asked to trap rather than refuse.
    untranslated: Option<FunctionReference>,
    /// The imported [`NO_FUNCTION_AT`], present for a linked input.
    no_function_at: Option<FunctionReference>,
    /// The x87 helpers, indexed by [`crate::translate::x87::X87Helper`],
    /// present when anything in the object uses the stack.
    x87_helpers: [Option<FunctionReference>; crate::translate::x87::X87Helper::ALL.len()],
    /// The top of [`X87_STACK`], present alongside the helpers.
    x87_stack: Option<DataReference>,
    /// The module's own [`WIDE_DIVISION`], present when anything divides a
    /// 128-bit dividend.
    wide_division: Option<FunctionReference>,
    /// The imported [`SYSCALL_ENTRY`], present when the object contains a
    /// `syscall` at all.
    syscall_entry: Option<FunctionReference>,
    /// Which data segment each input section became.
    segment_of_section: HashMap<usize, u32>,
    data: HashMap<usize, u32>,
    names: HashMap<usize, String>,
}

impl SymbolResolver for SymbolTable<'_> {
    fn function(&self, elf_symbol: usize, addend: i64) -> Result<FunctionReference> {
        // A defined callee is identified by where it is, which covers both a
        // named function symbol and a section symbol plus an offset.
        if let Some((section, offset)) = self.object.resolve(elf_symbol, addend)
            && self.object.sections[section].role == SectionRole::Text
            && offset >= 0
        {
            return self.function_at(section, offset as u64);
        }

        if addend != 0 {
            bail!(
                "transfer to `{}`+{addend} is into the middle of a function \
                 defined elsewhere, which is out of scope",
                self.name(elf_symbol)
            );
        }
        self.functions.get(&elf_symbol).copied().ok_or_else(|| {
            anyhow::anyhow!(
                "`{}` is referenced as a call target but is not a function symbol",
                self.name(elf_symbol)
            )
        })
    }

    fn function_at(&self, section: usize, offset: u64) -> Result<FunctionReference> {
        self.functions_by_location
            .get(&(section, offset))
            .copied()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no function begins at section {section}+{offset:#x}; a \
                     transfer into the middle of a function is out of scope"
                )
            })
    }

    /// Where control goes when a function runs off its end.
    ///
    /// The function beginning exactly there, or — when the bytes between are
    /// filler — the next one after them. The gap is real and routine: a
    /// linker aligns functions, so `__memcpy_chk` ends at `0xba96d`, a
    /// three-byte `nop` follows, and `memcpy` starts at `0xba970`. glibc
    /// really does fall the first into the second, and asking only about
    /// `0xba96d` finds nothing and emits a trap where the program has a
    /// path.
    ///
    /// The filler check is what keeps this honest rather than merely
    /// permissive: skipping *code* to reach the next function would invent
    /// control flow, and skipping `nop`s reproduces exactly what the
    /// processor does with them.
    fn fall_out_target(&self, section: usize, offset: u64) -> Option<FunctionReference> {
        if let Some(exact) = self.functions_by_location.get(&(section, offset)) {
            return Some(*exact);
        }
        let next = self
            .object
            .functions
            .iter()
            .filter(|function| function.section == section && function.offset > offset)
            .map(|function| function.offset)
            .min()?;
        let bytes = self.object.sections[section]
            .bytes
            .get(offset as usize..next as usize)?;
        crate::discover::is_filler(bytes).then_some(())?;
        self.functions_by_location.get(&(section, next)).copied()
    }

    fn table_slot_at(&self, section: usize, offset: u64) -> Result<TableReference> {
        self.slot_at(section, offset)
    }

    fn jump_table_address(&self, section: usize, offset: u64) -> Result<DataReference> {
        self.jump_tables
            .get(&(section, offset))
            .map(|symbol_index| DataReference {
                symbol_index: *symbol_index,
                addend: 0,
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no symbol was created for the jump table at {}+{offset:#x}",
                    self.object.sections[section].name
                )
            })
    }

    fn linked(&self) -> bool {
        self.object.layout == crate::reader::Layout::Linked
    }

    fn jump_table_at(&self, section: usize, offset: u64) -> Result<u64> {
        Ok(self.object.sections[section].address + offset)
    }

    fn exec_map(&self) -> Result<FunctionReference> {
        self.exec_map.ok_or_else(|| {
            anyhow::anyhow!(
                "an indirect call in a linked input, with no exec map to \
                 turn the address into a table slot"
            )
        })
    }

    fn function_at_address(&self, address: u64) -> Result<FunctionReference> {
        let (section, offset) = self.object.section_at(address).ok_or_else(|| {
            anyhow::anyhow!("a transfer to {address:#x}, which is in no loaded section")
        })?;
        self.function_at(section, offset)
    }

    fn section_address(&self, section: usize) -> u64 {
        self.object.sections[section].address
    }

    fn syscall_entry(&self) -> Result<FunctionReference> {
        self.syscall_entry.ok_or_else(|| {
            anyhow::anyhow!(
                "a `syscall` was translated but `{SYSCALL_ENTRY}` was never \
                 declared; the instruction scan and the import declaration \
                 disagree about what this object contains"
            )
        })
    }

    fn x87_helper(&self, helper: crate::translate::x87::X87Helper) -> Result<FunctionReference> {
        self.x87_helpers[helper.index()].ok_or_else(|| {
            anyhow::anyhow!(
                "an x87 instruction was translated but `{}` was never \
                 declared; the scan and the declaration disagree",
                helper.symbol_name()
            )
        })
    }

    fn wide_division(&self) -> Result<FunctionReference> {
        self.wide_division.ok_or_else(|| {
            anyhow::anyhow!(
                "a 128-bit division was translated but `{WIDE_DIVISION}` was \
                 never declared; the scan and the declaration disagree"
            )
        })
    }

    fn x87_stack(&self) -> Result<DataReference> {
        self.x87_stack.ok_or_else(|| {
            anyhow::anyhow!(
                "an x87 instruction was translated but `{X87_STACK}` was \
                 never reserved; the scan and the declaration disagree"
            )
        })
    }

    fn value(&self, elf_symbol: usize, addend: i64) -> Result<SymbolValue> {
        if let Some(function) = self.table_slot(elf_symbol, addend)? {
            return Ok(SymbolValue::FunctionPointer(function));
        }

        let symbol_index = *self.data.get(&elf_symbol).ok_or_else(|| {
            anyhow::anyhow!(
                "`{}` is referenced as data but has no linear-memory address",
                self.name(elf_symbol)
            )
        })?;
        let addend = i32::try_from(addend).with_context(|| {
            format!(
                "offset {addend} from `{}` does not fit a 32-bit address",
                self.name(elf_symbol)
            )
        })?;
        Ok(SymbolValue::Address(DataReference {
            symbol_index,
            addend,
        }))
    }
}

impl SymbolTable<'_> {
    /// The table slot a reference denotes, when it names a function's
    /// address rather than a place in memory.
    fn slot_at(&self, section: usize, offset: u64) -> Result<TableReference> {
        self.table_slots_by_location
            .get(&(section, offset))
            .copied()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no function begins at {}+{offset:#x}; an address inside a \
                     function has no wasm equivalent",
                    self.object.sections[section].name
                )
            })
    }

    fn table_slot(&self, elf_symbol: usize, addend: i64) -> Result<Option<TableReference>> {
        if let Some(slot) = self.table_slots_by_symbol.get(&elf_symbol) {
            return Ok(Some(*slot));
        }
        let Some((section, offset)) = self.object.resolve(elf_symbol, addend) else {
            return Ok(None);
        };
        if self.object.sections[section].role != SectionRole::Text || offset < 0 {
            return Ok(None);
        }
        self.table_slots_by_location
            .get(&(section, offset as u64))
            .copied()
            .map(Some)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no table slot was reserved for the function at {}+{offset:#x}, \
                     named by `{}`+{addend}",
                    self.object.sections[section].name,
                    self.name(elf_symbol),
                )
            })
    }

    fn name(&self, elf_symbol: usize) -> &str {
        self.names
            .get(&elf_symbol)
            .map(String::as_str)
            .unwrap_or("<unnamed symbol>")
    }
}

/// What the transpiler decided to emit for one input function.
struct FunctionPlan {
    /// Index into [`ObjectFile::functions`].
    input: usize,
    guest: FunctionReference,
    /// Present when the function is visible outside the object and therefore
    /// gets a host-entry wrapper carrying its clean name.
    wrapper: Option<FunctionReference>,
    /// The function's resume body and its table slot, when checkpoint-resume
    /// is on: the dispatcher over the call-split graph, entered at whichever
    /// post-call point a resume ID names.
    resume: Option<FunctionReference>,
    resume_slot: Option<TableReference>,
}

pub struct Transpiler<'a> {
    object: &'a ObjectFile,
    mode: structurer::Mode,
    signatures: SignatureTable,
    promote: bool,
    resume: bool,
    untranslatable: Untranslatable,
}

impl<'a> Transpiler<'a> {
    pub fn new(object: &'a ObjectFile) -> Self {
        Self {
            object,
            mode: structurer::Mode::default(),
            signatures: SignatureTable::new(),
            promote: true,
            resume: false,
            untranslatable: Untranslatable::Refuse,
        }
    }

    /// What to do with a function that cannot be translated. See
    /// [`Untranslatable`]; the default refuses, which is right for anything
    /// written to be translated and wrong for a binary that ships code for
    /// processors this one is not.
    pub fn with_untranslatable(mut self, untranslatable: Untranslatable) -> Self {
        self.untranslatable = untranslatable;
        self
    }

    /// Emits the checkpoint-resume machinery: call sites store resume IDs in
    /// their return-address slots, every function gets a resume body, and a
    /// weak [`RESUME_DRIVER`] rebuilds the suspended frames of a restored
    /// snapshot. Off by default — ordinary output is byte-identical without
    /// it.
    pub fn with_resume(mut self, resume: bool) -> Self {
        self.resume = resume;
        self
    }

    /// Chooses the control-flow translation. Every mode must produce the same
    /// results; running a corpus through more than one is how that is
    /// checked.
    pub fn with_mode(mut self, mode: structurer::Mode) -> Self {
        self.mode = mode;
        self
    }

    /// Supplies signatures for exported functions, so that their host-entry
    /// wrappers carry a real wasm type instead of the uniform shim.
    ///
    /// This is per function, not all-or-nothing: a name in the table gets a
    /// typed face, a name absent from it keeps the zero-information one. An
    /// object can therefore be half interoperable, which is the normal state
    /// of affairs while signatures are being pinned down.
    pub fn with_signatures(mut self, signatures: SignatureTable) -> Self {
        self.signatures = signatures;
        self
    }

    /// Turns register promotion off, leaving every machine-state access on
    /// the globals. The output is slower and the semantics are identical,
    /// which is exactly what makes it worth keeping: a miscompile that
    /// disappears with promotion off is a promotion bug, located.
    pub fn with_promotion(mut self, promote: bool) -> Self {
        self.promote = promote;
        self
    }

    /// The names of function symbols this object references but does not
    /// define.
    ///
    /// This is precisely the set [`Transpiler::transpile`] will import as
    /// `<name>_guest`, computed by the same classification rather than by a
    /// second rule that could drift from it — which is what the thunk object
    /// has to match if it is to satisfy those imports and no others.
    pub fn referenced_undefined_functions(&self) -> Result<Vec<String>> {
        let lifted_functions = lifter::lift_object(self.object)?;
        let references = self.classify_references(&lifted_functions)?;
        Ok(self
            .object
            .symbols
            .iter()
            .enumerate()
            .filter(|(index, symbol)| !symbol.defined && references.is_referenced_function(*index))
            .map(|(_, symbol)| symbol.name.clone())
            .filter(|name| !name.is_empty())
            .collect())
    }

    /// The names of function symbols this object defines and another object
    /// could name — the other half of deciding what is foreign.
    pub fn defined_function_names(&self) -> Vec<String> {
        self.object
            .functions
            .iter()
            .filter(|function| {
                function
                    .symbol
                    .is_some_and(|index| self.object.symbols[index].binding != SymbolBinding::Local)
            })
            .map(|function| function.name.clone())
            .collect()
    }

    /// Translates the whole object, returning the serialized relocatable wasm
    /// object.
    pub fn transpile(&self) -> Result<Vec<u8>> {
        Ok(self.translate()?.module)
    }

    /// The same, with the image patches a linked input needs.
    pub fn translate(&self) -> Result<Translation> {
        let lifted_functions = lifter::lift_object(self.object)?;
        let references = self.classify_references(&lifted_functions)?;

        let mut wasm = WasmObject::new();
        let machine = MachineState::define(&mut wasm);

        let guest_type = wasm.intern_type(FunctionType {
            parameters: vec![],
            results: vec![],
        });
        let mut symbols = SymbolTable {
            object: self.object,
            functions: HashMap::new(),
            functions_by_location: HashMap::new(),
            table_slots_by_location: HashMap::new(),
            table_slots_by_symbol: HashMap::new(),
            exec_map: None,
            untranslated: None,
            no_function_at: None,
            x87_helpers: [None; crate::translate::x87::X87Helper::ALL.len()],
            x87_stack: None,
            wide_division: None,
            jump_tables: HashMap::new(),
            syscall_entry: None,
            segment_of_section: HashMap::new(),
            data: HashMap::new(),
            names: HashMap::new(),
        };
        for (index, symbol) in self.object.symbols.iter().enumerate() {
            if !symbol.name.is_empty() {
                symbols.names.insert(index, symbol.name.clone());
            }
        }

        // The resume bodies' shared type: the entry index in, the resume ID
        // of the frame above out. Interned only when the machinery is on, so
        // ordinary output is byte-identical without it.
        let resume_type = self.resume.then(|| {
            wasm.intern_type(FunctionType {
                parameters: vec![ValueType::I32],
                results: vec![ValueType::I64],
            })
        });

        // Imports first: they occupy the low end of the function index space,
        // so every undefined callee has to be known before any definition
        // takes an index.
        let report_type = wasm.intern_type(FunctionType {
            parameters: vec![ValueType::I32, ValueType::I32],
            results: vec![],
        });
        let address_type = wasm.intern_type(FunctionType {
            parameters: vec![ValueType::I64],
            results: vec![],
        });
        self.declare_imported_functions(
            &mut wasm,
            &mut symbols,
            guest_type,
            report_type,
            address_type,
            &references,
        )?;
        self.declare_data(&mut wasm, &mut symbols, &references)?;
        let (mut plans, driver, exec_map, wide_division) =
            self.declare_functions(&mut wasm, &mut symbols, &references)?;
        symbols.exec_map = exec_map;
        symbols.wide_division = wide_division;

        // Table slots have to exist before anything can refer to one, and
        // they are only known once every function has a symbol.
        self.assign_table_slots(&mut wasm, &mut symbols, &references, &mut plans);
        // The static exec map, which only a linked input needs: a function
        // pointer there is a virtual address, and this is what turns one
        // into a slot.
        let exec_map_body = exec_map.map(|_| {
            let lookup_type = wasm.intern_type(FunctionType {
                parameters: vec![ValueType::I64],
                results: vec![ValueType::I32],
            });
            let mut entries: Vec<(u64, TableReference)> = symbols
                .table_slots_by_location
                .iter()
                .map(|((section, offset), slot)| {
                    (self.object.sections[*section].address + offset, *slot)
                })
                .collect();
            // Sorted, because the lookup searches in halves.
            entries.sort_unstable_by_key(|(address, _)| *address);
            let table = build_exec_map_table(&mut wasm, &entries);
            (
                lookup_type,
                build_exec_map_lookup(
                    table,
                    entries.len() as u32,
                    symbols
                        .no_function_at
                        .expect("a linked input declares the reporter"),
                ),
            )
        });
        wasm.uses_function_table = references.calls_indirectly || !wasm.table_functions.is_empty();
        self.rewrite_jump_tables(&mut wasm, &mut symbols, &references)?;
        self.translate_data_relocations(&mut wasm, &symbols, &references)?;

        // Bodies are built against the finished symbol table, then handed to
        // the object in the order the indices were reserved.
        let mut bodies: Vec<(u32, u32, FunctionBody)> = Vec::new();
        let mut refused: Vec<Refusal> = Vec::new();
        for plan in &plans {
            let lifted = &lifted_functions[plan.input];

            // With resume on, both of the function's bodies are built against
            // the same call-split graph and site map: the ordinary body
            // stores the IDs, the resume body is what they name.
            let resume_context = match (plan.resume, plan.resume_slot) {
                (Some(_), Some(slot)) => {
                    let graph = ControlFlowGraph::build_resumable(lifted)
                        .with_context(|| format!("splitting `{}` at its calls", lifted.name))?;
                    let entries = resume_entries(lifted, &graph)
                        .with_context(|| format!("mapping `{}`'s resume points", lifted.name))?;
                    Some((graph, slot, entries))
                }
                _ => None,
            };

            let translated = self
                .translate_guest_function(&symbols, &machine, lifted, guest_type, &resume_context)
                .with_context(|| format!("translating function `{}`", lifted.name));
            let body = match translated {
                Ok(body) => body,
                Err(error) if self.untranslatable == Untranslatable::Trap => {
                    refused.push(Refusal {
                        name: lifted.name.clone(),
                        witness: self.object.functions[lifted.function].witness,
                        reason: format!("{error:#}"),
                    });
                    build_refusal(
                        &mut wasm,
                        &lifted.name,
                        symbols
                            .untranslated
                            .expect("the reporter is declared whenever trapping is asked for"),
                    )
                }
                Err(error) => return Err(error),
            };
            bodies.push((plan.guest.function_index, guest_type, body));

            if let (Some(resume), Some((graph, slot, entries))) = (plan.resume, &resume_context) {
                let resume_body = self
                    .translate_resume_body(
                        &symbols, &machine, lifted, guest_type, graph, *slot, entries,
                    )
                    .with_context(|| format!("translating `{}`'s resume body", lifted.name))?;
                bodies.push((
                    resume.function_index,
                    resume_type.expect("resume bodies exist only when the type does"),
                    resume_body,
                ));
            }

            if let Some(wrapper) = plan.wrapper {
                let name = &self.object.functions[plan.input].name;
                let (type_index, body) = match self.signatures.get(name) {
                    Some(signature) => {
                        let type_index = wasm.intern_type(typed_wrapper_type(signature)?);
                        let body = self
                            .build_typed_entry_wrapper(&machine, plan.guest, signature)
                            .with_context(|| {
                                format!("building the typed entry wrapper for `{name}`")
                            })?;
                        (type_index, body)
                    }
                    None => (
                        wasm.intern_type(uniform_wrapper_type()),
                        self.build_host_entry_wrapper(&machine, plan.guest),
                    ),
                };
                bodies.push((wrapper.function_index, type_index, body));
            }
        }

        if let Some(driver) = driver {
            bodies.push((
                driver.function_index,
                guest_type,
                build_resume_driver(
                    &machine,
                    resume_type.expect("the driver exists only when the type does"),
                ),
            ));
        }

        if let (Some(reference), Some((lookup_type, body))) = (exec_map, exec_map_body) {
            bodies.push((reference.function_index, lookup_type, body));
        }

        if let Some(reference) = wide_division {
            let type_index = wasm.intern_type(wide_division_type());
            bodies.push((reference.function_index, type_index, build_wide_division()));
        }

        bodies.sort_by_key(|(index, _, _)| *index);
        for (index, type_index, body) in bodies {
            debug_assert_eq!(index, wasm.next_defined_function_index());
            wasm.defined_functions
                .push(DefinedFunction { type_index, body });
        }

        Ok(Translation {
            module: wasm.serialize(),
            patches: self.image_patches(&references)?,
            refused,
        })
    }

    /// The jump-table rewrites a linked image needs, as bytes at addresses.
    ///
    /// Every entry is made to hold whatever makes the dispatch's own
    /// arithmetic arrive at `table + arm`, which is what the `br_table`
    /// subtracts the table's address back out of: the arm number for a table
    /// of differences from itself, the table's address plus the arm for a
    /// table of whole addresses, and the distance between the two bases plus
    /// the arm for a computed goto measuring from a code label.
    fn image_patches(&self, references: &TextReferences) -> Result<Vec<Patch>> {
        if self.object.layout != crate::reader::Layout::Linked {
            return Ok(Vec::new());
        }
        let mut patches = Vec::new();
        for table in &references.tables {
            let table_address =
                self.object.sections[table.table_section].address + table.table_offset;
            for (arm, (_, entry_offset)) in table.entries().enumerate() {
                // Whatever the dispatch's own arithmetic is, it has to end up
                // computing `origin + index`, because that is what the
                // `br_table` subtracts the origin back out of. The guest adds
                // the entry to the base — or takes it whole, where there is
                // no base — so the entry is whatever makes that come out
                // right.
                //
                // `index` is the arm's place in the *origin's* arm space
                // rather than in this table, which is what lets several
                // tables feed one merged dispatch; see
                // `jump_table::share_arm_spaces`.
                let index = table.arm_offset + arm as u64;
                let value = table
                    .origin
                    .wrapping_add(index)
                    .wrapping_sub(table.base.unwrap_or(0));
                let width = table.stride as usize;
                // The guest sign-extends a narrow entry before adding it to
                // the base, so a difference that does not fit the entry is
                // not a difference this rewrite can express — and writing
                // the low bytes of one would dispatch somewhere arbitrary
                // rather than fail. Refused instead, naming the table.
                if width < 8 {
                    let bits = width as u32 * 8;
                    let signed = value as i64;
                    let low = -(1i64 << (bits - 1));
                    let high = (1i64 << (bits - 1)) - 1;
                    if signed < low || signed > high {
                        bail!(
                            "the jump table at {table_address:#x} measures its \
                             entries from {:#x}, which is {} away — too far to \
                             write into a {width}-byte entry",
                            table.base.unwrap_or(table_address),
                            signed.abs()
                        );
                    }
                }
                patches.push(Patch {
                    address: self.object.sections[table.table_section].address + entry_offset,
                    bytes: value.to_le_bytes()[..width].to_vec(),
                });
            }
        }
        Ok(patches)
    }

    /// Works out what every relocation in the object means.
    ///
    /// A reference into a text section is either a call, or a function
    /// pointer, or an entry of a jump table the lifter already consumed.
    /// Anything else pointing into text — the middle of a function, reached
    /// some other way — has no wasm equivalent and is reported rather than
    /// guessed at.
    fn classify_references(&self, functions: &[LiftedFunction]) -> Result<TextReferences> {
        use iced_x86::FlowControl;
        let mut references = TextReferences::default();

        for function in functions {
            for table in function.jump_tables.values() {
                references.consumed_entries.extend(table.entries());
                references.tables.push(table.clone());
            }

            for (position, lifted) in function.instructions.iter().enumerate() {
                // A conditional branch counts too: at higher optimisation
                // levels a cold path is split into its own section and
                // reached by one, which makes it a conditional tail call.
                let transfers_control = matches!(
                    lifted.instruction.flow_control(),
                    FlowControl::Call
                        | FlowControl::UnconditionalBranch
                        | FlowControl::ConditionalBranch
                );
                // A call through a global offset table slot names its callee
                // in the displacement rather than the immediate.
                let names_callee = lifted.immediate.filter(|_| transfers_control).or_else(|| {
                    lifted
                        .displacement
                        .filter(|reference| reference.via_global_offset_table && transfers_control)
                });
                if let Some(reference) = names_callee {
                    references.call_targets.insert(reference.symbol);
                    continue;
                }

                // The address computation of a recovered dispatch names the
                // table, which is data; nothing else about it survives.
                if function.jump_tables.contains_key(&position) {
                    continue;
                }
                if crate::translate::x87::is_x87_mnemonic(lifted.instruction.mnemonic()) {
                    references.uses_x87 = true;
                }
                if crate::translate::divides_wide(&lifted.instruction) {
                    references.divides_wide = true;
                }
                if lifted.instruction.mnemonic() == iced_x86::Mnemonic::Syscall {
                    references.issues_syscalls = true;
                    continue;
                }
                if matches!(
                    lifted.instruction.flow_control(),
                    FlowControl::IndirectCall | FlowControl::IndirectBranch
                ) {
                    references.calls_indirectly = true;
                }

                if lifted.displacement.is_none()
                    && lifted.instruction.memory_base() == iced_x86::Register::RIP
                    // Relocatable only. There, a program-counter-relative
                    // operand with no relocation means the assembler
                    // resolved it against this very section, which it can
                    // only do for something in it — so it names a function.
                    // In a linked object *nothing* has a relocation and the
                    // operand is an address like any other, naming data as
                    // readily as code; it is emitted as the constant it is,
                    // and a function's address reaches a call through the
                    // exec map rather than through a slot recorded here.
                    && self.object.layout != crate::reader::Layout::Linked
                {
                    let target = lifted.instruction.memory_displacement64();
                    if !self.begins_a_function(function.section, target) {
                        bail!(
                            "`{}` at {:#x} points {target:#x} bytes into {}, \
                             which is not the start of a function; an address \
                             inside a function has no wasm equivalent",
                            crate::translate::render(&lifted.instruction),
                            lifted.offset,
                            self.object.sections[function.section].name
                        );
                    }
                    references
                        .addressed_locations
                        .insert((function.section, target));
                    continue;
                }

                for reference in [lifted.displacement, lifted.immediate]
                    .into_iter()
                    .flatten()
                {
                    self.record_reference(&mut references, reference.symbol, reference.addend)?;
                }
            }
        }

        for (index, section) in self.object.sections.iter().enumerate() {
            if !section.role.is_data() {
                continue;
            }
            for relocation in &section.relocations {
                if references
                    .consumed_entries
                    .contains(&(index, relocation.offset))
                {
                    continue;
                }
                self.record_reference(&mut references, relocation.symbol, relocation.addend)?;
            }
        }

        Ok(references)
    }

    /// Records one reference, if it points at a function.
    fn record_reference(
        &self,
        references: &mut TextReferences,
        symbol: usize,
        addend: i64,
    ) -> Result<()> {
        let elf_symbol = &self.object.symbols[symbol];
        match self.object.resolve(symbol, addend) {
            Some((section, offset)) => {
                if self.object.sections[section].role != SectionRole::Text {
                    return Ok(());
                }
                if offset < 0 || !self.begins_a_function(section, offset as u64) {
                    bail!(
                        "`{}`+{addend} points {offset:#x} bytes into {}, which is \
                         not the start of a function; an address inside a \
                         function has no wasm equivalent",
                        elf_symbol.name,
                        self.object.sections[section].name
                    );
                }
                references
                    .addressed_locations
                    .insert((section, offset as u64));
            }
            // An undefined symbol whose address is taken. ELF cannot say
            // whether it is a function or data, so it is taken to be a
            // function only when the symbol table says so; otherwise it is
            // data, and a wrong guess fails at link time rather than
            // silently.
            None if elf_symbol.role == SymbolRole::Function => {
                references.addressed_symbols.insert(symbol);
            }
            None => {}
        }
        Ok(())
    }

    fn begins_a_function(&self, section: usize, offset: u64) -> bool {
        self.object
            .functions
            .iter()
            .any(|function| function.section == section && function.offset == offset)
    }

    /// Gives every address-taken function a slot in the indirect function
    /// table, and records the slots for the emitter's element segment.
    fn assign_table_slots(
        &self,
        wasm: &mut WasmObject,
        symbols: &mut SymbolTable<'_>,
        references: &TextReferences,
        plans: &mut [FunctionPlan],
    ) {
        let mut next_slot = FIRST_TABLE_INDEX;
        let mut claim = |function: FunctionReference| {
            let slot = TableReference {
                symbol_index: function.symbol_index,
                table_index: next_slot,
            };
            next_slot += 1;
            slot
        };

        // In a linked executable there are no relocations, so nothing says
        // which functions have their address taken — every one of them
        // might, and the evidence that would narrow it was consumed by the
        // linker. So all of them get a slot, and the exec map below is how
        // an address finds one.
        let addressed: Vec<(usize, u64)> = if self.object.layout == crate::reader::Layout::Linked {
            self.object
                .functions
                .iter()
                .map(|function| (function.section, function.offset))
                .collect()
        } else {
            references.addressed_locations.iter().copied().collect()
        };
        for location in &addressed {
            let Some(function) = symbols.functions_by_location.get(location).copied() else {
                continue;
            };
            let slot = claim(function);
            symbols.table_slots_by_location.insert(*location, slot);
            wasm.table_functions.push(function.function_index);
        }
        for symbol in &references.addressed_symbols {
            let Some(function) = symbols.functions.get(symbol).copied() else {
                continue;
            };
            let slot = claim(function);
            symbols.table_slots_by_symbol.insert(*symbol, slot);
            wasm.table_functions.push(function.function_index);
        }

        // Resume bodies are reached through the table too: a resume ID's low
        // half is the slot claimed here.
        for plan in plans {
            let Some(resume) = plan.resume else { continue };
            plan.resume_slot = Some(claim(resume));
            wasm.table_functions.push(resume.function_index);
        }
    }

    /// Undefined function symbols become imports of the callee's *guest*
    /// entry point, since that is the convention both sides of a
    /// transpiled-to-transpiled call speak.
    fn declare_imported_functions(
        &self,
        wasm: &mut WasmObject,
        symbols: &mut SymbolTable<'_>,
        guest_type: u32,
        report_type: u32,
        address_type: u32,
        references: &TextReferences,
    ) -> Result<()> {
        for (index, symbol) in self.object.symbols.iter().enumerate() {
            if symbol.defined || !references.is_referenced_function(index) {
                continue;
            }
            let name = format!("{}{GUEST_SUFFIX}", symbol.name);
            let function_index = wasm.imported_functions.len() as u32;
            wasm.imported_functions.push(ImportedFunction {
                module: ENVIRONMENT_MODULE.to_string(),
                field: name.clone(),
                type_index: guest_type,
            });
            let symbol_index = wasm.add_symbol(Symbol {
                name,
                target: SymbolTarget::Function(function_index),
                flags: symbol_flags::UNDEFINED,
            });
            symbols.functions.insert(
                index,
                FunctionReference {
                    symbol_index,
                    function_index,
                },
            );
        }

        // Declared whether or not anything ends up refused: imports occupy
        // the low end of the function index space, so one that appears
        // partway through building bodies would shift every definition's
        // index out from under the plans.
        if self.untranslatable == Untranslatable::Trap {
            let function_index = wasm.imported_functions.len() as u32;
            wasm.imported_functions.push(ImportedFunction {
                module: ENVIRONMENT_MODULE.to_string(),
                field: UNTRANSLATED.to_string(),
                type_index: report_type,
            });
            let symbol_index = wasm.add_symbol(Symbol {
                name: UNTRANSLATED.to_string(),
                target: SymbolTarget::Function(function_index),
                flags: symbol_flags::UNDEFINED,
            });
            symbols.untranslated = Some(FunctionReference {
                symbol_index,
                function_index,
            });
        }

        // Every x87 helper, when anything uses the stack at all.
        //
        // All of them rather than the ones actually reached: per-helper
        // tracking would need a second scan that can drift from the first,
        // and `wasm-ld` resolves only what is called, so an unused import
        // costs a line in a section nothing reads.
        if references.uses_x87 {
            symbols.x87_stack = Some(define_x87_stack(wasm));
            for helper in crate::translate::x87::X87Helper::ALL {
                let function_index = wasm.imported_functions.len() as u32;
                let type_index = wasm.intern_type(helper.signature());
                wasm.imported_functions.push(ImportedFunction {
                    module: ENVIRONMENT_MODULE.to_string(),
                    field: helper.symbol_name().to_string(),
                    type_index,
                });
                let symbol_index = wasm.add_symbol(Symbol {
                    name: helper.symbol_name().to_string(),
                    target: SymbolTarget::Function(function_index),
                    flags: symbol_flags::UNDEFINED,
                });
                symbols.x87_helpers[helper.index()] = Some(FunctionReference {
                    symbol_index,
                    function_index,
                });
            }
        }

        // The exec map's own failure, which only a linked input can have.
        if self.object.layout == crate::reader::Layout::Linked {
            let function_index = wasm.imported_functions.len() as u32;
            wasm.imported_functions.push(ImportedFunction {
                module: ENVIRONMENT_MODULE.to_string(),
                field: NO_FUNCTION_AT.to_string(),
                type_index: address_type,
            });
            let symbol_index = wasm.add_symbol(Symbol {
                name: NO_FUNCTION_AT.to_string(),
                target: SymbolTarget::Function(function_index),
                flags: symbol_flags::UNDEFINED,
            });
            symbols.no_function_at = Some(FunctionReference {
                symbol_index,
                function_index,
            });
        }

        // The seam is an import like any other undefined callee, but it is
        // named by us rather than by a relocation: `syscall` has no operand.
        if references.issues_syscalls {
            let function_index = wasm.imported_functions.len() as u32;
            wasm.imported_functions.push(ImportedFunction {
                module: ENVIRONMENT_MODULE.to_string(),
                field: SYSCALL_ENTRY.to_string(),
                type_index: guest_type,
            });
            let symbol_index = wasm.add_symbol(Symbol {
                name: SYSCALL_ENTRY.to_string(),
                target: SymbolTarget::Function(function_index),
                flags: symbol_flags::UNDEFINED,
            });
            symbols.syscall_entry = Some(FunctionReference {
                symbol_index,
                function_index,
            });
        }
        Ok(())
    }

    /// Turns every data-carrying input section into one wasm data segment,
    /// which keeps intra-section offsets — including references past the end
    /// of any single symbol — exactly as the input had them.
    fn declare_data(
        &self,
        wasm: &mut WasmObject,
        symbols: &mut SymbolTable<'_>,
        references: &TextReferences,
    ) -> Result<()> {
        if self.object.layout == crate::reader::Layout::Linked {
            // A linked executable's data reaches the guest through the
            // loader, which copies each `PT_LOAD` segment to its own virtual
            // address. Carrying it as module data segments as well would put
            // a second copy at an address `wasm-ld` chose, and every operand
            // in the program points at the first one. It would also drag in
            // the boundary symbols a linker leaves behind — `__bss_start`,
            // `_end` — which point *past* their section and are markers
            // rather than objects.
            return Ok(());
        }
        for (index, section) in self.object.sections.iter().enumerate() {
            if !section.role.is_data() || section.size == 0 {
                continue;
            }
            let bytes = match section.role {
                // Wasm has no zero-fill section; `wasm-ld` recognises the
                // `.bss` name and keeps these bytes out of the linked module.
                SectionRole::ZeroFilled => vec![0; section.size as usize],
                _ => section.bytes.clone(),
            };
            let segment_index = wasm.data_segments.len() as u32;
            wasm.data_segments.push(DataSegment {
                name: section.name.clone(),
                alignment_log2: section.alignment.trailing_zeros(),
                bytes,
                relocations: Vec::new(),
            });
            symbols.segment_of_section.insert(index, segment_index);
        }

        for (index, symbol) in self.object.symbols.iter().enumerate() {
            // Anything defined inside a section that became a data segment has
            // a linear-memory address, whatever the ELF symbol table calls it.
            // Floating-point literal pools arrive as untyped local labels
            // (`.LC0` and friends) with no size, which is neither `STT_OBJECT`
            // nor anything else the type field distinguishes.
            if symbol.role == SymbolRole::Function || !symbol.defined {
                continue;
            }
            let Some(section) = symbol.section else {
                continue;
            };
            let Some(&segment_index) = symbols.segment_of_section.get(&section) else {
                continue;
            };
            let segment_size = wasm.data_segments[segment_index as usize].bytes.len() as u32;
            let size = if symbol.role == SymbolRole::Section {
                // A section symbol stands for the whole section; relocations
                // against it carry the offset as an addend.
                segment_size
            } else {
                symbol.size as u32
            };
            let name = wasm_data_symbol_name(&self.object.sections[section].name, symbol);
            let symbol_index = wasm.add_symbol(Symbol {
                name,
                target: SymbolTarget::Data(Some(DataSymbolLocation {
                    segment_index,
                    offset: symbol.offset as u32,
                    size,
                })),
                flags: data_symbol_flags(symbol),
            });
            symbols.data.insert(index, symbol_index);
        }

        // A symbol another object defines still needs an entry, so that a
        // relocation naming it has something to point at.
        for (index, symbol) in self.object.symbols.iter().enumerate() {
            if symbol.defined || references.is_referenced_function(index) || symbol.name.is_empty()
            {
                continue;
            }
            if !self.is_referenced(index) {
                continue;
            }
            if symbol.role == SymbolRole::Function {
                bail!(
                    "`{}` is an undefined function referenced as data; taking \
                     the address of a function needs table indices, which are \
                     out of scope",
                    symbol.name
                );
            }
            let symbol_index = wasm.add_symbol(Symbol {
                name: symbol.name.clone(),
                target: SymbolTarget::Data(None),
                flags: symbol_flags::UNDEFINED,
            });
            symbols.data.insert(index, symbol_index);
        }

        Ok(())
    }

    fn is_referenced(&self, elf_symbol: usize) -> bool {
        self.object.sections.iter().any(|section| {
            section
                .relocations
                .iter()
                .any(|relocation| relocation.symbol == elf_symbol)
        })
    }

    /// Replaces every jump table's contents with the arm indices a `br_table`
    /// needs.
    ///
    /// Entry `k` is made to hold `k` where the guest adds the table's address
    /// back, and `table + k` where it jumps to the entry directly — so that
    /// in both cases the address the dispatch computes is the table's plus the
    /// arm's index, whatever shape the arithmetic took. The `table + k` form
    /// is a relocation against the table's own symbol; the plain `k` form is
    /// just a number, and needs none.
    fn rewrite_jump_tables(
        &self,
        wasm: &mut WasmObject,
        symbols: &mut SymbolTable<'_>,
        references: &TextReferences,
    ) -> Result<()> {
        for table in &references.tables {
            if self.object.layout == crate::reader::Layout::Linked {
                // Nothing to rewrite here: the bytes the guest reads come
                // from the image the loader copies, so the rewrite is a
                // patch to that image. It is a constant either way — the
                // table's virtual address is known now, where in relocatable
                // mode only the linker knows where the table lands.
                continue;
            }
            let Some(&segment_index) = symbols.segment_of_section.get(&table.table_section) else {
                bail!(
                    "the jump table in {} is not in a section that became a \
                     data segment",
                    self.object.sections[table.table_section].name
                );
            };
            let section_name = &self.object.sections[table.table_section].name;
            let symbol_index = wasm.add_symbol(Symbol {
                name: format!("{section_name}.switch.{:#x}", table.table_offset),
                target: SymbolTarget::Data(Some(DataSymbolLocation {
                    segment_index,
                    offset: table.table_offset as u32,
                    size: table.byte_length() as u32,
                })),
                // A table is private to the object that dispatches through it.
                flags: symbol_flags::LOCAL,
            });
            symbols
                .jump_tables
                .insert((table.table_section, table.table_offset), symbol_index);

            let segment = &mut wasm.data_segments[segment_index as usize];
            for (arm, (_, entry_offset)) in table.entries().enumerate() {
                let start = entry_offset as usize;
                let end = start + table.stride as usize;
                segment.bytes[start..end].fill(0);
                let index = table.arm_offset + arm as u64;
                if table.relative() {
                    // A relocatable input's relative entries are always
                    // differences from the table itself, so the difference to
                    // write is just the index.
                    let value = index.to_le_bytes();
                    let width = table.stride as usize;
                    segment.bytes[start..end].copy_from_slice(&value[..width]);
                } else {
                    segment.relocations.push(WasmRelocation {
                        kind: WasmRelocationKind::MemoryAddressI32,
                        offset: entry_offset as u32,
                        symbol_index,
                        addend: index as i32,
                    });
                }
            }
        }
        Ok(())
    }

    /// Turns relocations that live inside data into their wasm equivalents.
    ///
    /// A pointer stored in data becomes a four-byte linear-memory address.
    /// An eight-byte ELF pointer keeps its width in the segment — the low
    /// half is patched and the high half stays zero, which is what a 32-bit
    /// address space makes of a 64-bit pointer.
    fn translate_data_relocations(
        &self,
        wasm: &mut WasmObject,
        symbols: &SymbolTable<'_>,
        references: &TextReferences,
    ) -> Result<()> {
        for (index, section) in self.object.sections.iter().enumerate() {
            let Some(&segment_index) = symbols.segment_of_section.get(&index) else {
                continue;
            };
            for relocation in &section.relocations {
                // A jump table's entries were consumed when the dispatch that
                // read them became a `br_table`; nothing loads them now, and
                // they name code, which has no address to relocate to.
                if references
                    .consumed_entries
                    .contains(&(index, relocation.offset))
                {
                    continue;
                }
                if relocation.kind.is_program_counter_relative() {
                    bail!(
                        "{}+{:#x} holds a program-counter-relative relocation; \
                         data has no program counter to be relative to",
                        section.name,
                        relocation.offset
                    );
                }

                let (kind, symbol_index, addend) =
                    match symbols.value(relocation.symbol, relocation.addend)? {
                        SymbolValue::Address(data) => (
                            WasmRelocationKind::MemoryAddressI32,
                            data.symbol_index,
                            data.addend,
                        ),
                        // A function pointer held in data: the slot number
                        // goes in, not an address.
                        SymbolValue::FunctionPointer(function) => {
                            (WasmRelocationKind::TableIndexI32, function.symbol_index, 0)
                        }
                    };

                let segment = &mut wasm.data_segments[segment_index as usize];
                let offset = relocation.offset as usize;
                let width = relocation.kind.width() as usize;

                // Only the low four bytes carry the value; anything the input
                // left above them would survive into the linked module as a
                // corrupt pointer.
                if segment.bytes[offset + 4..offset + width]
                    .iter()
                    .any(|byte| *byte != 0)
                {
                    bail!(
                        "{}+{:#x} holds a pointer whose high half is not zero; \
                         it cannot be represented in a 32-bit address space",
                        section.name,
                        relocation.offset
                    );
                }

                segment.relocations.push(WasmRelocation {
                    kind,
                    offset: relocation.offset as u32,
                    symbol_index,
                    addend,
                });
            }
        }
        Ok(())
    }

    fn declare_functions(
        &self,
        wasm: &mut WasmObject,
        symbols: &mut SymbolTable<'_>,
        references: &TextReferences,
    ) -> Result<(
        Vec<FunctionPlan>,
        Option<FunctionReference>,
        Option<FunctionReference>,
        Option<FunctionReference>,
    )> {
        let mut plans = Vec::new();
        let mut next_index = wasm.imported_functions.len() as u32;

        for (input, function) in self.object.functions.iter().enumerate() {
            let elf_symbol = function.symbol.map(|index| &self.object.symbols[index]);

            let guest_index = next_index;
            next_index += 1;
            let guest_symbol = wasm.add_symbol(Symbol {
                name: format!("{}{GUEST_SUFFIX}", function.name),
                target: SymbolTarget::Function(guest_index),
                flags: guest_symbol_flags(elf_symbol),
            });
            let guest = FunctionReference {
                symbol_index: guest_symbol,
                function_index: guest_index,
            };
            if let Some(index) = function.symbol {
                symbols.functions.insert(index, guest);
            }
            symbols
                .functions_by_location
                .insert((function.section, function.offset), guest);

            // A function nothing outside the object can name needs no host
            // entry point.
            let wrapper = match elf_symbol {
                Some(named) if named.binding != SymbolBinding::Local => {
                    let wrapper_index = next_index;
                    next_index += 1;
                    let wrapper_symbol = wasm.add_symbol(Symbol {
                        name: function.name.clone(),
                        target: SymbolTarget::Function(wrapper_index),
                        flags: wrapper_symbol_flags(named),
                    });
                    Some(FunctionReference {
                        symbol_index: wrapper_symbol,
                        function_index: wrapper_index,
                    })
                }
                // Local, or named by nothing at all: either way no host
                // entry point, because nothing outside could ask for one.
                _ => None,
            };

            let resume = if self.resume {
                let resume_index = next_index;
                next_index += 1;
                let resume_symbol = wasm.add_symbol(Symbol {
                    name: format!("{}{GUEST_SUFFIX}.resume", function.name),
                    target: SymbolTarget::Function(resume_index),
                    // Resume bodies are reached through the function table by
                    // resume IDs this same object wrote; no other object ever
                    // names one.
                    flags: symbol_flags::LOCAL,
                });
                Some(FunctionReference {
                    symbol_index: resume_symbol,
                    function_index: resume_index,
                })
            } else {
                None
            };

            plans.push(FunctionPlan {
                input,
                guest,
                wrapper,
                resume,
                resume_slot: None,
            });
        }

        let driver = if self.resume {
            let driver_index = next_index;
            let driver_symbol = wasm.add_symbol(Symbol {
                name: RESUME_DRIVER.to_string(),
                target: SymbolTarget::Function(driver_index),
                flags: symbol_flags::WEAK,
            });
            Some(FunctionReference {
                symbol_index: driver_symbol,
                function_index: driver_index,
            })
        } else {
            None
        };
        if driver.is_some() {
            next_index += 1;
        }

        // The exec map's lookup takes an index here too, so that every
        // definition's index is reserved in one place and the bodies can be
        // pushed in the order they were claimed.
        // Visible across the link, not local: it is how the boot path turns
        // the entry point's address into the table slot it enters through,
        // and in linked mode there is no other name for any of the
        // program's own functions.
        let exec_map = (self.object.layout == crate::reader::Layout::Linked).then(|| {
            let index = next_index;
            let symbol = wasm.add_symbol(Symbol {
                name: EXEC_MAP_LOOKUP.to_string(),
                target: SymbolTarget::Function(index),
                flags: 0,
            });
            FunctionReference {
                symbol_index: symbol,
                function_index: index,
            }
        });
        if exec_map.is_some() {
            next_index += 1;
        }

        // The wide-division helper, for the same reason and in the same
        // place: its index is reserved here so the bodies can be pushed in
        // the order the indices were claimed.
        let wide_division = references.divides_wide.then(|| {
            let index = next_index;
            let symbol = wasm.add_symbol(Symbol {
                name: WIDE_DIVISION.to_string(),
                target: SymbolTarget::Function(index),
                flags: symbol_flags::LOCAL,
            });
            FunctionReference {
                symbol_index: symbol,
                function_index: index,
            }
        });
        Ok((plans, driver, exec_map, wide_division))
    }

    fn translate_guest_function(
        &self,
        symbols: &SymbolTable<'_>,
        machine: &MachineState,
        lifted: &LiftedFunction,
        guest_type: u32,
        resume: &Option<(ControlFlowGraph, TableReference, HashMap<u64, u32>)>,
    ) -> Result<FunctionBody> {
        let mut body = FunctionBodyBuilder::new(0);
        let mut translator = FunctionTranslator::new(symbols, machine, lifted.section, guest_type);
        if let Some((_, slot, entries)) = resume {
            translator.enable_resume(ResumeSites {
                table_slot: *slot,
                entries: entries.clone(),
            });
        }
        if self.promote {
            translator.begin_function(&mut body, lifted);
        }
        structurer::translate_function(&mut body, &mut translator, lifted, self.mode)?;
        Ok(body.finish())
    }

    /// The function's second body: entered by the resume driver at whichever
    /// post-call point the popped resume ID names, running the frame to its
    /// return and yielding the next frame's ID. Always the dispatcher shape —
    /// the entry parameter is a block index, which structured control flow
    /// has no way to jump to.
    #[allow(clippy::too_many_arguments)]
    fn translate_resume_body(
        &self,
        symbols: &SymbolTable<'_>,
        machine: &MachineState,
        lifted: &LiftedFunction,
        guest_type: u32,
        graph: &ControlFlowGraph,
        slot: TableReference,
        entries: &HashMap<u64, u32>,
    ) -> Result<FunctionBody> {
        let mut body = FunctionBodyBuilder::new(1);
        let mut translator = FunctionTranslator::new(symbols, machine, lifted.section, guest_type);
        // Fresh calls made by a resumed frame reserve slots like any others,
        // so a checkpoint taken under a resumed frame resumes too.
        translator.enable_resume(ResumeSites {
            table_slot: slot,
            entries: entries.clone(),
        });
        translator.yield_next_site_on_return();
        if self.promote {
            translator.begin_function(&mut body, lifted);
        }
        structurer::translate_resume_function(&mut body, &mut translator, lifted, graph, 0)?;
        Ok(body.finish())
    }

    /// The host entry point: the uniform zero-information shim the design
    /// falls back to when no signature is known. Arguments land in the SysV
    /// argument registers of both files, the guest stack is started from the
    /// linker's `__stack_pointer`, and `rax` and `xmm0` come back as the two
    /// results.
    ///
    /// A `float` argument occupies only the low half of its XMM register, so
    /// the caller supplies its four bytes in the low half of the `f64` it
    /// passes; a `float` result comes back the same way. That is what SysV
    /// does with the register, and reinterpreting rather than converting is
    /// what keeps it exact.
    fn build_host_entry_wrapper(
        &self,
        machine: &MachineState,
        guest: FunctionReference,
    ) -> FunctionBody {
        let parameter_count = UNIFORM_WRAPPER_ARGUMENTS + UNIFORM_WRAPPER_FLOAT_ARGUMENTS;
        let mut body = FunctionBodyBuilder::new(parameter_count as u32);

        for (parameter, register) in ARGUMENT_REGISTERS.iter().enumerate() {
            body.local_get(parameter as u32);
            body.global_set(machine.register(*register));
        }
        for (offset, register) in FLOAT_ARGUMENT_REGISTERS.iter().enumerate() {
            body.local_get((UNIFORM_WRAPPER_ARGUMENTS + offset) as u32);
            body.i64_reinterpret_f64();
            body.global_set(machine.vector_register(*register, VectorHalf::Low));
        }

        begin_guest_stack(&mut body, machine);

        body.call(guest);
        body.global_get(machine.register(RETURN_VALUE_REGISTER));
        body.global_get(machine.vector_register(FLOAT_RETURN_VALUE_REGISTER, VectorHalf::Low));
        body.f64_reinterpret_i64();
        body.finish()
    }

    /// The host entry point for a function whose signature is known: an
    /// ordinary wasm function, as far as anything calling it can tell.
    ///
    /// This is the same wrapper as above with the zero-information part taken
    /// out. Instead of filling every argument register of both files and
    /// handing back both result registers, it fills exactly the registers the
    /// signature names and returns exactly the one result it has — which is
    /// what lets a foreign module, or a host, call it without knowing that an
    /// emulated register file exists.
    fn build_typed_entry_wrapper(
        &self,
        machine: &MachineState,
        guest: FunctionReference,
        signature: &Signature,
    ) -> Result<FunctionBody> {
        let locations = signature.argument_locations()?;
        let mut body = FunctionBodyBuilder::new(signature.parameters.len() as u32);

        for (index, (parameter, location)) in
            signature.parameters.iter().zip(&locations).enumerate()
        {
            body.local_get(index as u32);
            marshal::store_argument(&mut body, machine, *parameter, *location);
        }

        begin_guest_stack(&mut body, machine);
        body.call(guest);

        if let Some(result) = signature.result {
            marshal::load_result(&mut body, machine, result);
        }
        Ok(body.finish())
    }
}

/// The resume entry for every slot-reserving transfer in a function, keyed
/// by the instruction's offset.
///
/// A call's entry is the block its next instruction heads in the call-split
/// graph — where execution stands when the callee returns. A tail jump's
/// entry is the epilogue arm past the real blocks: the frame it suspends has
/// nothing left to run but its own return. A call with no next instruction
/// is a call to a function that never returns; its slot's ID is never
/// consumed, and the epilogue arm is as good a value as any.
fn resume_entries(lifted: &LiftedFunction, graph: &ControlFlowGraph) -> Result<HashMap<u64, u32>> {
    use iced_x86::FlowControl;
    let epilogue = graph.blocks.len() as u32;
    let mut entries = HashMap::new();
    for (position, instruction) in lifted.instructions.iter().enumerate() {
        let entry = match instruction.instruction.flow_control() {
            FlowControl::Call | FlowControl::IndirectCall => {
                match lifted.instructions.get(position + 1) {
                    Some(next) => u32::try_from(graph.block_at(next.offset)?)
                        .expect("a function has fewer than 2^32 blocks"),
                    None => epilogue,
                }
            }
            FlowControl::UnconditionalBranch
            | FlowControl::ConditionalBranch
            | FlowControl::IndirectBranch => epilogue,
            _ => continue,
        };
        entries.insert(instruction.offset, entry);
    }
    // A function that runs off its end into the one below makes a call no
    // instruction stands for, so its site is keyed by the boundary — a place
    // no instruction occupies, which is what keeps the two kinds of key
    // apart.
    for block in &graph.blocks {
        match &block.terminator {
            crate::cfg::Terminator::FallsOut { into } => {
                entries.insert(*into, epilogue);
            }
            // A `switch` arm that leaves the function is a tail call, and it
            // reserves a slot the same way.
            crate::cfg::Terminator::Switch { targets } => {
                for target in targets.iter().filter(|target| !lifted.contains(**target)) {
                    entries.insert(*target, epilogue);
                }
            }
            _ => {}
        }
    }
    Ok(entries)
}

/// The driver that brings a restored checkpoint back to life.
///
/// The restored guest stack's top slot holds the resume ID of the innermost
/// suspended frame — the caller of whatever the checkpoint was taken inside.
/// Pop it (the pop that suspended callee's return owes), then walk: each
/// resume body runs its frame to completion and yields the ID above it,
/// until the sentinel the entry wrapper planted says the chain is done. The
/// program's result is where it always is, in the register globals.
fn build_resume_driver(machine: &MachineState, resume_type: u32) -> FunctionBody {
    let mut body = FunctionBodyBuilder::new(0);
    let id = body.declare_local(ValueType::I64);

    body.global_get(machine.register(STACK_POINTER_REGISTER));
    body.i32_wrap_i64();
    body.i64_load(3, 0);
    body.local_set(id);

    // Give back what the suspended site reserved. A call site takes eight
    // bytes; a syscall site takes the red zone as well, because the kernel is
    // not allowed to spend it. Only this first pop can be either — every
    // later slot in the chain is popped by a resume body's own `ret`, and
    // what a `ret` gives back is always its caller's call slot.
    body.global_get(machine.register(STACK_POINTER_REGISTER));
    body.i64_const(8);
    body.i64_const(SYSCALL_RESERVATION);
    body.local_get(id);
    body.i64_const(RED_ZONE_RESERVED);
    body.i64_and();
    body.i64_eqz();
    body.select();
    body.i64_add();
    body.global_set(machine.register(STACK_POINTER_REGISTER));

    body.block();
    body.loop_();
    body.local_get(id);
    body.i64_const(RETURN_ADDRESS_SENTINEL);
    body.i64_eq();
    body.if_();
    body.branch(2);
    body.end();

    // A resume ID: entry index in the high half, table slot in the low, and
    // the frame-size marker riding above the index.
    body.local_get(id);
    body.i64_const(32);
    body.i64_shr_unsigned();
    body.i64_const(RESUME_ENTRY_MASK);
    body.i64_and();
    body.i32_wrap_i64();
    body.local_get(id);
    body.i32_wrap_i64();
    body.call_indirect(resume_type);
    body.local_set(id);
    body.branch(0);
    body.end();
    body.end();

    body.finish()
}

/// Reserves the region a host-entry wrapper runs its guest on.
///
/// `.bss`, so the module carries a symbol rather than sixty-four kilobytes
/// of zeros.
fn define_x87_stack(wasm: &mut WasmObject) -> DataReference {
    let segment_index = wasm.data_segments.len() as u32;
    wasm.data_segments.push(DataSegment {
        name: format!(".bss.{X87_STACK}"),
        alignment_log2: 4,
        bytes: vec![0; X87_STACK_SIZE as usize],
        relocations: Vec::new(),
    });
    let symbol_index = wasm.add_symbol(Symbol {
        name: X87_STACK.to_string(),
        target: SymbolTarget::Data(Some(DataSymbolLocation {
            segment_index,
            offset: 0,
            size: X87_STACK_SIZE,
        })),
        // Local, because every object using x87 wants its own and
        // they must not collapse onto one another.
        flags: symbol_flags::LOCAL,
    });
    DataReference {
        symbol_index,
        // The top: a stack grows down from the end of its region.
        addend: X87_STACK_SIZE as i32,
    }
}

/// The stack an x87 helper runs on, and the symbol naming it.
///
/// A helper cannot borrow the guest's, and this is the same rule the seam's
/// kernel stack states: SysV lets a compiler keep values in the 128 bytes
/// *below* `%rsp` without moving it, and the guest's stack pointer is where
/// a host-entry wrapper started it — the linker's own. A helper frame
/// allocated from there lands exactly in that red zone.
///
/// It is not hypothetical. A `long double` comparison stores its two
/// operands in the red zone, executes two x87 instructions, and reads them
/// back; with the helpers on the guest's stack it read back what the helper
/// had written, and the answer was wrong in a way no single instruction's
/// test could see.
const X87_STACK: &str = "x87_helper_stack";

/// How much of it there is. Sized like the kernel's region: the helpers are
/// leaf-ish Rust with a bounded call depth, and a fixed region cannot grow
/// into anything.
const X87_STACK_SIZE: u32 = 64 * 1024;

/// Starts the guest stack from the linker's stack pointer.
///
/// SysV asks for `rsp % 16 == 8` at entry, as if a return address had just
/// been pushed. Align down, then make room for the slot that address would
/// occupy — and fill it, so that a guest `ret` pops something recognisable
/// rather than whatever was there.
pub(crate) fn begin_guest_stack(body: &mut FunctionBodyBuilder, machine: &MachineState) {
    body.global_get(machine.linker_stack_pointer);
    body.i64_extend_i32_unsigned();
    body.i64_const(-16);
    body.i64_and();
    body.i64_const(8);
    body.i64_sub();
    body.global_set(machine.register(STACK_POINTER_REGISTER));

    body.global_get(machine.register(STACK_POINTER_REGISTER));
    body.i32_wrap_i64();
    body.i64_const(RETURN_ADDRESS_SENTINEL);
    body.i64_store(3, 0);
}

/// The wasm type of the uniform zero-information shim.
fn uniform_wrapper_type() -> FunctionType {
    FunctionType {
        parameters: [ValueType::I64; UNIFORM_WRAPPER_ARGUMENTS]
            .into_iter()
            .chain([ValueType::F64; UNIFORM_WRAPPER_FLOAT_ARGUMENTS])
            .collect(),
        results: vec![ValueType::I64, ValueType::F64],
    }
}

/// The wasm type a known signature gives a wrapper.
fn typed_wrapper_type(signature: &Signature) -> Result<FunctionType> {
    // Asked for its side effect: a signature whose arguments do not all fit
    // in registers cannot be carried, and finding that out here keeps the
    // failure at the point where the signature is chosen.
    signature.argument_locations()?;
    Ok(FunctionType {
        parameters: signature
            .parameters
            .iter()
            .map(|parameter| parameter.value_type())
            .collect(),
        results: signature
            .result
            .map(|result| vec![result.value_type()])
            .unwrap_or_default(),
    })
}

/// What the input's relocations turn out to mean, classified by *use* rather
/// than by what the ELF symbol table calls things — which is the only way to
/// tell a callee from a data object when the input says only "undefined".
#[derive(Default)]
struct TextReferences {
    /// Symbols reached by a `call` or a tail `jmp`.
    call_targets: HashSet<usize>,
    /// Defined functions whose address is taken, by where they begin.
    addressed_locations: BTreeSet<(usize, u64)>,
    /// Symbols another object defines whose address is taken.
    addressed_symbols: BTreeSet<usize>,
    /// Data locations consumed as jump-table entries, whose relocations must
    /// not be translated: they name code, which has no address.
    consumed_entries: HashSet<(usize, u64)>,
    /// Every recovered jump table, whose entries are rewritten to hold their
    /// own index.
    tables: Vec<crate::jump_table::JumpTable>,
    /// Whether anything calls through the indirect function table. An object
    /// that only receives function pointers needs the table without putting
    /// anything in it.
    calls_indirectly: bool,
    /// Whether anything uses the x87 stack, and so whether its helpers must
    /// be imported. Answered by the same scan that finds the syscalls,
    /// because imports occupy the low end of the function index space and
    /// have to be declared before any body takes an index.
    uses_x87: bool,
    /// Whether the object contains a `syscall`, which is what obliges it to
    /// import the kernel seam. There is no relocation to notice, so the
    /// instruction scan is the only evidence.
    issues_syscalls: bool,
    /// Whether anything divides a 128-bit dividend, which is what obliges
    /// the object to define [`WIDE_DIVISION`]. Found by the same scan, for
    /// the same reason: a defined function's index has to be reserved before
    /// any body is built.
    divides_wide: bool,
}

impl TextReferences {
    fn is_referenced_function(&self, symbol: usize) -> bool {
        self.call_targets.contains(&symbol) || self.addressed_symbols.contains(&symbol)
    }
}

/// Data symbol names must not collide when two transpiled objects are linked.
/// Section symbols are per-object concepts, so they are emitted as local
/// symbols under a name that says where they came from.
fn wasm_data_symbol_name(section_name: &str, symbol: &crate::reader::Symbol) -> String {
    if symbol.role == SymbolRole::Section {
        format!("{section_name}.whole")
    } else {
        symbol.name.clone()
    }
}

fn data_symbol_flags(symbol: &crate::reader::Symbol) -> u32 {
    match symbol.binding {
        // Locals cannot be referenced from other objects, so a local name can
        // never collide at link time.
        SymbolBinding::Local => symbol_flags::LOCAL,
        SymbolBinding::Weak => symbol_flags::WEAK,
        SymbolBinding::Global => 0,
    }
}

fn guest_symbol_flags(symbol: Option<&crate::reader::Symbol>) -> u32 {
    // The guest entry point keeps the input's binding so that duplicate
    // definitions resolve the way they would have natively, but it is never
    // exported: the wrapper is the module's public face. A function no
    // symbol named binds locally, since there is no name to collide with.
    let binding = match symbol.map(|symbol| symbol.binding) {
        None | Some(SymbolBinding::Local) => symbol_flags::LOCAL,
        Some(SymbolBinding::Weak) => symbol_flags::WEAK,
        Some(SymbolBinding::Global) => 0,
    };
    binding | symbol_flags::HIDDEN
}

fn wrapper_symbol_flags(symbol: &crate::reader::Symbol) -> u32 {
    let binding = match symbol.binding {
        SymbolBinding::Weak => symbol_flags::WEAK,
        _ => 0,
    };
    binding | symbol_flags::EXPORTED
}

/// What to do with a function the translator cannot translate.
///
/// A real binary contains code that never runs. glibc ships AVX-512 string
/// routines beside SSE2 ones and picks between them at startup from CPUID —
/// which the design curates to a baseline without AVX, precisely so the SSE2
/// paths are the ones selected. The AVX bodies are still *there*, and a
/// translator that must render every byte it finds would refuse a binary
/// over code that cannot execute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Untranslatable {
    /// Fail the whole translation, naming the instruction. The default,
    /// because for anything written to be translated a refusal is a gap in
    /// the translator rather than a fact about the input.
    Refuse,
    /// Give the function a body that names itself and stops, and report it.
    /// The refusal moves from bake time to the moment something actually
    /// calls the thing — where it is still loud, and where it also says
    /// that the curation was wrong.
    Trap,
}

/// A function that got a trapping body instead of a translation.
#[derive(Clone, Debug)]
pub struct Refusal {
    pub name: String,
    /// What said the function was there. A refusal for a function no symbol
    /// named is a different kind of news from one for a function the symbol
    /// table describes: the first may be discovery reaching too far, and the
    /// second cannot be. See `crate::discover`.
    pub witness: crate::discover::Witness,
    /// The translator's own message, kept whole: it names the instruction,
    /// which is what turns this list into a worklist.
    pub reason: String,
}

/// A change the loader must make to the program image before it runs it.
///
/// A jump table's entries are rewritten so that the address the dispatch
/// computes is `table + arm` whatever form the entries were in — which is
/// what makes the dispatch a `br_table` over an arm number. In a relocatable
/// object those bytes are in a data segment the module carries, so the
/// rewrite happens there. In a linked executable the bytes reach the guest
/// through the loader, from the image, so the rewrite has to reach the image
/// too — and this is how it travels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Patch {
    /// The virtual address the bytes go at.
    pub address: u64,
    pub bytes: Vec<u8>,
}

/// A finished translation: the module, whatever the image needs done to it
/// before the module can run, and whatever could not be translated.
pub struct Translation {
    pub module: Vec<u8>,
    pub patches: Vec<Patch>,
    /// Empty unless [`Untranslatable::Trap`] was asked for, and worth
    /// printing when it is not: these are functions that will stop the
    /// container if anything reaches them.
    pub refused: Vec<Refusal>,
}

/// The name the exec map's lookup function is given, so a dump can point at
/// it and a linked module can be inspected.
pub const EXEC_MAP_LOOKUP: &str = "x86_slot_of";
/// The data segment holding the map itself.
pub const EXEC_MAP_TABLE: &str = "x86_exec_map";

/// What a function that could not be translated calls instead of running.
///
/// It takes the function's own name, so the failure says which one rather
/// than leaving a bare trap in a backtrace — and the kernel is what turns it
/// into a sentence, on the same path every other named fault takes.
pub const UNTRANSLATED: &str = "kisal_untranslated";

/// What the exec map calls when an address is not a function's.
///
/// A function pointer in a linked program is a virtual address, and the map
/// turns one into the slot the indirect-call table is indexed by. An address
/// with no entry means the guest computed a pointer to something that is not
/// the start of a translated function — and the *address* is the whole of
/// what is worth knowing, so it is reported rather than trapped on.
pub const NO_FUNCTION_AT: &str = "kisal_no_function_at";

/// One entry: a virtual address and the table slot the function at it holds.
/// Sixteen bytes so the address is eight-byte aligned, which is what a load
/// wants and what makes the search's arithmetic a shift.
const EXEC_MAP_ENTRY: u64 = 16;

/// Builds the static exec map: virtual address to indirect-table slot.
///
/// This exists because a linked executable has no relocations, so nothing
/// turns a function pointer into a table slot at translation time. A
/// function pointer in the guest is a *virtual address* — the number the
/// linker put there — and stays one, because that is what the guest can
/// store, compare and pass around. What has to happen instead is that every
/// indirect call translates the address it was given into a slot, and this
/// is the table it reads.
///
/// Sorted by address and searched in halves. The alternative — a slot for
/// every address in the text section — would be a table the size of the
/// program; the alternative to *that*, a scan, is linear in the number of
/// functions and CPython has thousands. A binary search is fourteen
/// comparisons for ten thousand functions, on a path the plan says is hot
/// and M11 measures.
/// The map itself: address and slot, eight bytes each, sorted so the lookup
/// can search it in halves.
///
/// The slot is a *relocation*, not the number this object happened to give
/// it. Every object numbers its own table entries from
/// [`FIRST_TABLE_INDEX`] and the linker renumbers them as it merges the
/// tables — so a constant here is a slot belonging to whichever object won
/// that number, which for a container is the seam. Writing the entry as a
/// relocation is what makes the address the guest holds and the slot the
/// module calls the same function.
fn build_exec_map_table(wasm: &mut WasmObject, entries: &[(u64, TableReference)]) -> DataReference {
    let mut bytes = Vec::with_capacity(entries.len() * EXEC_MAP_ENTRY as usize);
    let mut relocations = Vec::with_capacity(entries.len());
    for (address, slot) in entries {
        bytes.extend_from_slice(&address.to_le_bytes());
        relocations.push(WasmRelocation {
            kind: WasmRelocationKind::TableIndexI32,
            offset: bytes.len() as u32,
            symbol_index: slot.symbol_index,
            addend: 0,
        });
        // The placeholder the linker overwrites, which is this object's own
        // numbering — right when nothing else contributes a table.
        bytes.extend_from_slice(&slot.table_index.to_le_bytes());
        bytes.extend_from_slice(&[0; 4]);
    }
    let segment_index = wasm.data_segments.len() as u32;
    let size = bytes.len() as u32;
    wasm.data_segments.push(DataSegment {
        name: format!(".rodata.{EXEC_MAP_TABLE}"),
        alignment_log2: 3,
        bytes,
        relocations,
    });
    DataReference {
        symbol_index: wasm.add_symbol(Symbol {
            name: EXEC_MAP_TABLE.to_string(),
            target: SymbolTarget::Data(Some(DataSymbolLocation {
                segment_index,
                offset: 0,
                size,
            })),
            flags: symbol_flags::LOCAL,
        }),
        addend: 0,
    }
}

/// `x86_slot_of(address: i64) -> i32`: the slot for a function address, or a
/// trap.
///
/// A trap, and not a sentinel. An indirect call through an address that is
/// not a function is a guest that has already gone wrong — a corrupted
/// pointer, a jump into data — and the useful thing is to stop where it
/// happened. A sentinel slot would turn it into a call to whatever occupies
/// that slot, which is the same bug arriving somewhere else.
/// A body for a function that could not be translated: it names itself and
/// does not return.
///
/// The `unreachable` after the call is not reached — the kernel's report
/// ends the run — but a body has to end, and ending in a trap says that
/// falling out of here was never a possibility.
/// The module's 128-by-64 division helper, as the linker sees it.
///
/// `div` and `idiv` at eight-byte width take a dividend twice as wide as any
/// wasm type, so unlike every narrower form they cannot be computed inline.
/// One function per module rather than one expansion per site: the algorithm
/// is a loop, and sixty-four sites carrying sixty-four copies of it is a
/// module nobody wants to read or to load.
///
/// Local, so that two transpiled objects linked together each keep their own
/// and neither has to be the definition.
const WIDE_DIVISION: &str = "x86_divide_128";

/// The wide-division helper's shape: the dividend's high and low halves, the
/// divisor, and whether the operands are signed; the quotient comes back.
///
/// The remainder is deliberately not returned. It is `dividend - quotient *
/// divisor`, and the low sixty-four bits of that are exact — the true
/// remainder is smaller than the divisor, so nothing it needs lives above
/// them. The caller computes it in two instructions rather than the helper
/// returning a pair.
fn wide_division_type() -> FunctionType {
    FunctionType {
        parameters: vec![
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
            ValueType::I32,
        ],
        results: vec![ValueType::I64],
    }
}

/// `x86_divide_128`: the quotient of a 128-bit dividend by a 64-bit divisor.
///
/// Restartable long division, one bit at a time, because wasm has no wider
/// type to borrow and no widening divide. Sixty-four iterations is slower
/// than the two-digit Knuth recurrence would be, and it is chosen anyway:
/// the fast path in front of it takes every division whose dividend fits in
/// sixty-four bits, which is nearly all of them, and what is left is a
/// handful of calls inside a library's own extended-precision arithmetic.
/// The loop is short enough to read and to check against the machine, and
/// the recurrence is not.
///
/// Every case hardware raises `#DE` for traps here instead: a zero divisor,
/// and a quotient too wide for the accumulator. That is not a shortcut — a
/// divide error is a fault, and a trap is what a fault is in a translated
/// program.
fn build_wide_division() -> FunctionBody {
    const HIGH: u32 = 0;
    const LOW: u32 = 1;
    const DIVISOR: u32 = 2;
    const SIGNED: u32 = 3;

    let mut body = FunctionBodyBuilder::new(4);
    let quotient = body.declare_local(ValueType::I64);
    let remainder = body.declare_local(ValueType::I64);
    let bit = body.declare_local(ValueType::I64);
    let carry = body.declare_local(ValueType::I64);
    let negative = body.declare_local(ValueType::I32);
    let high = body.declare_local(ValueType::I64);
    let low = body.declare_local(ValueType::I64);
    let divisor = body.declare_local(ValueType::I64);

    // A zero divisor is a divide error before anything else is looked at.
    body.local_get(DIVISOR);
    body.i64_eqz();
    body.if_();
    body.unreachable();
    body.end();

    // Signed division is unsigned division of the magnitudes, with the sign
    // put back afterwards. `negative` records whether the two operands
    // disagreed, which is what decides the quotient's sign.
    body.local_get(HIGH);
    body.local_set(high);
    body.local_get(LOW);
    body.local_set(low);
    body.local_get(DIVISOR);
    body.local_set(divisor);
    body.i32_const(0);
    body.local_set(negative);

    body.local_get(SIGNED);
    body.if_();
    // The dividend's sign is its high half's.
    body.local_get(high);
    body.i64_const(0);
    body.i64_lt_signed();
    body.if_();
    body.i32_const(1);
    body.local_set(negative);
    // Negating a 128-bit value: complement both halves and add one, with the
    // carry out of the low half landing in the high.
    body.i64_const(0);
    body.local_get(low);
    body.i64_sub();
    body.local_set(carry);
    body.i64_const(0);
    body.local_get(high);
    body.i64_sub();
    body.local_get(low);
    body.i64_eqz();
    body.i64_extend_i32_unsigned();
    body.i64_const(1);
    body.i64_sub();
    body.i64_add();
    body.local_set(high);
    body.local_get(carry);
    body.local_set(low);
    body.end();

    body.local_get(divisor);
    body.i64_const(0);
    body.i64_lt_signed();
    body.if_();
    body.i32_const(1);
    body.local_get(negative);
    body.i32_sub();
    body.local_set(negative);
    body.i64_const(0);
    body.local_get(divisor);
    body.i64_sub();
    body.local_set(divisor);
    body.end();
    body.end();

    // A quotient this wide cannot be delivered: hardware raises `#DE` for
    // exactly this, and it is the same condition — the high half alone is
    // already at least as large as the divisor.
    body.local_get(high);
    body.local_get(divisor);
    body.i64_ge_unsigned();
    body.if_();
    body.unreachable();
    body.end();

    body.i64_const(0);
    body.local_set(quotient);
    body.local_get(high);
    body.local_set(remainder);

    // The fast path: a dividend that fits in sixty-four bits is one wasm
    // instruction, and is what all but a handful of divisions are.
    body.local_get(high);
    body.i64_eqz();
    body.if_();
    body.local_get(low);
    body.local_get(divisor);
    body.i64_div_unsigned();
    body.local_set(quotient);
    body.else_();

    // Long division, most significant bit first. The invariant is that
    // `remainder` is always less than the divisor, so doubling it and adding
    // one bit needs sixty-five bits — `carry` is the sixty-fifth, and a set
    // carry means the value is certainly at least the divisor without any
    // comparison being possible in sixty-four bits.
    body.i64_const(64);
    body.local_set(bit);
    body.block();
    body.loop_();
    body.local_get(bit);
    body.i64_eqz();
    body.branch_if(1);
    body.local_get(bit);
    body.i64_const(1);
    body.i64_sub();
    body.local_set(bit);

    body.local_get(remainder);
    body.i64_const(63);
    body.i64_shr_unsigned();
    body.local_set(carry);

    body.local_get(remainder);
    body.i64_const(1);
    body.i64_shl();
    body.local_get(low);
    body.local_get(bit);
    body.i64_shr_unsigned();
    body.i64_const(1);
    body.i64_and();
    body.i64_or();
    body.local_set(remainder);

    body.local_get(carry);
    body.i64_eqz();
    body.i32_eqz();
    body.local_get(remainder);
    body.local_get(divisor);
    body.i64_ge_unsigned();
    body.i32_or();
    body.if_();
    body.local_get(remainder);
    body.local_get(divisor);
    body.i64_sub();
    body.local_set(remainder);
    body.local_get(quotient);
    body.i64_const(1);
    body.local_get(bit);
    body.i64_shl();
    body.i64_or();
    body.local_set(quotient);
    body.end();

    body.branch(0);
    body.end(); // loop
    body.end(); // block
    body.end(); // the fast path's `if`

    // The sign, and the range check that goes with it. A negative quotient
    // may be `-2^63` exactly; a positive one may not be `2^63`, which is the
    // asymmetry two's complement has and hardware faults on.
    body.local_get(negative);
    body.if_();
    body.local_get(quotient);
    body.i64_const(i64::MIN);
    body.i64_gt_unsigned();
    body.if_();
    body.unreachable();
    body.end();
    body.i64_const(0);
    body.local_get(quotient);
    body.i64_sub();
    body.local_set(quotient);
    body.else_();
    body.local_get(SIGNED);
    body.if_();
    body.local_get(quotient);
    body.i64_const(0);
    body.i64_lt_signed();
    body.if_();
    body.unreachable();
    body.end();
    body.end();
    body.end();

    body.local_get(quotient);
    body.finish()
}

fn build_refusal(wasm: &mut WasmObject, name: &str, report: FunctionReference) -> FunctionBody {
    let segment_index = wasm.data_segments.len() as u32;
    let bytes = name.as_bytes().to_vec();
    let size = bytes.len() as u32;
    wasm.data_segments.push(DataSegment {
        name: format!(".rodata.{UNTRANSLATED}.{name}"),
        alignment_log2: 0,
        bytes,
        relocations: Vec::new(),
    });
    let text = DataReference {
        symbol_index: wasm.add_symbol(Symbol {
            name: format!("{UNTRANSLATED}.{name}"),
            target: SymbolTarget::Data(Some(DataSymbolLocation {
                segment_index,
                offset: 0,
                size,
            })),
            flags: symbol_flags::LOCAL,
        }),
        addend: 0,
    };

    let mut body = FunctionBodyBuilder::new(0);
    body.i32_const_data_address(text);
    body.i32_const(size as i32);
    body.call(report);
    body.unreachable();
    body.finish()
}

fn build_exec_map_lookup(
    table: DataReference,
    count: u32,
    report: FunctionReference,
) -> FunctionBody {
    let mut body = FunctionBodyBuilder::new(1);
    let low = body.declare_local(ValueType::I32);
    let high = body.declare_local(ValueType::I32);
    let middle = body.declare_local(ValueType::I32);

    body.i32_const(0);
    body.local_set(low);
    body.i32_const(count as i32);
    body.local_set(high);

    body.block();
    body.loop_();
    // while low < high
    body.local_get(low);
    body.local_get(high);
    body.i32_ge_unsigned();
    body.branch_if(1);

    // middle = low + (high - low) / 2
    body.local_get(high);
    body.local_get(low);
    body.i32_sub();
    body.i32_const(1);
    body.i32_shr_unsigned();
    body.local_get(low);
    body.i32_add();
    body.local_set(middle);

    // The address stored at `middle`.
    body.i32_const_data_address(table);
    body.local_get(middle);
    body.i32_const(EXEC_MAP_ENTRY as i32);
    body.i32_mul();
    body.i32_add();
    body.local_tee(middle);
    body.i64_load(3, 0);

    // Found: return its slot.
    body.local_get(0);
    body.i64_eq();
    body.if_();
    body.local_get(middle);
    body.i32_load(2, 8);
    body.return_();
    body.end();

    // Otherwise halve. `middle` now holds a byte address, so the index has
    // to be recovered from it — cheaper than keeping both.
    body.local_get(middle);
    body.i64_load(3, 0);
    body.local_get(0);
    body.i64_lt_unsigned();
    body.if_();
    // The entry is below the target: search above it.
    body.local_get(middle);
    body.i32_const_data_address(table);
    body.i32_sub();
    body.i32_const(EXEC_MAP_ENTRY as i32);
    body.i32_div_unsigned();
    body.i32_const(1);
    body.i32_add();
    body.local_set(low);
    body.else_();
    body.local_get(middle);
    body.i32_const_data_address(table);
    body.i32_sub();
    body.i32_const(EXEC_MAP_ENTRY as i32);
    body.i32_div_unsigned();
    body.local_set(high);
    body.end();

    body.branch(0);
    body.end(); // loop
    body.end(); // block

    // Not a function. The address is the whole of what is worth knowing,
    // so it is named rather than trapped on anonymously.
    body.local_get(0);
    body.call(report);
    body.unreachable();
    body.finish()
}
