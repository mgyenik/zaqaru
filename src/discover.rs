//! Where the functions are: the witness model.
//!
//! A linked executable has to be split into functions before any of it can
//! be translated — translation is per function, the exec map is keyed by
//! function entry, and an extent decides where decoding stops. Nothing in a
//! stripped binary is *required* to say where the functions are, and there
//! is no sound algorithm that recovers them; the only soundness results in
//! the literature buy soundness by restricting the input to position-
//! independent executables, which this project cannot do because the target
//! is any binary.
//!
//! What there is instead is evidence, of several kinds, combined under a
//! rule. Each kind is a [`Witness`]. The rule is the one thing in this
//! module that matters, and it follows from the two failure directions
//! being asymmetric:
//!
//! - **A missed function fails loudly.** Every indirect transfer goes
//!   through the exec map, and an address the map does not hold is a named
//!   runtime error carrying that address. The fix is a re-bake.
//! - **A false function start fails silently.** A start invented inside a
//!   real function bounds that function short — its tail cut off, or split
//!   at something that is not an instruction boundary — and nothing says so
//!   until wrong bytes execute. This project has no detector for it.
//!
//! So the bar for evidence that may *bound* a function is absolute, and the
//! bar for evidence that may merely *add* one is much lower: an added
//! function that is wrong is dead code nothing ever calls, which costs bytes
//! and not correctness. [`Coverage`] is where that distinction lives, as two
//! doors rather than as a convention each pass remembers — `establish` for
//! strong evidence, `fill` for weak, and the type refuses a `fill` into
//! covered code so that no caller can get it wrong.
//!
//! See `docs/code-discovery.md`, which is the design authority; where it and
//! this module disagree, it wins.

use anyhow::{Result, bail};

use crate::reader::{Layout, Section, SectionRole, Symbol, SymbolRole};

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
    /// What said this function is here. Carried so that a refusal or a
    /// runtime miss can name its evidence rather than only its address.
    pub witness: Witness,
}

/// What said a function is here.
///
/// Recorded per function so that a discovery refusal or a runtime exec-map
/// miss can name its evidence: "reached `fn.0x511aa5`, discovered by data
/// array" is diagnosable where an address alone is an afternoon.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Witness {
    /// An `STT_FUNC` symbol: a name, an address, and usually a size.
    Symbol,
    /// A `.eh_frame` frame description entry: a start and an extent. What a
    /// stripped binary has instead of a symbol table.
    UnwindEntry,
    /// A slot of a linkage table, at the stride the section's alignment
    /// states. Every slot is a stub; no symbol names them and no unwind
    /// entry describes them.
    LinkageTable,
    /// An entry in `.init_array`, `.preinit_array` or `.fini_array`. The ABI
    /// defines these to hold pointers to functions and the C runtime calls
    /// through them.
    InitialiserArray,
    /// Named by the file itself: `e_entry`; the `.init` and `.fini`
    /// sections, which the ABI defines as holding the runtime's initialiser
    /// and finaliser; or the addend of an `R_X86_64_IRELATIVE` or
    /// `R_X86_64_RELATIVE` relocation. The kernel transfers to the first,
    /// the loader calls the second through `DT_INIT`, the startup code
    /// calls an ifunc resolver, and the last is a pointer the linker marked
    /// exactly.
    FileStated,
    /// A direct call or jump from code already discovered.
    Transfer,
    /// An instruction takes this address: a program-counter-relative `lea`,
    /// or an immediate in non-position-independent code. How a callback is
    /// registered before anything calls it.
    AddressTaken,
    /// Cut out of a function that something branched into partway. The piece
    /// before it keeps its original witness.
    InteriorEntry,
}

impl Witness {
    /// Whether this witness may define boundaries — start a function, state
    /// its extent, and bound the function before it.
    ///
    /// Strong evidence is what the ELF format or the psABI *defines* to mean
    /// "a function starts here". Weak evidence is an observation that code
    /// appears to be at an address, which is worth acting on only where
    /// nothing better has spoken: see [`Coverage::fill`].
    pub fn is_strong(self) -> bool {
        match self {
            Witness::Symbol
            | Witness::UnwindEntry
            | Witness::LinkageTable
            | Witness::InitialiserArray
            | Witness::FileStated => true,
            Witness::Transfer | Witness::AddressTaken => false,
            // Not evidence of its own: a cut redistributes a function that
            // strong or weak evidence had already established, and the
            // pieces inherit whatever standing it had.
            Witness::InteriorEntry => false,
        }
    }
}

/// Why a [`Coverage::fill`] was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// Something already covers the address. A weak witness may not split,
    /// shorten or bound what strong evidence established, so the only
    /// correct response is to drop it — see the module header.
    AlreadyCovered,
    /// The address is not inside a section holding code.
    NotCode,
    /// The bytes there are what a linker puts *between* functions, and
    /// padding is never a function whatever named it.
    Padding,
    /// The bound the next known start gives leaves no bytes.
    Empty,
}

/// Every function found so far, and which bytes they account for.
///
/// The one type that owns the fact the invariant is about. Before it, the
/// coverage and start-set bookkeeping was rebuilt ad hoc by each pass and
/// the gap-only rule was a convention each re-implemented — which is the
/// shape a rule gets broken in. Here the permission lives in the API: there
/// are two doors, and a weak witness cannot reach the one that bounds.
#[derive(Default)]
pub struct Coverage {
    /// In insertion order, which the pipeline sorts where it means to.
    functions: Vec<Function>,
    /// Per section, every function's extent, keyed by start. A `BTreeMap`
    /// rather than a sorted vector because the containment query wants the
    /// greatest start not exceeding an offset, which is one lookup.
    extents: std::collections::HashMap<usize, std::collections::BTreeMap<u64, u64>>,
}

impl Coverage {
    /// Strong evidence: this function is here, and it may bound.
    ///
    /// Nothing is inserted where a function already starts or where one
    /// already covers the address — two strong witnesses describing the same
    /// bytes agree far more often than they disagree, and the caller that
    /// cares about a disagreement is [`collect_functions`], which resolves
    /// it by the documented precedence before ever reaching this door.
    ///
    /// The permission to *cut* an established function is what makes this
    /// door different from [`Self::fill`], and no witness exercises it
    /// today: the strong witnesses all state extents directly.
    pub fn establish(&mut self, sections: &[Section], function: Function) -> bool {
        debug_assert!(
            function.witness.is_strong(),
            "`{}` reached the strong door with the weak witness {:?}",
            function.name,
            function.witness
        );
        if starts_on_padding(sections, &function) {
            return false;
        }
        self.insert(function)
    }

    /// Weak evidence: this function is here, if nothing better has spoken.
    ///
    /// Refused where anything already covers the address. That refusal is
    /// the invariant, and it is the type's job rather than the caller's
    /// because a convention is a bug each caller is invited to write.
    pub fn fill(&mut self, sections: &[Section], function: Function) -> Result<(), Refused> {
        if self.covers(function.section, function.offset) {
            return Err(Refused::AlreadyCovered);
        }
        if function.size == 0 {
            return Err(Refused::Empty);
        }
        if starts_on_padding(sections, &function) {
            return Err(Refused::Padding);
        }
        self.insert(function);
        Ok(())
    }

    fn insert(&mut self, function: Function) -> bool {
        let extents = self.extents.entry(function.section).or_default();
        let end = function.offset + function.size;
        // Two functions may share a start, and dropping the second would be
        // wrong: `__libc_start_main` and `__libc_start_main_impl` are one
        // body under two symbols, and a linked glibc has hundreds of such
        // pairs. What is recorded once is the *coverage* — the further of
        // the two extents, since either name reaches all of it.
        extents
            .entry(function.offset)
            .and_modify(|known| *known = (*known).max(end))
            .or_insert(end);
        self.functions.push(function);
        true
    }

    /// Whether any function already accounts for this byte.
    pub fn covers(&self, section: usize, offset: u64) -> bool {
        let Some(extents) = self.extents.get(&section) else {
            return false;
        };
        extents
            .range(..=offset)
            .next_back()
            .is_some_and(|(_, end)| offset < *end)
    }

    /// Whether any function accounts for any byte of a range.
    ///
    /// Stricter than [`Self::covers`], and the difference matters for a
    /// witness that states an extent rather than a start: an unwind entry
    /// whose range the symbols already split is redundant, even though its
    /// own first byte may be uncovered.
    pub fn overlaps(&self, section: usize, range: std::ops::Range<u64>) -> bool {
        let Some(extents) = self.extents.get(&section) else {
            return false;
        };
        if let Some((_, end)) = extents.range(..range.start).next_back()
            && *end > range.start
        {
            return true;
        }
        extents.range(range.clone()).next().is_some()
    }

    /// The first function start strictly after an offset, which is what
    /// bounds anything discovered before it.
    pub fn next_start_after(&self, section: usize, offset: u64) -> Option<u64> {
        self.extents
            .get(&section)?
            .range(offset + 1..)
            .next()
            .map(|(start, _)| *start)
    }

    /// Every maximal range of a text section that no function covers.
    ///
    /// What the saturated tier is built over, and what a diagnostic reports
    /// when a program reaches an address discovery never claimed.
    pub fn residue(&self, sections: &[Section]) -> Vec<(usize, std::ops::Range<u64>)> {
        let mut residue = Vec::new();
        for (index, section) in sections.iter().enumerate() {
            if section.role != SectionRole::Text || section.bytes.is_empty() {
                continue;
            }
            let end = section.bytes.len() as u64;
            let mut cursor = 0u64;
            if let Some(extents) = self.extents.get(&index) {
                for (start, finish) in extents {
                    if *start > cursor {
                        residue.push((index, cursor..*start));
                    }
                    cursor = cursor.max(*finish);
                }
            }
            if cursor < end {
                residue.push((index, cursor..end));
            }
        }
        residue
    }

    pub fn functions(&self) -> &[Function] {
        &self.functions
    }

    /// The functions, in insertion order. Sorting is the pipeline's to do
    /// where it means to, because the order is observable.
    pub fn finish(self) -> Vec<Function> {
        self.functions
    }
}

/// Evidence the ELF file states outright, harvested by the reader because
/// only it holds the file.
///
/// Both are strong: the format defines what they mean, and neither is an
/// inference about what code looks like.
pub struct FileEvidence {
    /// `e_entry` — the one address the kernel is defined to transfer to.
    /// Normally `_start` is found through its symbol or its frame entry, and
    /// a stripped binary with an unwind hole is obliged to provide neither.
    pub entry: u64,
    /// Addresses named by `R_X86_64_IRELATIVE` and `R_X86_64_RELATIVE`
    /// relocations — an ifunc resolver, or a pointer the linker marked
    /// exactly. See `harvest_relocation_targets` in `crate::reader`.
    pub relocated: Vec<u64>,
}

/// The pipeline, in order. See `docs/code-discovery.md`.
///
/// Strong witnesses first, so that everything they account for is covered
/// before a weak one is consulted — which is not merely tidy. The invariant
/// only bites if the strong evidence is in place when the weak evidence
/// arrives; run the other way round, a transfer target would establish a
/// function whose extent runs over the constructor an initialiser array was
/// about to name, and the array would then be refused as covered.
pub fn discover(
    symbols: &[Symbol],
    sections: &[Section],
    layout: Layout,
    evidence: &FileEvidence,
) -> Result<Vec<Function>> {
    let mut coverage = Coverage::default();
    collect_functions(&mut coverage, symbols, sections, layout)?;
    if layout == Layout::Linked {
        // The entry point and the relocation targets, which the file states
        // outright. Cheap, and the only witness a stripped binary with no
        // unwind entry for `_start` has.
        let mut stated: std::collections::BTreeSet<u64> =
            evidence.relocated.iter().copied().collect();
        if evidence.entry != 0 {
            stated.insert(evidence.entry);
        }
        for function in placements(&coverage, sections, &stated, Witness::FileStated) {
            coverage.establish(sections, function);
        }

        let arrays = initialiser_array_targets(sections);
        for function in placements(&coverage, sections, &arrays, Witness::InitialiserArray) {
            coverage.establish(sections, function);
        }
        fill_from_transfers(&mut coverage, sections)?;
    }
    let mut functions = coverage.finish();
    if layout == Layout::Linked {
        functions.sort_by_key(|function| (function.section, function.offset));
    }
    split_at_interior_entries(sections, &mut functions)?;
    make_names_unique(sections, &mut functions);
    Ok(functions)
}

/// Makes every function's name unique, because a wasm symbol's must be and
/// an ELF's need not be.
///
/// Two independent reasons a linked file names one thing twice, both of them
/// ordinary rather than exotic:
///
/// - **`.symtab` and `.dynsym` are two views of the same code.** An exported
///   function is in both, so reading both — which a stripped shared object
///   requires, since it has only the second — sees it twice at one address.
///   That is one function with one name, and the duplicate is dropped.
/// - **Symbol versioning puts one name at several addresses.** glibc ships
///   `memcpy@GLIBC_2.2.5` beside `memcpy@GLIBC_2.14`, and the version lives
///   in `.gnu.version` rather than in the name, so the symbol table says
///   `memcpy` twice about two genuinely different functions. Those are kept
///   and told apart by where they are.
///
/// Every occurrence of a colliding name is qualified, not merely the later
/// ones: which copy is "the first" depends on the order two symbol tables
/// happened to be read in, and a name that silently means a different
/// function depending on that is worse than a name with an address in it.
fn make_names_unique(sections: &[Section], functions: &mut Vec<Function>) {
    let mut seen: std::collections::HashSet<(usize, u64, String)> =
        std::collections::HashSet::new();
    functions.retain(|function| {
        seen.insert((function.section, function.offset, function.name.clone()))
    });

    let mut count: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for function in functions.iter() {
        *count.entry(function.name.as_str()).or_default() += 1;
    }
    let collides: std::collections::HashSet<String> = count
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(name, _)| name.to_string())
        .collect();
    for function in functions.iter_mut() {
        if collides.contains(&function.name) {
            let address = sections[function.section].address + function.offset;
            function.name = format!("{}.{address:#x}", function.name);
        }
    }
}

fn collect_functions(
    coverage: &mut Coverage,
    symbols: &[Symbol],
    sections: &[Section],
    layout: Layout,
) -> Result<()> {
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

    let mut functions: Vec<Function> = Vec::new();
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
            witness: Witness::Symbol,
        });
    }
    functions.sort_by_key(|function| (function.section, function.offset));
    for function in functions {
        coverage.establish(sections, function);
    }

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
        if coverage.overlaps(section, offset..offset + length) {
            continue;
        }
        discovered.push(Function {
            // Named after where it is, since nothing named it.
            name: format!("fn.{address:#x}"),
            symbol: None,
            section,
            offset,
            size: length,
            witness: Witness::UnwindEntry,
        });
    }
    for function in discovered {
        coverage.establish(sections, function);
    }

    // `.init` and `.fini`, each of which the ABI defines as holding exactly
    // one function beginning at its start — `_init` and `_fini`, the
    // loader's `DT_INIT` and `DT_FINI`.
    //
    // Not a nicety. In a *stripped dynamic* executable `_init` has no
    // symbol, and `crti.o`'s hand-written prologue carries no unwind entry
    // either, so nothing else sees it at all — while the loader calls it
    // before `main` on every run. Every stripped coreutil on this machine
    // died at the first byte of its own `.init` before this existed.
    if layout == Layout::Linked {
        for (index, section) in sections.iter().enumerate() {
            if !matches!(section.name.as_str(), ".init" | ".fini")
                || section.role != SectionRole::Text
                || section.bytes.is_empty()
            {
                continue;
            }
            coverage.establish(
                sections,
                Function {
                    name: format!("{}.{:#x}", section.name.trim_start_matches('.'), section.address),
                    symbol: None,
                    section: index,
                    offset: 0,
                    size: section.bytes.len() as u64,
                    witness: Witness::FileStated,
                },
            );
        }
    }

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
                coverage.establish(sections, Function {
                    name: format!("plt.{:#x}", section.address + offset),
                    symbol: None,
                    section: index,
                    offset,
                    size: stride,
                    witness: Witness::LinkageTable,
                });
            }
        }
    }

    Ok(())
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
/// The initialiser arrays: strong evidence, from data.
///
/// `.init_array`, `.preinit_array` and `.fini_array` are defined by the ABI
/// to hold pointers to functions, and the C runtime calls through every one
/// of them. Modern glibc's `__libc_start_main` walks `__init_array_start`,
/// so in a stripped binary the constructor it reaches has no symbol, no
/// unwind entry, and no instruction anywhere naming its address — the array
/// is the only thing in the file that says it exists.
///
/// Matched by name because that is what this reader carries; the section
/// *type* would be the stronger test, and these names are fixed by the ABI
/// rather than by convention.
fn initialiser_array_targets(sections: &[Section]) -> std::collections::BTreeSet<u64> {
    let mut targets = std::collections::BTreeSet::new();
    for section in sections {
        if !matches!(
            section.name.as_str(),
            ".init_array" | ".fini_array" | ".preinit_array"
        ) {
            continue;
        }
        for entry in section.bytes.chunks_exact(8) {
            let address = u64::from_le_bytes(entry.try_into().expect("eight bytes"));
            // A null entry is a slot the linker left empty, which each of
            // these arrays is allowed to contain.
            if address != 0 {
                targets.insert(address);
            }
        }
    }
    targets
}

/// The address-taken candidates that survive the negative filters.
///
/// Held to a stricter standard than a transfer target, because the evidence
/// is weaker: an instruction that jumps somewhere says control goes there,
/// while one that computes a number says only that the number exists. So a
/// candidate landing on inter-function padding is dropped — a branch target
/// never lands on padding, and an integer that happens to equal a text
/// address routinely does.
fn addressed_placements(
    coverage: &Coverage,
    sections: &[Section],
    addressed: &std::collections::BTreeSet<u64>,
    already: &[Function],
) -> Vec<Function> {
    let claimed: std::collections::BTreeSet<(usize, u64)> = already
        .iter()
        .map(|function| (function.section, function.offset))
        .collect();
    placements(coverage, sections, addressed, Witness::AddressTaken)
        .into_iter()
        .filter(|function| !claimed.contains(&(function.section, function.offset)))
        .collect()
}

/// Whether a candidate function begins on bytes that are filler.
///
/// Applied at *both* doors, which is a deliberate departure from the first
/// version of `docs/code-discovery.md`: it wrote the padding rule as a
/// constraint on the weak witnesses alone, on the argument that a branch
/// target never lands on padding. Real binaries say otherwise, and the two
/// cases that say it are both in glibc:
///
/// - An address-taken operand landing on a ten-byte `cs nopw` — an integer
///   that happens to equal a text address, which is the case the rule was
///   written for.
/// - **An `.eh_frame` FDE that deliberately starts one byte early.** The
///   signal-return trampoline `__restore_rt` is covered by an FDE beginning
///   at `restorer - 1`, so that unwinding a signal frame — whose return
///   address *is* the trampoline's first byte — finds an entry covering
///   `pc - 1`. It is not a mistake in the binary; it is the unwinder's
///   convention, and it makes a strong witness name an address that is not
///   an instruction boundary at all.
///
/// Accepting the second would translate the tail of a `nop` as though it
/// were code — the silent failure this whole design is built to avoid —
/// where rejecting it costs at most a loud miss on an address nothing in a
/// kisal container ever transfers to. Loud beats silent, so the filter
/// applies to every witness.
fn starts_on_padding(sections: &[Section], function: &Function) -> bool {
    is_padding_at(sections, function.section, function.offset)
}

/// The same question about an address that is not yet a function.
fn is_padding_at(sections: &[Section], section: usize, offset: u64) -> bool {
    let Some(section) = sections.get(section) else {
        return false;
    };
    let start = offset as usize;
    if start >= section.bytes.len() {
        return false;
    }
    let end = (start + 16).min(section.bytes.len());
    is_padding(&section.bytes[start..end])
}

/// Whether bytes are what a linker puts *between* functions.
///
/// The shapes are the ones a linker and an assembler emit: zero fill, the
/// `int3` a linker uses to make a fall-through fault, and the single- and
/// multi-byte `nop`s an assembler pads with. The prefixes that lead the wide
/// forms — the operand-size `0x66` and the `cs` override `0x2e`, which GNU
/// as emits together in the ten- and eleven-byte forms — are counted rather
/// than recursed through, so that a real instruction beginning with one is
/// not mistaken for filler.
/// Whether a whole run of bytes is filler, rather than whether one begins
/// with it.
///
/// The alignment a linker inserts between functions: a single multi-byte
/// `nop`, or several, or zero fill. Used where the question is "is there
/// anything real between here and there" — see
/// `SymbolTable::fall_out_target`.
pub fn is_filler(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    let mut decoder = iced_x86::Decoder::new(64, bytes, iced_x86::DecoderOptions::NONE);
    let mut instruction = iced_x86::Instruction::default();
    while decoder.can_decode() {
        decoder.decode_out(&mut instruction);
        if instruction.is_invalid() {
            return false;
        }
        if !matches!(
            instruction.mnemonic(),
            iced_x86::Mnemonic::Nop | iced_x86::Mnemonic::Int3
        ) && !(instruction.mnemonic() == iced_x86::Mnemonic::Add
            && bytes.iter().all(|byte| *byte == 0))
        {
            return false;
        }
    }
    true
}

fn is_padding(bytes: &[u8]) -> bool {
    let Some(first) = bytes.first() else {
        return true;
    };
    if matches!(first, 0x00 | 0xcc | 0x90) {
        return true;
    }
    let prefixes = bytes
        .iter()
        .take_while(|byte| matches!(**byte, 0x66 | 0x2e))
        .count();
    matches!(
        bytes.get(prefixes..),
        Some([0x0f, 0x1f, ..]) | Some([0x90, ..])
    )
}

/// Every address one function's instructions *name*, as distinct from every
/// address they transfer to.
///
/// A callback is registered before it is ever called, and the registration
/// is an instruction too — `lea handler(%rip), %rdi`, or `mov $handler,
/// %edi` where the code is not position-independent. Nothing transfers to
/// the handler directly and no symbol need name it, so without this it is
/// invisible until something calls through the pointer at run time.
///
/// These are exactly the values the discriminating indirect call will later
/// hand to the exec map. Collecting them at bake time is the same question
/// asked where a miss is cheap.
///
/// The two operand shapes are not interchangeable and the difference is
/// easy to get backwards: a program-counter-relative operand is an offset
/// the decoder resolved against a section-relative program counter, so the
/// section's address has to be added back; an immediate is already the
/// address it means.
fn collect_addressed(
    section: &Section,
    instruction: &iced_x86::Instruction,
    into: &mut std::collections::BTreeSet<u64>,
) {
    match instruction.memory_base() {
        iced_x86::Register::RIP => {
            into.insert(
                section
                    .address
                    .wrapping_add(instruction.memory_displacement64()),
            );
        }
        iced_x86::Register::None => {
            into.insert(instruction.memory_displacement64());
        }
        _ => {}
    }
    for index in 0..instruction.op_count() {
        if matches!(
            instruction.op_kind(index),
            iced_x86::OpKind::Immediate32to64
                | iced_x86::OpKind::Immediate64
                | iced_x86::OpKind::Immediate32
        ) {
            into.insert(instruction.immediate(index));
        }
    }
    into.remove(&0);
}

/// Every address one function transfers to directly.
///
/// The decoder runs with a section-relative program counter, so an operand
/// is an offset into the *source's* section — which is why it has to be
/// turned back into an address before it can be looked up: a call into
/// another section arrives as an offset that has wrapped below zero, and
/// `_start` calling `_init` is exactly that shape.
fn collect_transfer_targets(
    sections: &[Section],
    function: &Function,
    into: &mut std::collections::BTreeSet<u64>,
    addressed: &mut std::collections::BTreeSet<u64>,
) {
    let section = &sections[function.section];
    let from = function.offset as usize;
    let to = from + function.size as usize;
    if to > section.bytes.len() {
        return;
    }
    let mut decoder = iced_x86::Decoder::with_ip(
        64,
        &section.bytes[from..to],
        function.offset,
        iced_x86::DecoderOptions::NONE,
    );
    while decoder.can_decode() {
        let instruction = decoder.decode();
        collect_addressed(section, &instruction, addressed);
        if instruction.op0_kind() != iced_x86::OpKind::NearBranch64 {
            continue;
        }
        if !matches!(
            instruction.flow_control(),
            iced_x86::FlowControl::Call
                | iced_x86::FlowControl::ConditionalBranch
                | iced_x86::FlowControl::UnconditionalBranch
        ) {
            continue;
        }
        into.insert(section.address.wrapping_add(instruction.near_branch64()));
    }
}

/// Where a set of candidate addresses would put functions.
///
/// Each is bounded by whichever comes first: the next candidate in the same
/// section, or the next function already known to start there. Bounding
/// against the other candidates is what keeps two starts in one gap from
/// producing a function that swallows its neighbour — the bound has to see
/// the whole batch, because a start discovered a moment later is still a
/// start.
fn placements(
    coverage: &Coverage,
    sections: &[Section],
    targets: &std::collections::BTreeSet<u64>,
    witness: Witness,
) -> Vec<Function> {
    let mut candidates: std::collections::BTreeMap<usize, Vec<(u64, u64)>> =
        std::collections::BTreeMap::new();
    for &address in targets {
        let Some((section, offset)) = section_holding(sections, address) else {
            continue;
        };
        if sections[section].role != SectionRole::Text {
            continue;
        }
        if coverage.covers(section, offset) {
            continue;
        }
        // Dropped here rather than at the door, because a candidate that is
        // filler must not *bound* anything either. A padding candidate
        // discarded after the extents were computed still leaves the
        // function before it ending in the middle of a `nop` — an extent
        // that cuts an instruction in half, which the lifter then refuses to
        // decode. The one-line version: filler is not a candidate, so it
        // never enters the batch the bounds are computed over.
        if is_padding_at(sections, section, offset) {
            continue;
        }
        candidates
            .entry(section)
            .or_default()
            .push((offset, address));
    }

    let mut placed = Vec::new();
    for (section, mut offsets) in candidates {
        offsets.sort_unstable();
        offsets.dedup();
        for (index, (offset, address)) in offsets.iter().enumerate() {
            let next_candidate = offsets.get(index + 1).map(|(offset, _)| *offset);
            let next_known = coverage.next_start_after(section, *offset);
            let end = [next_candidate, next_known]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(sections[section].bytes.len() as u64);
            if end <= *offset {
                continue;
            }
            placed.push(Function {
                name: format!("fn.{address:#x}"),
                symbol: None,
                section,
                offset: *offset,
                size: end - *offset,
                witness,
            });
        }
    }
    placed
}

/// The fourth witness: something transfers to it, and nothing said it was
/// there.
///
/// Symbols, unwind entries and linkage tables between them describe almost
/// every function in a linked program, and "almost" is the problem. The crt
/// fragments in `.init` and `.fini` carry no `.size`, no unwind entry and —
/// in a stripped binary — no symbol either, and `_start` calls straight into
/// them. A stripped busybox stops on its first call for exactly this reason.
///
/// Weak, so it goes through [`Coverage::fill`] and reaches only what nothing
/// else claimed. Direct evidence about a particular address — *this*
/// instruction transfers *here* — which is the line the weak witnesses do
/// not cross: scanning for prologues, or for the `endbr64` that marks some
/// indirect-branch target, finds more functions and also invents them.
///
/// A function found this way transfers somewhere itself, so this repeats
/// until nothing new appears — over what the previous round added, not over
/// everything. Re-decoding every function every round is the difference
/// between a second and a minute on a two-megabyte program.
fn fill_from_transfers(coverage: &mut Coverage, sections: &[Section]) -> Result<()> {
    const ROUNDS: usize = 16;
    let mut targets = std::collections::BTreeSet::new();
    let mut addressed = std::collections::BTreeSet::new();
    // Candidates a door refused for a reason that will not change. The
    // loop's termination test is "nothing new was placed", so a candidate
    // that is offered and refused every round never terminates — and
    // whether that can happen depends on `placements` and `fill` agreeing
    // about what is acceptable. This makes termination independent of that
    // agreement, which is worth three lines: the failure it prevents is a
    // hang, and the coupling it removes is the kind that breaks silently
    // when a door learns a new refusal.
    let mut declined: std::collections::BTreeSet<(usize, u64)> = std::collections::BTreeSet::new();
    let mut examined = 0;
    for round in 0..=ROUNDS {
        let known = coverage.functions().len();
        for index in examined..known {
            collect_transfer_targets(
                sections,
                &coverage.functions()[index],
                &mut targets,
                &mut addressed,
            );
        }
        examined = known;

        // Transfer targets first, and separately, because the two are not
        // equally good evidence even though both are weak. An instruction
        // that jumps somewhere is saying that control goes there; one that
        // computes an address is saying only that the number exists. Where
        // both point at the same place the stronger reading is recorded.
        let mut placed = placements(coverage, sections, &targets, Witness::Transfer);
        placed.extend(addressed_placements(
            coverage, sections, &addressed, &placed,
        ));
        placed.retain(|function| !declined.contains(&(function.section, function.offset)));
        if placed.is_empty() {
            return Ok(());
        }
        if round == ROUNDS {
            bail!(
                "discovering functions from what calls them did not settle in \
                 {ROUNDS} rounds"
            );
        }
        for function in placed {
            let where_it_was = (function.section, function.offset);
            // Refusals are the invariant doing its job, not an error: a
            // candidate that a later batch covered is one the strong
            // witnesses already accounted for. `AlreadyCovered` needs no
            // note, because coverage itself now answers for it; the others
            // are permanent and are remembered so they are not re-offered.
            match coverage.fill(sections, function) {
                Ok(()) | Err(Refused::AlreadyCovered) => {}
                Err(Refused::NotCode | Refused::Empty | Refused::Padding) => {
                    declined.insert(where_it_was);
                }
            }
        }
    }
    Ok(())
}

fn split_at_interior_entries(sections: &[Section], functions: &mut Vec<Function>) -> Result<()> {
    // To a fixpoint, because a cut makes boundaries that were not there
    // before: a branch that stayed inside one function may cross two of its
    // pieces, and that is a second entry the first pass could not see.
    // glibc's `memmove` variants need three rounds — the entry cut exposes a
    // branch into the tail, which exposes another.
    const ROUNDS: usize = 16;
    for round in 0..=ROUNDS {
        if !split_once(sections, functions)? {
            return Ok(());
        }
        if round == ROUNDS {
            bail!(
                "splitting functions at their entry points did not settle in \
                 {ROUNDS} rounds, which means a cut is creating another"
            );
        }
    }
    Ok(())
}

/// One pass of [`split_at_interior_entries`], reporting whether it cut
/// anything.
fn split_once(sections: &[Section], functions: &mut Vec<Function>) -> Result<bool> {
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
                        // A direct call names an entry as plainly as a jump
                        // does. It matters where a weak witness had to guess
                        // an extent: in a region with no unwind coverage a
                        // filled function runs to the next thing known to
                        // start, which can be tens of kilobytes and dozens
                        // of real functions away, and every call into one of
                        // them then lands "in the middle of a function".
                        // Compilers do not call into the interior of a
                        // function they emitted, so a call that appears to
                        // is evidence that the extent, not the call, is
                        // wrong.
                        | iced_x86::FlowControl::Call
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

    // Each function's neighbours, sorted by where they start, with a
    // running maximum of how far anything up to that point reaches.
    //
    // This is what keeps the pass from being quadratic. Asking "which
    // function contains this target" by scanning every function, for every
    // target, is fine on a corpus fixture and fatal on a real program:
    // CPython has enough of both that a single round did not finish in two
    // and a half minutes. The prefix maximum is what makes the search
    // exact rather than merely fast — a target is contained only by
    // functions starting at or before it, and the walk backwards can stop
    // as soon as nothing earlier reaches far enough.
    let mut by_section: HashMap<usize, Vec<(u64, u64, usize)>> = HashMap::new();
    for (index, function) in functions.iter().enumerate() {
        by_section.entry(function.section).or_default().push((
            function.offset,
            function.offset + function.size,
            index,
        ));
    }
    for placed in by_section.values_mut() {
        placed.sort_unstable();
        let mut reach = 0u64;
        for entry in placed.iter_mut() {
            reach = reach.max(entry.1);
            entry.1 = reach;
        }
    }

    // A target that lands inside a function but not at its start is an entry
    // the symbol table did not describe.
    let mut cuts: HashMap<usize, BTreeSet<u64>> = HashMap::new();
    for (section, section_targets) in &targets {
        let Some(placed) = by_section.get(section) else {
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
            let mut at = placed.partition_point(|(offset, _, _)| *offset <= *target);
            while at > 0 {
                let (_, reach, index) = placed[at - 1];
                if reach <= *target {
                    break;
                }
                at -= 1;
                let function = &functions[index];
                let interior =
                    (function.offset + 1..function.offset + function.size).contains(target);
                if interior && starts.get(&index).is_some_and(|b| b.contains(target)) {
                    cuts.entry(index).or_default().insert(*target);
                }
            }
        }
    }
    if cuts.is_empty() {
        return Ok(false);
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
                // Named after where the piece begins within whatever it was
                // cut from. A later round cuts a piece that already carries
                // a suffix, so the offsets compose rather than nesting.
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
                // The first piece is the function that was cut and keeps
                // whatever found it; the rest exist because something
                // branched into them.
                witness: if start == function.offset {
                    function.witness
                } else {
                    Witness::InteriorEntry
                },
            });
            start = point;
        }
    }
    split.sort_by_key(|function| (function.section, function.offset));
    *functions = split;
    Ok(true)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A text section filled with `ret`.
    ///
    /// Not `nop`, which is what this used to be: `nop` *is* padding, and
    /// padding is now refused at both doors, so a section of it is a
    /// section in which no candidate can be placed. A fixture whose bytes
    /// are filler cannot test where functions go.
    fn text(bytes: usize) -> Section {
        Section {
            name: ".text".to_string(),
            role: SectionRole::Text,
            address: 0x400000,
            size: bytes as u64,
            bytes: vec![0xc3; bytes],
            alignment: 16,
            relocations: Vec::new(),
        }
    }

    fn function(offset: u64, size: u64, witness: Witness) -> Function {
        Function {
            name: format!("fn.{offset:#x}"),
            symbol: None,
            section: 0,
            offset,
            size,
            witness,
        }
    }

    /// The invariant, as the type enforces it: weak evidence cannot reach
    /// bytes strong evidence has claimed.
    ///
    /// This is the whole reason [`Coverage`] exists rather than a `Vec`. A
    /// false function start inside a real function bounds that function
    /// short and nothing says so until wrong bytes execute, so the rule
    /// lives in the API where a caller cannot forget it.
    #[test]
    fn fill_is_refused_where_strong_evidence_has_spoken() {
        let sections = [text(0x400)];
        let mut coverage = Coverage::default();
        coverage.establish(&sections, function(0x100, 0x40, Witness::Symbol));

        // The start itself, and a byte in the middle: both are covered.
        assert_eq!(
            coverage.fill(&sections, function(0x100, 0x10, Witness::Transfer)),
            Err(Refused::AlreadyCovered)
        );
        assert_eq!(
            coverage.fill(&sections, function(0x120, 0x10, Witness::Transfer)),
            Err(Refused::AlreadyCovered)
        );
        // One byte past the end is not.
        assert_eq!(
            coverage.fill(&sections, function(0x140, 0x10, Witness::Transfer)),
            Ok(())
        );
        assert_eq!(coverage.functions().len(), 2);
    }

    /// Filler is refused at both doors, not only the weak one.
    ///
    /// The case that forced it is glibc's signal-return trampoline, whose
    /// `.eh_frame` entry deliberately begins one byte before the function
    /// so that unwinding a signal frame finds it. A strong witness naming
    /// something that is not an instruction boundary is not hypothetical.
    #[test]
    fn padding_is_not_a_function_whatever_named_it() {
        let mut sections = [text(0x400)];
        // A ten-byte `cs nopw`, which is what an assembler pads with and
        // what an earlier version of the filter did not recognise.
        sections[0].bytes[0x100..0x10a]
            .copy_from_slice(&[0x66, 0x2e, 0x0f, 0x1f, 0x84, 0, 0, 0, 0, 0]);
        let mut coverage = Coverage::default();
        assert!(!coverage.establish(&sections, function(0x100, 0x10, Witness::UnwindEntry)));
        assert_eq!(
            coverage.fill(&sections, function(0x100, 0x10, Witness::Transfer)),
            Err(Refused::Padding)
        );
        assert!(coverage.functions().is_empty());
    }

    /// And filler may not *bound* anything either, which is the same rule
    /// one step earlier: a padding candidate discarded after the extents
    /// were computed still leaves the function before it ending in the
    /// middle of a `nop`.
    #[test]
    fn a_padding_candidate_never_bounds_its_neighbour() {
        let mut sections = [text(0x400)];
        sections[0].bytes[0x180..0x18a]
            .copy_from_slice(&[0x66, 0x2e, 0x0f, 0x1f, 0x84, 0, 0, 0, 0, 0]);
        let coverage = Coverage::default();
        let targets = std::collections::BTreeSet::from([0x400100, 0x400180]);
        let placed = placements(&coverage, &sections, &targets, Witness::Transfer);
        assert_eq!(placed.len(), 1, "the filler was placed as a function");
        assert_eq!(
            placed[0].size, 0x300,
            "the filler bounded the function before it"
        );
    }

    /// A weak witness may not bound a strong one either, which is the same
    /// rule seen from the other side: the extent a weak witness would take
    /// is cut short by whatever starts after it.
    #[test]
    fn a_filled_function_stops_where_the_next_known_start_begins() {
        let sections = [text(0x400)];
        let mut coverage = Coverage::default();
        coverage.establish(&sections, function(0x200, 0x40, Witness::InitialiserArray));

        let targets = std::collections::BTreeSet::from([0x400100]);
        let placed = placements(&coverage, &sections, &targets, Witness::Transfer);
        assert_eq!(placed.len(), 1);
        assert_eq!(
            placed[0].size, 0x100,
            "the transfer target ran over the constructor after it"
        );
    }

    /// Two candidates in one gap bound each other. Without this the first
    /// swallows the second, and the second is then refused as covered —
    /// which is how a real busybox function came to span its neighbour.
    #[test]
    fn candidates_in_one_gap_bound_each_other() {
        let sections = [text(0x400)];
        let coverage = Coverage::default();
        let targets = std::collections::BTreeSet::from([0x400100, 0x400180]);
        let placed = placements(&coverage, &sections, &targets, Witness::Transfer);
        assert_eq!(placed.len(), 2);
        assert_eq!(placed[0].size, 0x80);
        assert_eq!(placed[1].size, 0x280);
    }

    /// What is left over, which is what the saturated tier is built on.
    #[test]
    fn residue_is_every_range_no_function_covers() {
        let sections = [text(0x400)];
        let mut coverage = Coverage::default();
        coverage.establish(&sections, function(0x100, 0x40, Witness::Symbol));
        coverage.establish(&sections, function(0x200, 0x100, Witness::Symbol));
        assert_eq!(
            coverage.residue(&sections),
            vec![(0, 0..0x100), (0, 0x140..0x200), (0, 0x300..0x400)]
        );
    }

    /// An extent-stating witness is redundant where the symbols already
    /// split what it describes as one function, even though its own first
    /// byte may be uncovered.
    #[test]
    fn overlaps_sees_a_range_the_start_alone_would_miss() {
        let sections = [text(0x400)];
        let mut coverage = Coverage::default();
        coverage.establish(&sections, function(0x100, 0x40, Witness::Symbol));
        assert!(!coverage.covers(0, 0xc0));
        assert!(coverage.overlaps(0, 0xc0..0x110));
        assert!(!coverage.overlaps(0, 0xc0..0x100));
    }
}
