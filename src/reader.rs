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
    /// A complete executable or shared object: sections at their virtual
    /// addresses, and every internal reference already the number it
    /// resolves to.
    ///
    /// A position-independent one carries those addresses relative to a base
    /// of zero, and the base is chosen at bake time — see
    /// [`ObjectFile::parse_at`]. Once it is chosen the two cases are the
    /// same case, which is the whole reason prelinking is where the design
    /// puts it.
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

pub use crate::discover::Function;

/// One input ELF inside a (possibly merged) object.
///
/// A dynamic program is several ELFs — the executable, its interpreter, and
/// every library between them — and every one of them has to be translated,
/// with one exec map spanning the lot. [`ObjectFile::merge`] is what makes
/// that one translation unit; this is what remembers which part of it came
/// from where, which the bake needs to place each file's bytes and to route
/// each patch back to the file it belongs to.
#[derive(Clone, Debug)]
pub struct Module {
    /// How the module is named in diagnostics and in symbol names — its path
    /// in the image, for a bake.
    pub name: String,
    /// The address its own addresses were translated at. Zero for a
    /// fixed-address executable, which states its own.
    pub base: u64,
    /// Its ELF entry point, already at `base`.
    pub entry: u64,
    /// One past the highest address any of its segments occupies.
    pub top: u64,
    /// Which of the merged object's sections this module contributed.
    pub sections: std::ops::Range<usize>,
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
    /// The inputs this object was built from, in order. Empty for a
    /// relocatable object, which is not loaded and has no base; one entry
    /// for a single linked file; one per file after [`ObjectFile::merge`].
    pub modules: Vec<Module>,
}

impl ObjectFile {
    /// Reads a file that states its own addresses, or has none to state.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        Self::parse_at(bytes, 0)
    }

    /// Reads a linked file as though a loader had placed it at `base`.
    ///
    /// This is the whole of "prelink at bake" on the reading side. A
    /// position-independent file — which nearly everything shipped is —
    /// carries its addresses relative to zero and expects a loader to add a
    /// base it chooses at run time. The design chooses it at bake time
    /// instead, and translates the file *at* that base, so that every
    /// internal reference is a concrete address exactly as a fixed-address
    /// executable's is. Nothing downstream then has to know which of the two
    /// it is looking at: there is no `module_base +` arithmetic anywhere,
    /// because the arithmetic was done once, here.
    ///
    /// The base is added to everything the loader would have added it to —
    /// allocated sections, segments, the entry point, and the addends of the
    /// relocations discovery harvests — and to nothing else. `base` must be
    /// zero for a relocatable object, which is not loaded at all.
    ///
    /// **A low base costs discovery quality, and silently.** A
    /// position-independent file's text begins a few kilobytes above its
    /// base, and at a low base that is exactly where ordinary integer
    /// constants live — so `mov $0x1770,%eax` reads as an instruction taking
    /// the address of code, and the operand harvest cannot tell the two
    /// apart. Measured on this machine's `ld-linux-x86-64.so.2`: read at
    /// zero, eleven address-taken functions against three at a real base,
    /// and the eight extra ones shredded a region no strong witness covered
    /// into pieces beginning partway through real instructions.
    ///
    /// This is not checked here, and the reason is that there is nothing to
    /// check: for a position-independent file the base is not the file's
    /// property but ours, chosen at bake time, so a bad one is a bake that
    /// chose badly rather than an input that arrived badly. The floor lives
    /// where the choice is made — `baker::layout::DYNAMIC_BASE`. What is
    /// right here is to say what the choice costs.
    pub fn parse_at(bytes: &[u8], base: u64) -> Result<Self> {
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
            // Both shapes of "already linked". A fixed-address executable
            // states its own addresses and is read at a base of zero; a
            // shared object or position-independent executable states them
            // relative to zero and is read at the base the bake assigned it.
            // The difference ends here.
            object::ObjectKind::Executable | object::ObjectKind::Dynamic => Layout::Linked,
            other => bail!(
                "expected a relocatable object (`gcc -c`) or a linked \
                 executable or shared object, found {other:?}"
            ),
        };
        if base != 0 && layout != Layout::Linked {
            bail!("a relocatable object has nothing to place, so it has no base");
        }

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
                // Only what the loader places moves. A section with no
                // `SHF_ALLOC` — a symbol table, debug information — has no
                // address at all, and giving it one would make every address
                // fall inside the first of them.
                address: match allocated(&section) {
                    true => section.address() + base,
                    false => section.address(),
                },
                bytes,
                size: section.size(),
                alignment: section.align().max(1),
                relocations: Vec::new(),
            });
        }

        let mut symbols = read_symbols(&file, &section_of_elf_index, layout)?;
        // Only a relocatable object has relocations this pipeline reads. A
        // linked executable's are *dynamic* — `IRELATIVE` for its ifuncs,
        // and whatever else a loader applies — and they say nothing about
        // the translation, because the code has already been placed and a
        // reference in it is the address it means. A static glibc program
        // applies its own, walking `__rela_iplt_start` from `_start`, which
        // is the guest's business and not the reader's.
        //
        // Reading them anyway was not harmless: a dynamic relocation type
        // this does not model stopped the parse, so a stripped static
        // busybox could not be opened at all.
        if layout != Layout::Linked {
            read_relocations(&file, &section_of_elf_index, &mut sections)?;
        }
        if layout == Layout::Linked {
            // A symbol's address is a virtual address once something has
            // been placed. The rest of the pipeline works in offsets within
            // a section, so subtract the section's own address — which for a
            // relocatable object is zero, and this is why it can be done in
            // one place rather than at every use.
            for symbol in &mut symbols {
                if let Some(section) = symbol.section {
                    symbol.offset = (symbol.offset + base).saturating_sub(sections[section].address);
                }
            }
        }
        let entry = match layout {
            Layout::Linked => file.entry() + base,
            Layout::Relocatable => 0,
        };
        let evidence = crate::discover::FileEvidence {
            base,
            entry,
            relocated: harvest_relocation_targets(&file, base),
        };
        let functions = crate::discover::discover(&symbols, &sections, layout, &evidence)?;
        let segments = read_segments(&file, base);

        let modules = match layout {
            Layout::Linked => vec![Module {
                name: String::new(),
                base,
                entry,
                top: segments
                    .iter()
                    .map(|segment| segment.address + segment.memory_size)
                    .max()
                    .unwrap_or(0),
                sections: 0..sections.len(),
            }],
            Layout::Relocatable => Vec::new(),
        };

        let mut object = Self {
            layout,
            entry,
            segments,
            sections,
            symbols,
            functions,
            modules,
        };
        // Discovery is not finished until the passes downstream of it stop
        // producing evidence it needed. Run here, at the one place an object
        // comes into existence, because a function list that a later pass
        // would still revise must never be observable — see
        // [`crate::frontend::settle`].
        //
        // A merge needs none of its own: it concatenates modules whose
        // sections stay distinct, and a jump table's arms are instruction
        // boundaries *in the dispatching function's own section*
        // (`read_linked_table`), so no arm can cross a module boundary and
        // nothing a merge does can strand one.
        crate::frontend::settle(&mut object)?;
        Ok(object)
    }

    /// One translation unit out of several linked files.
    ///
    /// A dynamic program is not one ELF. It is the executable, its
    /// interpreter, and every library between them, and every one of them
    /// has to be translated. They could be translated separately — but the
    /// exec map could not be built separately, and the exec map is the
    /// whole mechanism: in a linked file a function pointer is a virtual
    /// address, so *every* indirect transfer, including every cross-module
    /// call through a `GOT` slot the loader wrote, is a lookup in it. One
    /// map means one sorted table, which means one object defining it.
    ///
    /// The merge is possible at all because prelinking already made the
    /// addresses disjoint and concrete. Sections keep their own addresses,
    /// so `section_at` still answers; functions keep their own extents;
    /// nothing has to be renumbered but the indices this struct uses
    /// internally.
    ///
    /// Names are qualified by module. Two libraries define `memcpy`, and
    /// each defined function becomes a wasm symbol — so unqualified names
    /// would collide at the link, loudly for the global ones and, worse,
    /// silently in every diagnostic that says which function stopped.
    pub fn merge(inputs: Vec<(String, ObjectFile)>) -> Result<Self> {
        if inputs.is_empty() {
            bail!("a merge needs at least one file");
        }
        let mut sections: Vec<Section> = Vec::new();
        let mut symbols: Vec<Symbol> = Vec::new();
        let mut functions: Vec<Function> = Vec::new();
        let mut segments: Vec<Segment> = Vec::new();
        let mut modules: Vec<Module> = Vec::new();
        let mut entry = 0;

        for (name, input) in inputs {
            if input.layout != Layout::Linked {
                bail!("`{name}` is not a linked file, so it cannot be placed beside one");
            }
            let [module] = &input.modules[..] else {
                bail!("`{name}` is itself a merge, and merges do not nest");
            };
            if entry == 0 {
                entry = input.entry;
            }
            let first_section = sections.len();
            let first_symbol = symbols.len();
            for mut function in input.functions {
                if !name.is_empty() {
                    function.name = format!("{name}!{}", function.name);
                }
                function.section += first_section;
                function.symbol = function.symbol.map(|index| index + first_symbol);
                functions.push(function);
            }
            modules.push(Module {
                name,
                base: module.base,
                entry: module.entry,
                top: module.top,
                sections: first_section..first_section + input.sections.len(),
            });
            sections.extend(input.sections);
            symbols.extend(input.symbols);
            segments.extend(input.segments);
        }

        Ok(Self {
            layout: Layout::Linked,
            entry,
            segments,
            sections,
            symbols,
            functions,
            modules,
        })
    }

    /// Which module holds a virtual address.
    pub fn module_at(&self, address: u64) -> Option<&Module> {
        self.modules
            .iter()
            .find(|module| address >= module.base && address < module.top)
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

/// Whether the loader places this section, which is what decides whether a
/// base applies to its address.
fn allocated(section: &object::read::Section<'_, '_>) -> bool {
    match section.flags() {
        object::SectionFlags::Elf { sh_flags, .. } => {
            sh_flags.0 & u64::from(object::elf::SHF_ALLOC.0) != 0
        }
        _ => false,
    }
}

fn read_symbols(
    file: &object::read::File<'_>,
    section_of_elf_index: &std::collections::HashMap<SectionIndex, usize>,
    layout: Layout,
) -> Result<Vec<Symbol>> {
    // Relocations name symbols by their raw ELF symbol-table index, so the
    // vector must be indexed the same way — including the reserved null
    // symbol at index 0, which the `object` crate's iterator omits.
    let mut symbols: Vec<Symbol> = Vec::new();
    fn place(symbols: &mut Vec<Symbol>, index: usize, symbol: Symbol) {
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
    }

    let convert = |symbol: &object::read::Symbol<'_, '_>| -> Symbol {
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
        }
    };

    for symbol in file.symbols() {
        place(&mut symbols, symbol.index().0, convert(&symbol));
    }

    // `.dynsym` as well, for a linked file. The strong-witness argument is
    // the same as `.symtab`'s — an `STT_FUNC` entry with a value and a size
    // is the format saying a function starts there — and it is the *only*
    // symbol table a stripped shared object has, because linking against one
    // requires it. Nearly everything shipped is stripped and dynamic, so
    // this is not a corner: it is where the symbols are.
    //
    // Appended past the static table rather than merged into it, because a
    // dynamic symbol's index belongs to a different table and only
    // `.symtab` indices are ever named by a relocation this reader reads —
    // and it reads none at all for a linked file. Duplicate starts are the
    // expected case, not an error: `discover` records the widest extent and
    // moves on, exactly as it does for the alias pairs `.symtab` already
    // carries.
    if layout == Layout::Linked {
        let first = symbols.len().max(1);
        for symbol in file.dynamic_symbols() {
            place(&mut symbols, first + symbol.index().0, convert(&symbol));
        }
    }
    Ok(symbols)
}

/// The addresses a linked file's dynamic relocations name, for discovery.
///
/// [`read_relocations`] reads relocations to *translate* them, which a linked
/// executable does not need — the code is placed and an operand is the
/// address it means. These say nothing about translation and a great deal
/// about where the functions are:
///
/// - Every `R_X86_64_IRELATIVE` addend is an ifunc **resolver**, a real
///   function that startup code calls through a pointer read out of
///   relocation data. In a stripped static glibc the resolvers have no
///   symbol, no unwind entry, and no instruction anywhere naming their
///   address.
/// - Every `R_X86_64_RELATIVE` addend is a code or data pointer the linker
///   marked exactly. A non-PIE static executable has none; a static-PIE one
///   has an entry for every pointer it embeds, which turns the hardest case
///   in `docs/code-discovery.md` into a table walk.
///
/// Whether either lands in code is [`crate::discover`]'s question, not this
/// one — this only harvests.
///
/// **Unmodelled types are skipped, never fatal.** This is a lesson the
/// worklog records: refusing a relocation type the pipeline does not model
/// made a stripped busybox unopenable, because the read is harvesting
/// evidence rather than interpreting the file.
fn harvest_relocation_targets(file: &object::read::File<'_>, base: u64) -> Vec<u64> {
    let mut targets = Vec::new();
    let mut take = |relocation: &object::Relocation| {
        let RelocationFlags::Elf { r_type } = relocation.flags() else {
            return;
        };
        if !matches!(
            r_type,
            object::elf::R_X86_64_IRELATIVE | object::elf::R_X86_64_RELATIVE
        ) {
            return;
        }
        // The addend is the address for both: what the resolver is, or
        // what the pointer points at. A negative one is not an address.
        if let Ok(address) = u64::try_from(relocation.addend()) {
            // Relative to the load base, in a file that has one: what a
            // loader would write is `base + addend`, and that is the
            // address the code will be at.
            targets.push(address + base);
        }
    };
    for section in file.sections() {
        for (_, relocation) in section.relocations() {
            take(&relocation);
        }
    }
    // And the *dynamic* relocations, which are a different table reached a
    // different way — a linked file's `.rela.dyn` is not attached to a
    // target section the way a relocatable object's `.rela.text` is, so the
    // loop above sees none of it.
    //
    // Skipping them cost every position-independent file its best witness.
    // A PIE embeds a `R_X86_64_RELATIVE` for every pointer it holds, which
    // is exactly the set of addresses nothing in the instruction stream
    // names: `main`, handed to `__libc_start_main` through a GOT slot, is
    // the first one every program needs and the one whose absence stopped
    // every stripped coreutil on this machine.
    if let Some(relocations) = file.dynamic_relocations() {
        for (_, relocation) in relocations {
            take(&relocation);
        }
    }
    targets
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
fn read_segments(file: &object::read::File<'_>, base: u64) -> Vec<Segment> {
    use object::read::ObjectSegment;
    file.segments()
        .map(|segment| {
            let flags: u32 = match segment.flags() {
                object::SegmentFlags::Elf { p_flags, .. } => p_flags.0,
                _ => 0,
            };
            let bytes = segment.data().unwrap_or(&[]).to_vec();
            Segment {
                address: segment.address() + base,
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
