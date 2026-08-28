//! Reading x86-64 ELF relocatable objects into the model the rest of the
//! pipeline consumes.
//!
//! Nothing here interprets machine code; this stage answers "what sections,
//! symbols and relocations exist, and where do functions live".

use anyhow::{Context, Result, bail};
use object::read::{Object, ObjectSection, ObjectSymbol};
use object::{RelocationFlags, SectionIndex, SymbolIndex};

/// What role a section plays in the translated module.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SectionRole {
    /// Executable code we lift.
    Text,
    /// Initialised, writable data.
    Data,
    /// Initialised, read-only data.
    ReadOnlyData,
    /// Zero-initialised data, occupying no bytes in the input.
    ZeroFilled,
    /// Anything else — debug info, notes, unwind tables. Carried so the dump
    /// can mention them; never translated.
    Untranslated,
}

impl SectionRole {
    pub fn is_data(self) -> bool {
        matches!(
            self,
            SectionRole::Data | SectionRole::ReadOnlyData | SectionRole::ZeroFilled
        )
    }
}

/// Which of the two shapes an input has.
///
/// The distinction runs through everything downstream. A relocatable object
/// has no addresses and every reference is a relocation naming a symbol; a
/// linked executable has addresses and no relocations at all, so a reference
/// is already the number it resolves to. The translator has to answer the
/// same question — "what does this operand point at" — from two different
/// kinds of evidence.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    /// `gcc -c`: sections at address zero, relocations to resolve.
    Relocatable,
    /// A complete static executable: sections at their virtual addresses,
    /// nothing left to relocate.
    Linked,
}

/// A `PT_LOAD` segment, for the kernel that has to place one.
///
/// Kept alongside the sections rather than derived from them: what a loader
/// maps is segments, and the section headers of a stripped binary may not
/// even be there. `memory_size` past `file_size` is `.bss`, which the loader
/// zeroes.
#[derive(Clone, Debug)]
pub struct Segment {
    pub address: u64,
    pub file_size: u64,
    pub memory_size: u64,
    pub bytes: Vec<u8>,
    pub readable: bool,
    pub writable: bool,
    pub executable: bool,
    pub alignment: u64,
}

#[derive(Clone, Debug)]
pub struct Section {
    pub name: String,
    pub role: SectionRole,
    /// The virtual address the section is loaded at. Zero for a relocatable
    /// object, where nothing has been placed yet.
    pub address: u64,
    /// Section contents. Empty for zero-filled sections, whose size is in
    /// `size` instead.
    pub bytes: Vec<u8>,
    pub size: u64,
    pub alignment: u64,
    pub relocations: Vec<Relocation>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SymbolBinding {
    Local,
    Global,
    Weak,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SymbolRole {
    Function,
    Data,
    /// The symbol standing for a whole section; relocation targets often name
    /// one of these plus an addend.
    Section,
    Other,
}

#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub role: SymbolRole,
    pub binding: SymbolBinding,
    pub hidden: bool,
    /// Section this symbol is defined in, if any.
    pub section: Option<usize>,
    /// Offset of the symbol within its section.
    pub offset: u64,
    pub size: u64,
    pub defined: bool,
}

/// The x86-64 relocation kinds the transpiler understands. Everything else is
/// a hard error naming the raw ELF type, never a silent skip.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RelocationKind {
    /// `R_X86_64_PC32` — program-counter-relative 32-bit.
    ProgramCounterRelative32,
    /// `R_X86_64_PLT32` — as above, but via the procedure linkage table;
    /// for a relocatable object the distinction does not survive lifting.
    ProcedureLinkage32,
    /// `R_X86_64_64` — absolute 64-bit.
    Absolute64,
    /// `R_X86_64_32` — absolute 32-bit, zero-extended.
    Absolute32,
    /// `R_X86_64_32S` — absolute 32-bit, sign-extended.
    Absolute32Signed,
    /// `R_X86_64_GOTPCREL` and its relaxable variants — a
    /// program-counter-relative reference to the symbol's *global offset
    /// table* slot, which is how the address of a symbol defined elsewhere is
    /// taken.
    ///
    /// There is no global offset table in the translated module. The `X`
    /// variants exist precisely so a linker may rewrite the load into the
    /// address computation it stands for, and that is what the translator
    /// does; see [`crate::translate`].
    GlobalOffsetTableRelative32,
}

impl RelocationKind {
    /// Width in bytes of the field the relocation overwrites.
    pub fn width(self) -> u64 {
        match self {
            RelocationKind::Absolute64 => 8,
            _ => 4,
        }
    }

    /// Whether the relocated value is relative to the relocation site.
    pub fn is_program_counter_relative(self) -> bool {
        matches!(
            self,
            RelocationKind::ProgramCounterRelative32
                | RelocationKind::ProcedureLinkage32
                | RelocationKind::GlobalOffsetTableRelative32
        )
    }

    /// Whether the relocation names the symbol's address indirectly, through
    /// a global offset table slot.
    pub fn is_global_offset_table(self) -> bool {
        self == RelocationKind::GlobalOffsetTableRelative32
    }

    fn from_elf_type(elf_type: u32) -> Option<Self> {
        const PROGRAM_COUNTER_RELATIVE_32: u32 = object::elf::R_X86_64_PC32.0;
        const PROCEDURE_LINKAGE_32: u32 = object::elf::R_X86_64_PLT32.0;
        const ABSOLUTE_64: u32 = object::elf::R_X86_64_64.0;
        const ABSOLUTE_32: u32 = object::elf::R_X86_64_32.0;
        const ABSOLUTE_32_SIGNED: u32 = object::elf::R_X86_64_32S.0;
        const GLOBAL_OFFSET_TABLE: u32 = object::elf::R_X86_64_GOTPCREL.0;
        const GLOBAL_OFFSET_TABLE_RELAXABLE: u32 = object::elf::R_X86_64_GOTPCRELX.0;
        const GLOBAL_OFFSET_TABLE_RELAXABLE_REX: u32 = object::elf::R_X86_64_REX_GOTPCRELX.0;

        match elf_type {
            PROGRAM_COUNTER_RELATIVE_32 => Some(RelocationKind::ProgramCounterRelative32),
            PROCEDURE_LINKAGE_32 => Some(RelocationKind::ProcedureLinkage32),
            ABSOLUTE_64 => Some(RelocationKind::Absolute64),
            ABSOLUTE_32 => Some(RelocationKind::Absolute32),
            ABSOLUTE_32_SIGNED => Some(RelocationKind::Absolute32Signed),
            GLOBAL_OFFSET_TABLE
            | GLOBAL_OFFSET_TABLE_RELAXABLE
            | GLOBAL_OFFSET_TABLE_RELAXABLE_REX => {
                Some(RelocationKind::GlobalOffsetTableRelative32)
            }
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Relocation {
    /// Offset of the relocated field within its section.
    pub offset: u64,
    pub kind: RelocationKind,
    /// Index into [`ObjectFile::symbols`].
    pub symbol: usize,
    pub addend: i64,
}

/// A function to translate: a span of a text section named by an `STT_FUNC`
/// symbol.
#[derive(Clone, Debug)]
pub struct Function {
    pub name: String,
    /// Index into [`ObjectFile::symbols`], where a symbol named this
    /// function. A function the unwind table found and the symbol table did
    /// not has none — nothing outside the object can name it, because there
    /// is no name to use.
    pub symbol: Option<usize>,
    /// Index into [`ObjectFile::sections`].
    pub section: usize,
    pub offset: u64,
    pub size: u64,
}

pub struct ObjectFile {
    pub layout: Layout,
    /// The entry point's virtual address, for a linked executable.
    pub entry: u64,
    /// The `PT_LOAD` segments a loader places. Empty for a relocatable
    /// object, which is not loaded at all.
    pub segments: Vec<Segment>,
    pub sections: Vec<Section>,
    /// Indexed by ELF symbol-table index, so relocations can refer to symbols
    /// directly. Entries the reader does not model are still present.
    pub symbols: Vec<Symbol>,
    pub functions: Vec<Function>,
}

impl ObjectFile {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let file =
            object::read::File::parse(bytes).context("parsing the input as an object file")?;

        if file.format() != object::BinaryFormat::Elf {
            bail!("expected an ELF object file, found {:?}", file.format());
        }
        if file.architecture() != object::Architecture::X86_64 {
            bail!(
                "expected an x86-64 object file, found {:?}",
                file.architecture()
            );
        }
        let layout = match file.kind() {
            object::ObjectKind::Relocatable => Layout::Relocatable,
            // A complete static executable. `Dynamic` is deliberately not
            // here: a shared object still has relocations and a `PT_INTERP`
            // consumer, and translating one without a loader would produce a
            // module whose imports nothing resolves.
            object::ObjectKind::Executable => Layout::Linked,
            other => bail!(
                "expected a relocatable object (`gcc -c`) or a static \
                 executable, found {other:?}"
            ),
        };

        // ELF section indices are one-based and sparse from our point of
        // view; map them onto our dense vector.
        let mut section_of_elf_index = std::collections::HashMap::new();
        let mut sections = Vec::new();
        for section in file.sections() {
            let name = section.name().unwrap_or("<unnamed>").to_string();
            let role = classify_section(&section, &name);
            let bytes = match role {
                SectionRole::ZeroFilled => Vec::new(),
                _ => section.data().unwrap_or(&[]).to_vec(),
            };
            section_of_elf_index.insert(section.index(), sections.len());
            sections.push(Section {
                name,
                role,
                address: section.address(),
                bytes,
                size: section.size(),
                alignment: section.align().max(1),
                relocations: Vec::new(),
            });
        }

        let mut symbols = read_symbols(&file, &section_of_elf_index)?;
        read_relocations(&file, &section_of_elf_index, &mut sections)?;
        if layout == Layout::Linked {
            // A symbol's address is a virtual address once something has
            // been placed. The rest of the pipeline works in offsets within
            // a section, so subtract the section's own address — which for a
            // relocatable object is zero, and this is why it can be done in
            // one place rather than at every use.
            for symbol in &mut symbols {
                if let Some(section) = symbol.section {
                    symbol.offset = symbol.offset.saturating_sub(sections[section].address);
                }
            }
        }
        let mut functions = collect_functions(&symbols, &sections, layout)?;
        split_at_interior_entries(&sections, &mut functions)?;
        let segments = read_segments(&file);

        Ok(Self {
            layout,
            entry: if layout == Layout::Linked {
                file.entry()
            } else {
                0
            },
            segments,
            sections,
            symbols,
            functions,
        })
    }

    /// Which section holds a virtual address, and where in it.
    ///
    /// The linked-mode counterpart of [`Self::resolve`]: with no relocations
    /// to name a symbol, an operand is already the address it means, and the
    /// question becomes which section contains it.
    pub fn section_at(&self, address: u64) -> Option<(usize, u64)> {
        if self.layout != Layout::Linked {
            return None;
        }
        self.sections
            .iter()
            .enumerate()
            .find_map(|(index, section)| {
                let size = section.size.max(section.bytes.len() as u64);
                // A section at address zero is one the loader does not place —
                // debug info, symbol tables — and every address would fall in
                // the first of them.
                (section.address != 0
                    && address >= section.address
                    && address < section.address + size)
                    .then(|| (index, address - section.address))
            })
    }

    /// The function whose extent contains a virtual address, if any.
    pub fn function_at(&self, address: u64) -> Option<usize> {
        let (section, offset) = self.section_at(address)?;
        self.functions.iter().position(|function| {
            function.section == section
                && offset >= function.offset
                && offset < function.offset + function.size
        })
    }

    /// The virtual address a function starts at.
    pub fn address_of(&self, function: &Function) -> u64 {
        self.sections[function.section].address + function.offset
    }

    /// Where a symbolic reference lands: the defining section and the byte
    /// offset within it. `None` for references to undefined symbols, which
    /// have no place in this object.
    pub fn resolve(&self, symbol: usize, addend: i64) -> Option<(usize, i64)> {
        let symbol = self.symbols.get(symbol)?;
        let section = symbol.section?;
        Some((section, symbol.offset as i64 + addend))
    }

    pub fn section_named(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|section| section.name == name)
    }

    /// Relocations applying to a byte range of a section, in offset order.
    pub fn relocations_in(&self, section: usize, range: std::ops::Range<u64>) -> Vec<&Relocation> {
        let mut found: Vec<&Relocation> = self.sections[section]
            .relocations
            .iter()
            .filter(|relocation| range.contains(&relocation.offset))
            .collect();
        found.sort_by_key(|relocation| relocation.offset);
        found
    }
}

fn classify_section(section: &object::read::Section<'_, '_>, name: &str) -> SectionRole {
    use object::SectionKind;
    // Unwind tables look like read-only data but describe code we are not
    // translating; carrying them would emit relocations against text symbols
    // that have no linear-memory address.
    if name == ".eh_frame" || name.starts_with(".eh_frame") || name == ".gcc_except_table" {
        return SectionRole::Untranslated;
    }
    match section.kind() {
        SectionKind::Text => SectionRole::Text,
        SectionKind::Data => SectionRole::Data,
        SectionKind::ReadOnlyData | SectionKind::ReadOnlyString => SectionRole::ReadOnlyData,
        SectionKind::UninitializedData => SectionRole::ZeroFilled,
        _ if name == ".rodata" || name.starts_with(".rodata.") => SectionRole::ReadOnlyData,
        _ => SectionRole::Untranslated,
    }
}

fn read_symbols(
    file: &object::read::File<'_>,
    section_of_elf_index: &std::collections::HashMap<SectionIndex, usize>,
) -> Result<Vec<Symbol>> {
    // Relocations name symbols by their raw ELF symbol-table index, so the
    // vector must be indexed the same way — including the reserved null
    // symbol at index 0, which the `object` crate's iterator omits.
    let mut symbols: Vec<Symbol> = Vec::new();
    let mut place = |index: usize, symbol: Symbol| {
        if symbols.len() <= index {
            symbols.resize_with(index + 1, || Symbol {
                name: String::new(),
                role: SymbolRole::Other,
                binding: SymbolBinding::Local,
                hidden: false,
                section: None,
                offset: 0,
                size: 0,
                defined: false,
            });
        }
        symbols[index] = symbol;
    };

    for symbol in file.symbols() {
        let role = match symbol.kind() {
            object::SymbolKind::Text => SymbolRole::Function,
            object::SymbolKind::Data => SymbolRole::Data,
            object::SymbolKind::Section => SymbolRole::Section,
            _ => SymbolRole::Other,
        };
        let binding = if symbol.is_weak() {
            SymbolBinding::Weak
        } else if symbol.is_global() {
            SymbolBinding::Global
        } else {
            SymbolBinding::Local
        };
        let section = symbol
            .section_index()
            .and_then(|index| section_of_elf_index.get(&index).copied());
        // Section symbols carry no name of their own in ELF; borrowing the
        // section's name keeps dumps and diagnostics readable.
        let name = match symbol.name() {
            Ok(name) if !name.is_empty() => name.to_string(),
            _ => symbol
                .section_index()
                .and_then(|index| file.section_by_index(index).ok())
                .and_then(|section| section.name().ok().map(str::to_string))
                .unwrap_or_default(),
        };
        place(
            symbol.index().0,
            Symbol {
                name,
                role,
                binding,
                hidden: matches!(symbol.scope(), object::SymbolScope::Compilation)
                    && !symbol.is_local(),
                section,
                offset: symbol.address(),
                size: symbol.size(),
                defined: symbol.is_definition() || matches!(role, SymbolRole::Section),
            },
        );
    }
    Ok(symbols)
}

fn read_relocations(
    file: &object::read::File<'_>,
    section_of_elf_index: &std::collections::HashMap<SectionIndex, usize>,
    sections: &mut [Section],
) -> Result<()> {
    for section in file.sections() {
        let Some(&target) = section_of_elf_index.get(&section.index()) else {
            continue;
        };
        if sections[target].role == SectionRole::Untranslated {
            continue;
        }
        for (offset, relocation) in section.relocations() {
            let RelocationFlags::Elf { r_type } = relocation.flags() else {
                bail!("unexpected non-ELF relocation in {}", sections[target].name);
            };
            let Some(kind) = RelocationKind::from_elf_type(r_type.0) else {
                bail!(
                    "unsupported relocation type {} at {}+{offset:#x}",
                    r_type.0,
                    sections[target].name
                );
            };
            let object::RelocationTarget::Symbol(SymbolIndex(symbol)) = relocation.target() else {
                bail!(
                    "relocation at {}+{offset:#x} does not target a symbol",
                    sections[target].name
                );
            };
            sections[target].relocations.push(Relocation {
                offset,
                kind,
                symbol,
                addend: relocation.addend(),
            });
        }
        sections[target]
            .relocations
            .sort_by_key(|relocation| relocation.offset);
    }
    Ok(())
}

/// The `PT_LOAD` segments, in the order the program headers give them.
fn read_segments(file: &object::read::File<'_>) -> Vec<Segment> {
    use object::read::ObjectSegment;
    file.segments()
        .map(|segment| {
            let flags: u32 = match segment.flags() {
                object::SegmentFlags::Elf { p_flags, .. } => p_flags.0,
                _ => 0,
            };
            let bytes = segment.data().unwrap_or(&[]).to_vec();
            Segment {
                address: segment.address(),
                // `segment.size()` is the *memory* size; what is in the file
                // is what `data` hands back, and the difference is `.bss` —
                // which the loader zeroes rather than copies.
                file_size: bytes.len() as u64,
                memory_size: segment.size(),
                bytes,
                readable: flags & 4 != 0,
                writable: flags & 2 != 0,
                executable: flags & 1 != 0,
                alignment: segment.align().max(1),
            }
        })
        .collect()
}

fn collect_functions(
    symbols: &[Symbol],
    sections: &[Section],
    layout: Layout,
) -> Result<Vec<Function>> {
    // A linked executable's symbol table is a weaker witness than a fresh
    // object's, so its unwind tables are read as a second one: they say
    // where each function begins and how long it is, which is the whole of
    // what discovery needs. See `crate::eh_frame`.
    let unwind = if layout == Layout::Linked {
        unwind_extents(sections)?
    } else {
        std::collections::BTreeMap::new()
    };
    // Where the next thing starts, per text section. A function whose symbol
    // states no size still has an upper bound: it cannot run past whatever
    // begins after it. This is the third and weakest witness, used only when
    // the other two say nothing — `crtbegin.o`'s stubs
    // (`deregister_tm_clones` and its neighbours) are hand-written enough to
    // carry neither a `.size` nor an unwind entry, and they are in every
    // binary gcc links.
    let boundaries = symbol_boundaries(symbols, sections);

    let mut functions = Vec::new();
    for (index, symbol) in symbols.iter().enumerate() {
        if symbol.role != SymbolRole::Function || !symbol.defined {
            continue;
        }
        let Some(section) = symbol.section else {
            continue;
        };
        if sections[section].role != SectionRole::Text {
            continue;
        }
        // Hand-written assembly routinely omits `.size`, and a symbol
        // without one says only where a function starts. The unwind table
        // knows how long it is.
        let size = match symbol.size {
            0 => unwind
                .get(&(sections[section].address + symbol.offset))
                .copied()
                .or_else(|| {
                    next_boundary(&boundaries, section, symbol.offset)
                        .map(|next| next - symbol.offset)
                })
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "function symbol `{}` has zero size, no unwind entry and \
                         nothing after it to bound it; the transpiler needs \
                         function extents",
                        symbol.name
                    )
                })?,
            size => size,
        };
        if size == 0 {
            bail!(
                "function symbol `{}` is bounded to nothing by the symbol that \
                 follows it",
                symbol.name
            );
        }
        let end = symbol.offset + size;
        if end > sections[section].bytes.len() as u64 {
            bail!(
                "function `{}` extends past the end of {}",
                symbol.name,
                sections[section].name
            );
        }
        functions.push(Function {
            name: symbol.name.clone(),
            symbol: Some(index),
            section,
            offset: symbol.offset,
            size,
        });
    }
    functions.sort_by_key(|function| (function.section, function.offset));

    // Whatever the symbols did not account for. A static function the
    // compiler never named, or every function in a stripped binary, reaches
    // the translator this way and no other.
    let mut discovered = Vec::new();
    for (&address, &length) in &unwind {
        let Some((section, offset)) = section_holding(sections, address) else {
            continue;
        };
        if sections[section].role != SectionRole::Text {
            continue;
        }
        if offset + length > sections[section].bytes.len() as u64 {
            bail!(
                "an unwind entry at {address:#x} extends past {}",
                sections[section].name
            );
        }
        // A symbol that already covers this start is the better name for it,
        // and one that covers part of the range means the symbols split what
        // the unwind table describes as a whole — in which case they are the
        // finer answer and this one is redundant.
        let covered = functions.iter().any(|function| {
            function.section == section
                && function.offset < offset + length
                && offset < function.offset + function.size
        });
        if covered {
            continue;
        }
        discovered.push(Function {
            // Named after where it is, since nothing named it.
            name: format!("fn.{address:#x}"),
            symbol: None,
            section,
            offset,
            size: length,
        });
    }
    functions.extend(discovered);

    // And the procedure linkage table, whose entries are functions that no
    // symbol names and no unwind entry describes.
    //
    // A *static* executable still has one: `R_X86_64_IRELATIVE` relocations
    // are how an ifunc is resolved, and `__libc_start_main` walks them at
    // startup, calls each resolver, and writes the answer into the slot the
    // stub jumps through. So `memcpy` and `strlen` are reached through the
    // table like anything else, and without these the calls to them resolve
    // to nothing.
    if layout == Layout::Linked {
        for (index, section) in sections.iter().enumerate() {
            if !is_linkage_table(&section.name) || section.role != SectionRole::Text {
                continue;
            }
            let length = section.bytes.len() as u64;
            // The section's alignment is the linker's own statement of how
            // long an entry is, and it is not a constant: a stub is sixteen
            // bytes when control-flow enforcement puts an `endbr64` in front
            // of the jump and eight when it does not. Nothing else in the
            // file says, and an entry size guessed wrong would split the
            // table into functions that begin in the middle of an
            // instruction.
            let stride = section.alignment.max(1);
            if length % stride != 0 {
                bail!(
                    "{} is {length:#x} bytes, which is not a whole number of \
                     {stride:#x}-byte entries",
                    section.name
                );
            }
            for offset in (0..length).step_by(stride as usize) {
                functions.push(Function {
                    name: format!("plt.{:#x}", section.address + offset),
                    symbol: None,
                    section: index,
                    offset,
                    size: stride,
                });
            }
        }
    }

    functions.sort_by_key(|function| (function.section, function.offset));
    Ok(functions)
}

/// The sections whose contents are linkage-table entries rather than
/// ordinary code.
///
/// `.plt` is the classic one, `.iplt` holds ifunc stubs where a linker
/// separates them, and `.plt.sec`/`.plt.got` are the forms produced with
/// control-flow enforcement and with early binding.
fn is_linkage_table(name: &str) -> bool {
    matches!(name, ".plt" | ".iplt" | ".plt.sec" | ".plt.got")
}

/// Every offset something begins at, per text section, sorted.
///
/// Only *defined* symbols with a place, since an undefined one bounds
/// nothing. A section symbol is skipped: it sits at offset zero and would
/// bound the first function to nothing.
fn symbol_boundaries(
    symbols: &[Symbol],
    sections: &[Section],
) -> std::collections::HashMap<usize, Vec<u64>> {
    let mut boundaries: std::collections::HashMap<usize, Vec<u64>> =
        std::collections::HashMap::new();
    for symbol in symbols {
        let Some(section) = symbol.section else {
            continue;
        };
        if !symbol.defined
            || symbol.role == SymbolRole::Section
            || sections[section].role != SectionRole::Text
        {
            continue;
        }
        boundaries.entry(section).or_default().push(symbol.offset);
    }
    for (section, offsets) in &mut boundaries {
        // The section's own end bounds the last function in it.
        offsets.push(sections[*section].bytes.len() as u64);
        offsets.sort_unstable();
        offsets.dedup();
    }
    boundaries
}

/// The first offset strictly after `offset` in the same section.
fn next_boundary(
    boundaries: &std::collections::HashMap<usize, Vec<u64>>,
    section: usize,
    offset: u64,
) -> Option<u64> {
    let offsets = boundaries.get(&section)?;
    let index = offsets.partition_point(|candidate| *candidate <= offset);
    offsets.get(index).copied()
}

/// Splits any function something branches *into* at the point it is entered.
///
/// A function can have more than one entry. gcc's hot/cold splitting is the
/// common source: it collects several of a function's cold exits into one
/// `.cold` fragment, gives the whole fragment a single symbol, and has the
/// hot code jump to each exit individually — so `execute_stack_op.cold` is
/// ten bytes holding two independent `call abort` stubs, and the jump to the
/// second one lands five bytes in. glibc's `memmove` variants do the same,
/// sharing tails between implementations.
///
/// Splitting is sound here for a reason particular to this design: a
/// function boundary carries no live register state. The machine file lives
/// in globals and is promoted into locals inside a body with a flush at
/// every call and exit, so entering a piece reloads and leaving flushes —
/// which is what already happens at every boundary. The guest stack is
/// ordinary memory and is shared either way.
///
/// Only instruction boundaries are cut at. A target *inside* an instruction
/// is a second instruction stream, which is a different question — see
/// `crate::cfg`'s handling of a branch past a `lock` prefix.
fn split_at_interior_entries(sections: &[Section], functions: &mut Vec<Function>) -> Result<()> {
    use std::collections::{BTreeSet, HashMap};

    // Where each function's instructions begin, and everywhere anything
    // branches to. One decode pass over the text answers both.
    // Targets carry the function they came *from*: a function's own
    // branches are ordinary control flow and say nothing about entries.
    let mut starts: HashMap<usize, BTreeSet<u64>> = HashMap::new();
    let mut targets: HashMap<usize, Vec<(u64, usize)>> = HashMap::new();
    for (index, function) in functions.iter().enumerate() {
        let section = &sections[function.section];
        let from = function.offset as usize;
        let to = from + function.size as usize;
        if to > section.bytes.len() {
            continue;
        }
        let mut decoder = iced_x86::Decoder::with_ip(
            64,
            &section.bytes[from..to],
            function.offset,
            iced_x86::DecoderOptions::NONE,
        );
        let mut boundaries = BTreeSet::new();
        while decoder.can_decode() {
            let instruction = decoder.decode();
            boundaries.insert(instruction.ip());
            if instruction.op0_kind() == iced_x86::OpKind::NearBranch64
                && matches!(
                    instruction.flow_control(),
                    iced_x86::FlowControl::ConditionalBranch
                        | iced_x86::FlowControl::UnconditionalBranch
                )
            {
                targets
                    .entry(function.section)
                    .or_default()
                    .push((instruction.near_branch64(), index));
            }
        }
        starts.insert(index, boundaries);
    }

    // A target that lands inside a function but not at its start is an entry
    // the symbol table did not describe.
    let mut cuts: HashMap<usize, BTreeSet<u64>> = HashMap::new();
    for (index, function) in functions.iter().enumerate() {
        let (Some(section_targets), Some(boundaries)) =
            (targets.get(&function.section), starts.get(&index))
        else {
            continue;
        };
        for (target, from) in section_targets {
            // A branch that stays inside its own body is ordinary control
            // flow. Compared by extent rather than by index, because one
            // body often carries several symbols: `__memcpy_avx_unaligned`
            // and `__memmove_avx_unaligned` are the same ten instructions
            // under two names, and each one's loops would otherwise look
            // like entries into the other.
            let source = &functions[*from];
            if (source.offset..source.offset + source.size).contains(target) {
                continue;
            }
            let interior = (function.offset + 1..function.offset + function.size).contains(target);
            if interior && boundaries.contains(target) {
                cuts.entry(index).or_default().insert(*target);
            }
        }
    }
    if cuts.is_empty() {
        return Ok(());
    }

    let mut split = Vec::with_capacity(functions.len());
    for (index, function) in functions.iter().enumerate() {
        let Some(points) = cuts.get(&index) else {
            split.push(function.clone());
            continue;
        };
        let mut start = function.offset;
        let end = function.offset + function.size;
        for point in points.iter().copied().chain(std::iter::once(end)) {
            split.push(Function {
                // The first piece keeps the symbol and the name; the rest
                // are named after where they begin, because nothing named
                // them.
                name: if start == function.offset {
                    function.name.clone()
                } else {
                    format!("{}+{:#x}", function.name, start - function.offset)
                },
                symbol: if start == function.offset {
                    function.symbol
                } else {
                    None
                },
                section: function.section,
                offset: start,
                size: point - start,
            });
            start = point;
        }
    }
    split.sort_by_key(|function| (function.section, function.offset));
    *functions = split;
    Ok(())
}

/// Which section holds a virtual address, and where in it.
fn section_holding(sections: &[Section], address: u64) -> Option<(usize, u64)> {
    sections.iter().enumerate().find_map(|(index, section)| {
        let size = section.size.max(section.bytes.len() as u64);
        (section.address != 0 && address >= section.address && address < section.address + size)
            .then(|| (index, address - section.address))
    })
}

/// The function extents every `.eh_frame` section in the file describes,
/// keyed by virtual address.
fn unwind_extents(sections: &[Section]) -> Result<std::collections::BTreeMap<u64, u64>> {
    let mut extents = std::collections::BTreeMap::new();
    for section in sections {
        if section.name != ".eh_frame" || section.bytes.is_empty() {
            continue;
        }
        for frame in crate::eh_frame::frames(&section.bytes, section.address)? {
            // Two entries for one address would mean the table disagrees
            // with itself; the longer extent is the safe reading, since
            // translating too little leaves a tail nothing reaches.
            let extent = extents.entry(frame.address).or_insert(frame.length);
            *extent = (*extent).max(frame.length);
        }
    }
    Ok(extents)
}
