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
use crate::translate::{FunctionTranslator, SymbolResolver, SymbolValue};

/// Suffix distinguishing a translated function, which runs on the emulated
/// register convention, from the host-entry wrapper that carries its name.
pub const GUEST_SUFFIX: &str = "_guest";

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
    /// Data symbols standing for recovered jump tables, by where they begin.
    jump_tables: HashMap<(usize, u64), u32>,
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
}

pub struct Transpiler<'a> {
    object: &'a ObjectFile,
    mode: structurer::Mode,
    signatures: SignatureTable,
    promote: bool,
}

impl<'a> Transpiler<'a> {
    pub fn new(object: &'a ObjectFile) -> Self {
        Self {
            object,
            mode: structurer::Mode::default(),
            signatures: SignatureTable::new(),
            promote: true,
        }
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
            .filter(|function| self.object.symbols[function.symbol].binding != SymbolBinding::Local)
            .map(|function| function.name.clone())
            .collect()
    }

    /// Translates the whole object, returning the serialized relocatable wasm
    /// object.
    pub fn transpile(&self) -> Result<Vec<u8>> {
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
            jump_tables: HashMap::new(),
            segment_of_section: HashMap::new(),
            data: HashMap::new(),
            names: HashMap::new(),
        };
        for (index, symbol) in self.object.symbols.iter().enumerate() {
            if !symbol.name.is_empty() {
                symbols.names.insert(index, symbol.name.clone());
            }
        }

        // Imports first: they occupy the low end of the function index space,
        // so every undefined callee has to be known before any definition
        // takes an index.
        self.declare_imported_functions(&mut wasm, &mut symbols, guest_type, &references)?;
        self.declare_data(&mut wasm, &mut symbols, &references)?;
        let plans = self.declare_functions(&mut wasm, &mut symbols)?;

        // Table slots have to exist before anything can refer to one, and
        // they are only known once every function has a symbol.
        self.assign_table_slots(&mut wasm, &mut symbols, &references);
        wasm.uses_function_table = references.calls_indirectly || !wasm.table_functions.is_empty();
        self.rewrite_jump_tables(&mut wasm, &mut symbols, &references)?;
        self.translate_data_relocations(&mut wasm, &symbols, &references)?;

        // Bodies are built against the finished symbol table, then handed to
        // the object in the order the indices were reserved.
        let mut bodies: Vec<(u32, u32, FunctionBody)> = Vec::new();
        for plan in &plans {
            let lifted = &lifted_functions[plan.input];
            let body = self
                .translate_guest_function(&symbols, &machine, lifted, guest_type)
                .with_context(|| format!("translating function `{}`", lifted.name))?;
            bodies.push((plan.guest.function_index, guest_type, body));

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

        bodies.sort_by_key(|(index, _, _)| *index);
        for (index, type_index, body) in bodies {
            debug_assert_eq!(index, wasm.next_defined_function_index());
            wasm.defined_functions
                .push(DefinedFunction { type_index, body });
        }

        Ok(wasm.serialize())
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
                if matches!(
                    lifted.instruction.flow_control(),
                    FlowControl::IndirectCall | FlowControl::IndirectBranch
                ) {
                    references.calls_indirectly = true;
                }

                if lifted.displacement.is_none()
                    && lifted.instruction.memory_base() == iced_x86::Register::RIP
                {
                    // The assembler resolved this against the same section,
                    // so it names a function's address with no relocation to
                    // read — the same shortcut it takes for direct calls.
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

        for location in &references.addressed_locations {
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
    }

    /// Undefined function symbols become imports of the callee's *guest*
    /// entry point, since that is the convention both sides of a
    /// transpiled-to-transpiled call speak.
    fn declare_imported_functions(
        &self,
        wasm: &mut WasmObject,
        symbols: &mut SymbolTable<'_>,
        guest_type: u32,
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
                if table.relative {
                    let value = (arm as u64).to_le_bytes();
                    let width = table.stride as usize;
                    segment.bytes[start..end].copy_from_slice(&value[..width]);
                } else {
                    segment.relocations.push(WasmRelocation {
                        kind: WasmRelocationKind::MemoryAddressI32,
                        offset: entry_offset as u32,
                        symbol_index,
                        addend: arm as i32,
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
    ) -> Result<Vec<FunctionPlan>> {
        let mut plans = Vec::new();
        let mut next_index = wasm.imported_functions.len() as u32;

        for (input, function) in self.object.functions.iter().enumerate() {
            let elf_symbol = &self.object.symbols[function.symbol];

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
            symbols.functions.insert(function.symbol, guest);
            symbols
                .functions_by_location
                .insert((function.section, function.offset), guest);

            // A function nothing outside the object can name needs no host
            // entry point.
            let wrapper = if elf_symbol.binding == SymbolBinding::Local {
                None
            } else {
                let wrapper_index = next_index;
                next_index += 1;
                let wrapper_symbol = wasm.add_symbol(Symbol {
                    name: function.name.clone(),
                    target: SymbolTarget::Function(wrapper_index),
                    flags: wrapper_symbol_flags(elf_symbol),
                });
                Some(FunctionReference {
                    symbol_index: wrapper_symbol,
                    function_index: wrapper_index,
                })
            };

            plans.push(FunctionPlan {
                input,
                guest,
                wrapper,
            });
        }

        Ok(plans)
    }

    fn translate_guest_function(
        &self,
        symbols: &SymbolTable<'_>,
        machine: &MachineState,
        lifted: &LiftedFunction,
        guest_type: u32,
    ) -> Result<FunctionBody> {
        let mut body = FunctionBodyBuilder::new(0);
        let mut translator = FunctionTranslator::new(symbols, machine, lifted.section, guest_type);
        if self.promote {
            translator.begin_function(&mut body, lifted);
        }
        structurer::translate_function(&mut body, &mut translator, lifted, self.mode)?;
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

fn guest_symbol_flags(symbol: &crate::reader::Symbol) -> u32 {
    // The guest entry point keeps the input's binding so that duplicate
    // definitions resolve the way they would have natively, but it is never
    // exported: the wrapper is the module's public face.
    let binding = match symbol.binding {
        SymbolBinding::Local => symbol_flags::LOCAL,
        SymbolBinding::Weak => symbol_flags::WEAK,
        SymbolBinding::Global => 0,
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
