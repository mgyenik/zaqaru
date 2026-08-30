# Code discovery: the witness model

Status: **active** — the design authority for how the linked-ELF front end
decides where functions are. What is built today is marked built; the
milestones D1–D6 at the end are the remaining work, in order. Where this
document and `src/reader.rs` disagree, this document wins and the code gets
corrected.

The shape of the answer, in two sentences. Witnesses — evidence a
function starts at an address, stratified by what each kind is allowed
to do — make discovered code fast; they are D1–D5. The saturated tier —
slow, boring, enterable at any instruction start the witnesses missed —
makes an arbitrary binary *run*; it is D6, and it is what turns a
discovery false negative from a dead container into a performance bug.

Written after a survey of the field (sources at the end), which settled the
question the worklog left open: **there is no sound algorithm for this
problem on a stripped non-PIE static binary, and nobody claims one.** The
only soundness results that exist (RetroWrite, SURI) buy soundness by
restricting the input class to PIE — which this project cannot do, because
the target is any binary. What the field does have is two well-validated
formalizations of exactly the shape this project grew independently —
evidence sources combined under rules — and a measured account of which
evidence is trustworthy. This document adopts what survives contact with
our failure model and names what does not.

## A base is part of the evidence

A position-independent file — which nearly everything shipped is — states
its addresses relative to zero and is read at the base a bake assigns it
(`ObjectFile::parse_at`). That base is not bookkeeping: **read at zero, a
shared object's text sits where its own integer constants sit**, a few
kilobytes up, so `mov $0x1770,%eax` reads as an instruction taking the
address of code and the operand harvest cannot tell the two apart.

Measured on `ld-linux-x86-64.so.2`: eleven address-taken functions at base
zero against three at a real base, and the eight extra ones shredded a
region no strong witness covered into pieces beginning partway through real
instructions. The floor is `baker::layout::DYNAMIC_BASE`, and it lives there
rather than in the reader because for a position-independent file the base
is *ours* — a bad one is a bake that chose badly, not an input that arrived
badly.

The same argument from the other side, where there is no choice to make: a
*fixed* executable linked low is refused
(`baker::layout::MINIMUM_FIXED_ADDRESS`), since its addresses are its own
and refusing is the only available answer. In practice this is close to
vacuous — `mmap_min_addr` forbids under 64 KiB and both GNU ld and lld emit
`0x400000` — which is why the floor for refusing someone else's choice sits
well below the base we pick for our own.

## The problem, and the asymmetry that shapes everything

A linked executable must be split into functions before translation:
translation is per-function, the exec map is keyed by function entry, and
an extent decides where decoding stops. Nothing in a stripped binary is
*required* to say where functions are.

The two failure directions are not symmetric, and every rule below follows
from that:

- **A missed function fails loudly.** Every indirect transfer goes through
  the exec map, and an address the map does not hold is a named runtime
  error (`src/transpile.rs:177` is the translation-time face of the same
  policy). The failure carries the address, and the fix is a re-bake.
- **A false function start fails silently.** A start invented inside a
  real function bounds that function short — its tail is cut off, or it is
  split at a point that is not an instruction boundary — and nothing says
  so until wrong bytes execute. This is the one failure mode the project
  has no detector for.

So the bar for evidence that may *bound* a function is absolute, and the
bar for evidence that may merely *add* one is much lower — because an
added function that is wrong is dead code nothing ever calls, a size cost
and not a correctness cost. That distinction is the load-bearing rule of
this design, stated next.

## The invariant: strong witnesses bound, weak witnesses only fill

Every witness is one of two kinds, and the kind decides what it is
permitted to do:

**Strong witnesses may define boundaries** — start a function, state or
imply its extent, and bound the function before it. Strong evidence is
what the ELF format or the psABI *defines* to mean "a function starts
here":

| witness | what it says | status |
|---|---|---|
| `.symtab` `STT_FUNC` value + size | start and extent | built (`collect_functions`) |
| `.dynsym` `STT_FUNC` value + size | the same, and the *only* symbol table a stripped shared object has, because linking against one requires it | built (`read_symbols`) |
| `.eh_frame` FDE | start and extent; sizes a sizeless symbol; names functions no symbol does | built (`unwind_extents`, `src/eh_frame.rs`) |
| the next known start | an upper bound for anything sizeless — a function cannot run past whatever begins after it | built (`symbol_boundaries`, `next_boundary`) |
| `.init_array` / `.preinit_array` / `.fini_array` entries | the runtime is told to call here | built (in `discover_from_transfers`) |
| linkage-table sections (`.plt`, `.iplt`, `.plt.sec`, `.plt.got`) at the section's stated stride | every slot is a stub | built |
| `e_entry` | the kernel transfers here | **D3** |
| `R_X86_64_IRELATIVE` addends | the startup code calls this resolver | **D3** |
| `R_X86_64_RELATIVE` targets landing in text (PIE / static-PIE inputs) | an embedded code pointer, marked exactly | **D3** |

### A stated extent and a guessed one are not the same thing

The first version of this document made the permission to cut turn on
*witness strength*, and that is the wrong axis. The two come apart: a symbol
is a strong witness, but a symbol with no size states only a **start**, and
its extent is then whatever begins next — a bound, not a fact. The same is
true of every candidate `placements` bounds.

So a function carries, beside its witness, where its **extent** came from:

- **Stated** — the file said so. A symbol's size, an unwind entry's length,
  a linkage table's stride, the length of `.init`.
- **Guessed** — worked out here, from whatever begins next.

The invariant's argument is that a false start silently truncates a real
function. That argument is about *stated* extents. An extent this pass
guessed is already a guess, and refusing to revise it means the first guess
wins permanently over better evidence.

Which is not hypothetical: busybox's `applet_main[]` names 278 functions
inside a span that a transfer target had been bounded across a 631 KiB
unwind hole, and `busybox cat` reached one of them and died on an exec-map
miss. So **weak evidence landing inside a guessed extent cuts it**, and
inside a stated one is skipped exactly as before — which is what keeps a
computed goto's 256 labels from shredding `_PyEval_EvalFrameDefault`, whose
size its symbol states.

**Every** weak witness, not merely the data-array one. The first version of
this rule was written for the applet table and applied only to it, and
`busybox ls -l` then failed on an address a `lea` had taken and D4 had duly
harvested — dropped, because something already covered it. An address an
instruction takes is exactly as much evidence that a guess ran too long as a
table entry is; the argument is about the standing of the *extent*, and says
nothing about which kind of weak evidence disputes it.

So there are three tiers rather than two:

| evidence | may cut a stated extent | may cut a guessed one |
|---|---|---|
| a direct branch or call | yes | yes |
| an address a table or an instruction names | no | yes |
| nothing else | no | no |

**Weak witnesses may only introduce a function into a region no strong
witness covers.** A weak witness must never split, shorten, or bound a
function that strong evidence established. Weak evidence is an
*observation* that code appears to be at an address:

| witness | what it says | status |
|---|---|---|
| direct transfer targets — a discovered function calls or jumps here | this instruction transfers *here* | built (`discover_from_transfers`, gap-only by construction: `uncovered_functions` skips covered code) |
| address-taken code in operands — a RIP-relative `lea` or a `mov` immediate whose value lands in text | this instruction takes the address of *here* | **D4** |
| data address arrays — a corroborated run of code pointers in a data section | something indexes a table of these | built (`data_array_targets`) |

The fourth witness already obeys the gap-only rule — `uncovered_functions`
refuses anything inside covered code, and a transfer *into* a covered
function is `split_at_interior_entries`' question, which acts only on
direct branch evidence. What D2 adds is the rule stated as an invariant in
the code, with witness provenance recorded per function, so the rule
survives the next witness instead of being rediscovered by its first
violation.

The reason the stratification is the whole design and not a refinement:
under it, the worst a weak witness can do is add a spurious function to a
gap. Nothing targets it, so nothing calls it; it costs bytes. Without it,
the same false positive truncates a neighbour silently. The survey's
clearest single finding is a system that draws this exact line — Ddisasm
promotes a data value to a *function entry* only when it sits in an
ABI-defined function-pointer section, and treats a code pointer in generic
data as evidence of code, never of a function boundary — and a measured
account of what happens without it (Pang et al. found thousands of data
pointers into function *middles*; Ghidra's contrary rule is demonstrably
unsound).

## The concrete trap this defends against, named so it stays defended

CPython is compiled with computed gotos. `opcode_targets[]` is ~256
pointer-sized, aligned, consecutive `&&label` addresses in `.rodata`, and
every one lands **in the middle of `_PyEval_EvalFrameDefault`** — the
single hottest function in the milestone target. That table passes every
plausible positive test for "array of function pointers." A discovery pass
that let it define boundaries would shred the eval loop into ~256 pieces,
silently. Under the invariant it is a non-event: the eval loop is covered
by a strong witness, so the run's targets land in covered code and the
weak witness is not consulted.

The residual case is the same shape in an *uncovered* region — a computed
goto or a `.rodata` jump table whose owning function is itself
undiscovered. D5's cluster rule exists for it: a run whose targets all
land inside one small span is refused and logged, because
function-interior labels bunch and genuine function tables scatter. The
refusal is deliberate — evidence-inconclusive is an answer this design is
allowed to give, and a loud runtime miss is recoverable where a silent
split is not.

## What each unbuilt witness is, precisely

### D3 — the free strong witnesses

**Dynamic relocations, read for discovery only.** `read` currently skips
relocations entirely for a linked input (`src/reader.rs:297`), on the
correct argument that they say nothing about *translation* — the code is
placed and an operand is the address it means. They say a great deal about
*discovery*:

- Every `R_X86_64_IRELATIVE` entry's addend is an ifunc **resolver** —
  a real function `__libc_start_main` (or `_start` walking
  `__rela_iplt_start` in a static glibc) calls through a pointer read
  from relocation data. In a stripped static glibc the resolvers have no
  symbol, no unwind entry, and no instruction anywhere naming their
  address: precisely the shape the built witnesses cannot see. This is
  ABI-grade evidence, the same class as the initialiser arrays.
- Every `R_X86_64_RELATIVE` target landing in a text section is an
  embedded code pointer, marked exactly. A non-PIE static executable has
  none; a **static-PIE** one has an entry for every pointer the binary
  embeds, which turns D5's whole problem into a table walk. The
  build-control corollary is worth more than any heuristic in this
  document: for an image we build ourselves, `-static-pie` converts the
  hard case into the solved case, and the bake should say when its input
  qualifies.

The parsing rule that history already taught (worklog: "dynamic
relocations stopping a parse"): an unmodelled dynamic relocation type is
*skipped* on the discovery read, never fatal — the read is harvesting
evidence, not interpreting the file, and refusing what it does not model
would reintroduce the defect that made a stripped busybox unopenable.

**`e_entry`.** One address the kernel is defined to transfer to. Today
`_start` is found through its symbol or its FDE, which a stripped binary
with an unwind hole is not obliged to provide. One line of insurance;
FunProbe and Ddisasm both treat it as definite.

### D4 — address-taken code from operands

`collect_transfer_targets` harvests only `NearBranch64` flow-control
targets. A callback is registered before it is ever called, and the
registration is an instruction too: `lea func(%rip), %rdi`, or in non-PIE
code `mov $func, %edi`. Harvest, per decoded instruction of every
discovered function: RIP-relative memory operands' effective addresses and
immediate operands' values, kept when they land inside a text section.
Gap-only like every weak witness, and subject to D5's padding rejection
(an immediate is just a number; `0x401000` landing on inter-function
padding is an integer, not evidence).

This closes discovery's loop with the machinery that already exists at run
time: these are exactly the values the discriminating indirect call will
later present to the exec map, collected at bake time where a miss is
cheap instead of at run time where it stops the program. It is also
higher-precision than scanning data, because the evidence is an
instruction that manipulates the address, not a bit pattern that resembles
one.

### D5 — data address arrays, corroborated

The witness the stripped busybox needs: its applet table is function
pointers in data, in a 640 KB text region with no unwind coverage (but see
D1 — the hole is diagnosed before this is built). A candidate run must
pass **all** of:

1. The source is pointer-size aligned and lies wholly inside a loaded,
   initialized, non-text section.
2. It is part of a run of **at least three** entries at a constant stride
   whose values all land in text sections.

   *Built as stride eight only, and the reason is a measurement.* This rule
   was written expecting arrays of structs, and the pitfall index below said
   a stride-eight scan would miss the applet table. On this machine's
   busybox it does not: the table is 278 bare pointers at stride eight in
   `.data.rel.ro`. The struct-stride generalisation waits for a binary that
   needs it, where it will arrive with a case instead of a guess about one.
3. Every target lands on an instruction boundary under the fixpoint
   sweep's decoding of the surrounding gap, and the bytes at the target
   are not padding (below).
4. Every target lies in a region no *stated* extent covers — see "A stated
   extent and a guessed one are not the same thing". A target inside a
   **guessed** extent cuts it, because that is evidence the bound was
   wrong; a target inside a stated one is skipped, which is the invariant
   doing its job.
5. The run is refused, with a log line naming it, if its targets all
   cluster inside one span smaller than the run could plausibly describe
   as separate functions — the computed-goto/jump-table tell — or if the
   source bytes read as a plausible ASCII string, or if a value's low
   bits are all zero with nothing at the target (the bitmask tell).

`endbr64` at the target is corroborating evidence and never sufficient —
the standing judgment, now with the survey's backing: the only tools that
treat it as sufficient do so by *assuming* a CET-enabled binary, which
promotes it from a hint to an invariant this project's inputs do not
carry.

### Negative witnesses, used by D4 and D5

The concept the witness list lacked entirely, and the reason the surveyed
systems afford aggressive positive rules at their measured precision. The
ones adopted:

- **Padding is never a function**, whatever named it. A candidate whose
  first bytes are `0x00`, `0xcc`, or a multi-byte NOP run — including the
  `cs`-prefixed wide forms, `66 2e 0f 1f 84 …`, which GNU as emits — is
  rejected.

  This was first written as a constraint on the weak witnesses alone, on
  the argument that a branch target never lands on padding. **Real input
  says otherwise, and the filter now applies at both doors.** The case that
  forced it: glibc's signal-return trampoline carries an `.eh_frame` FDE
  beginning one byte *before* `__restore_rt`, so that unwinding a signal
  frame — whose return address is the trampoline's first byte — finds an
  entry covering `pc - 1`. That is the unwinder's convention rather than a
  mistake in the binary, and it makes a strong witness name an address that
  is not an instruction boundary at all. Accepting it translates the tail of
  a `nop` as code, which is the silent failure this design exists to avoid;
  refusing it costs at most a loud miss on an address nothing transfers to.

  The rejection also happens in `placements`, before extents are computed,
  because filler must not *bound* a neighbour either: a padding candidate
  discarded afterwards still leaves the function before it ending inside a
  `nop`, which the lifter then refuses to decode — a failure that presents
  three functions away from its cause.
- **A jump table's arms are not functions.** Any run or target already
  consumed by jump-table recovery (`src/jump_table.rs`) is excluded;
  the cluster rule above covers the case where the owning function is
  itself undiscovered.
- **A string is not a pointer array.** Overlap with a plausible string
  rejects the source.

What is deliberately *not* adopted: weighted or Bayesian scoring over the
witnesses (Ddisasm's auction, FunProbe's belief propagation). Scoring
replaces the invariant's hard line with a tunable threshold, and a
mis-tuned threshold reopens the truncation risk the invariant closes. The
stratification captures what the weights are for at zero tuning surface.
The witness provenance D2 records is the substrate scoring would need, so
the option stays open at no cost; the trigger for taking it is a real
binary the hard rules cannot decide, not a preference for sophistication.

## The floor: the saturated tier (D6)

Everything above makes discovery *better*; none of it makes discovery
*complete*, because complete is not on offer — an indirect target is a
value the program computes, and no sound static rule reaches all of them.
For an arbitrary OCI image that must simply work, the answer is not a
sharper witness. It is a floor: a tier of deliberately slow, boring code
that can be **entered at any instruction start the witnesses missed**, so
that a miss stops being fatal.

What the floor changes is the meaning of a discovery false negative.
Today it is a correctness failure — `kisal_no_function_at` and the
container dies. With the floor, the ladder is:

1. strong witnesses → structured, promoted, fast code;
2. weak witnesses → the same, for what they add;
3. the saturated tier → dispatcher-shaped, unpromoted, slow — and
   *present*, for every instruction start the witnesses did not claim;
4. a loud miss → only for a jump to a mid-instruction byte offset,
   which is an overlapping instruction stream, which gcc and clang do
   not emit (Andriesse et al. measured exactly this).

A missed function becomes a performance bug instead of a dead container.
The witnesses become the optimization tier; the guarantee comes from
below. Profile-guided re-baking then makes slow code fast later — but
nothing *depends* on exercising a path at build time to make it run.

### Why this is assembly of existing parts, not new machinery

The property the tier needs — AOT-emitted code enterable at an address
nobody predicted — is the property the resume machinery already has,
because blocking and checkpointing required it. The parts, each already
built and tested:

- **The dispatcher body** (`emit_dispatcher`, `src/structurer.rs:263`):
  a `loop` over a `br_table` whose entry arm is chosen by a parameter.
  Resume bodies are exactly this, entered "wherever the parameter says
  instead of at the top" (`translate_resume_function`,
  `src/structurer.rs:249`). The tier reuses it unchanged.
- **The resume-body type**, `(entry: i32) -> (id: i64)`
  (`src/transpile.rs:472`), and the resume-ID encoding — body's table
  slot in the low 32 bits, entry index in the high 32
  (`src/translate.rs:238`). Call sites inside the tier store ordinary
  resume IDs, so blocking inside tier code composes with the scheduler
  with nothing added.
- **The exec map**, untouched. `x86_slot_of`
  (`build_exec_map_lookup`, `src/transpile.rs:2297`) binary-searches
  `x86_exec_map` — 16-byte `(vaddr, table-slot)` entries built from
  `table_slots_by_location` (`src/transpile.rs:515`) — and every
  indirect transfer site already calls it
  (`emit_transfer`/`emit_tail_transfer`, `src/translate.rs:635/588`).
  The tier only *adds entries*; the builder, the lookup and every call
  site are byte-identical.
- **The unpromoted configuration.** Promotion's off-switch is the empty
  promotion map, not a second code path (design.md) — tier bodies use
  it per-function, which is a supported configuration and not a new
  one.
- **The fixpoint sweep** (`src/lifter.rs:167`): decodes a byte range,
  collects branch targets, re-sweeps, poisons undecodable offsets. The
  tier translates the sweep's canonical instruction stream — which for
  gcc/clang output is the true decode, since those compilers embed no
  data and no overlapping streams in text.

### The design

**Regions.** After D5, the coverage type's `residue()` reports every
maximal text range no function covers. Each range is a *region*; each
region is decoded by the fixpoint sweep and cut into **pieces** of at
most a fixed instruction count (start at 4096; the S-gate below
measures), because one 640 KB region is ~150k instructions and a single
`br_table` over 150k arms is a bet no engine has been asked to honour.

**Piece bodies.** A piece translates as a dispatcher over a *saturated*
graph — `ControlFlowGraph::build_saturated` (new): every instruction is
its own block, so every instruction start is a `br_table` arm. Two
bodies per piece, exactly as every function already gets two under
`--resume` (`src/transpile.rs:544–594`):

- the ordinary body, type `(entry: i32) -> ()` — `emit_dispatcher` with
  the parameter as the state local and `yield_on_return` off;
- the resume body, type `(i32) -> i64`, the standing resume shape.

Tier bodies are always dispatcher-mode regardless of `Mode::Structured`;
there is nothing structured about a graph with an edge into every node.

**Trampolines — the PLT of the design.** The exec map's contract is
"address → table slot of a `() -> ()` guest-convention function," and
every `call_indirect` site names `guest_type`; a piece body's type is
not that and must never reach a call site. So each instruction start
gets a **trampoline**: a `() -> ()` function whose whole body is
`i32.const k; call piece_n; end`. The trampoline takes the table slot
and the exec-map entry — registered through `table_slots_by_location`
keyed by the instruction's `(section, offset)`, which is the *only*
integration point: the map builder picks it up with zero changes. An
indirect transfer to a residue address then works today's path
end-to-end: read operand → flush → `x86_slot_of` finds the trampoline's
slot → `call_indirect` → trampoline → piece body enters at block `k`.
Lazy binding with all translations pre-existing in the artifact: PLT
semantics without runtime code generation.

**Edges, by kind:**

- *Within a piece*: `br_table` transfer, the dispatcher's ordinary move.
- *Across pieces, static target*: pieces share one type, so the
  ordinary body emits `i32.const k; return_call piece_m`. The resume
  body uses a plain call instead — the same rule ordinary resume bodies
  already follow, because `return_call` requires the yield types to
  agree (`src/translate.rs:556`).
- *Out of the region, direct*: not the tier's problem by construction.
  The region sweep runs during **discovery**, so its direct call and
  jump targets feed the fourth witness and
  `split_at_interior_entries` like anyone else's — by translation
  time, every direct out-edge lands on a function entry and translates
  as an ordinary transfer.
- *Out of the region, indirect*: the standing exec-map path, shared
  with all translated code.
- *`ret`, `syscall`, `%rsp`*: the ordinary translations, unmodified. An
  indirect call into the tier reserved its return slot at the site;
  the tier's `ret` pops it and returns through the trampoline. Nothing
  is special-cased, which is the point.

**What gets no entry**: poisoned offsets (padding, undecodable bytes)
and mid-instruction offsets. A jump there still dies at
`kisal_no_function_at` — honestly, because the canonical decode says
there is no instruction there, and inventing one would be the silent
answer this project does not give.

### Costs, stated up front

Per residue instruction (~150k for a 640 KB hole): one trampoline
(~10 bytes of code section plus a function entry), one table element,
one 16-byte exec-map entry (~2.5 MB for the busybox hole), and one
dispatcher arm. Per instruction *executed* in the tier: a `br_table`
round trip and unpromoted state traffic — several times slower than
structured code, on code that is disproportionately cold (no symbol, no
FDE, reached only through pointers describes crt glue and dispatch
stubs, not inner loops). The counts are the risk: ~150k extra
functions, table elements and map entries is scale nothing in this
toolchain has been asked for, which is why D6 opens with a gate, not
with code.

### Boundaries, named so the guarantee is honest

- **Mid-instruction targets.** The tier covers every instruction start
  of the canonical decode. Deliberately overlapping instruction
  streams — obfuscated or adversarial binaries — still miss loudly.
  Per-byte saturation (Multiverse's move) would cover even that and
  stays rejected below; the residual is pathology, not compiler
  output.
- **Runtime code generation.** An image that JITs — Node, a JVM — fails
  at `mmap(PROT_EXEC)` of bytes that were never baked, and no
  discovery tier touches that. The honest envelope is *any image that
  does not generate code at runtime*.
- **Interior addresses of covered functions.** An indirect jump into
  the middle of a *discovered* function (a computed goto whose table
  the jump-table recovery failed to recognise) is not served by this
  tier — the address is covered, so no trampoline exists, and the
  exec map misses. Jump-table recovery owns that case. The
  generalization — resolving interior addresses into the function's
  own resume body, whose `br_table` already covers every block — is
  the same mechanism at a different granularity (resume bodies split
  at calls, not at instructions) and is named future work, not D6.

## What stays rejected, now with measurements attached

- **Prologue scanning.** Adds functions at 77.5% precision (Pang et al.
  measured the heuristic across tools), and its false-positive mode is
  truncation — Andriesse et al. traced Dyninst matching `push %r15` as a
  prologue and missing the instructions before it, which is this
  project's nightmare case verbatim.
- **`endbr64` as sufficient evidence.** It marks every indirect-branch
  target — jump-table arms and landing pads included.
- **Per-byte superset translation** — translating every *byte offset*
  of a region so even overlapping instruction streams resolve. Sound by
  construction and measured at ~9.5× binary size in Multiverse; in wasm
  it is worse still. The saturated tier (D6) is deliberately not this:
  it saturates the *canonical decode's instruction starts*, which is
  linear in the instruction count and covers everything gcc and clang
  emit, and it leaves the per-byte case — hand-crafted overlapping
  streams — as the named loud miss.
- **Learned detectors** (ByteWeight, XDA, DeepDi). Scores without
  evidentiary meaning, fragile under distribution shift, unauditable —
  the wrong tool for a design whose every acceptance is "name the
  evidence."

## The pipeline, restated with the new pieces in place

For a linked input (`Layout::Linked`), in order:

1. `collect_functions` — symbols, unwind extents, next-start bounds,
   linkage tables. Strong.
2. **D3**: `e_entry`; the dynamic-relocation harvest (IRELATIVE addends
   as strong starts; RELATIVE targets as strong starts where they land
   in text). Strong.
3. `discover_from_transfers` — initialiser arrays (strong), then the
   transfer-target fixpoint (weak, gap-only). **D4** adds the operand
   harvest to the same fixpoint: address-taken values are collected in
   the same decode pass that collects branch targets.
4. **D5**: the data-array scan over what remains uncovered, corroborated
   and negatively filtered as above. Runs after the fixpoint settles,
   because every function it adds feeds back into the fixpoint (a
   table-discovered function's own calls are direct evidence), so the
   two iterate together to the same bound `ROUNDS` enforces today.
5. `split_at_interior_entries` — unchanged; it acts on direct branch
   evidence only, which is what makes it safe to cut with.
6. **D6**: the residue the coverage type reports becomes regions; each
   region's canonical sweep runs *here*, in discovery, so its direct
   transfer targets feed steps 3–5 as witnesses — and what remains
   uncovered after that is translated as the saturated tier, every
   instruction start given a trampoline in the exec map.

A function carries its provenance — which witnesses produced it — from D2
on, and every discovery-related refusal and runtime exec-map miss reports
it: "reached `fn.0x511aa5`, discovered by data-array scan, bounded by the
FDE at `0x511b40`" is a diagnosable message where an address alone is an
afternoon.

## The shape of the code, which D2 changes

The plan is not purely additive. Discovery today is ~600 lines of free
functions inside `src/reader.rs`, and its shape cannot hold the invariant:

- `collect_functions` interleaves four witnesses in one body;
  `discover_from_transfers` runs a strong witness (the initialiser
  arrays) and a weak one (the transfer fixpoint) in the same pass, though
  the two have different permissions.
- The coverage and start-set bookkeeping is rebuilt ad hoc in
  `uncovered_functions` and again per round in `split_once` — the fact
  the invariant is *about* has no single owner.
- The gap-only rule is a convention each pass re-implements. Adding
  D3–D5 into this shape would grow it by accretion, and the invariant
  would be enforced nowhere — which is the pattern this project has
  already paid for: the overlay's `node()` lesson is that one check at
  the accessor makes "this number is upper" a fact, where a convention
  is a bug each caller is invited to write.

So D2 is a restructure, not a comment:

- **Discovery moves out of `reader.rs` into its own module** (`src/discover.rs`),
  leaving the reader what its name says — ELF parsing. The precedent is
  `src/seam.rs`: split out when "the two differ in the way that matters."
- **One type owns coverage**, holding the interval set, the start set,
  and every function's provenance — and answering `residue()`, the
  uncovered ranges D6's tier is built over — with exactly two doors:
  `establish(..)` for strong evidence, which may bound and may cut, and
  `fill(..)` for weak evidence, which the *type* refuses when the region
  is covered — the caller cannot get it wrong, because the permission
  lives in the API and not in each pass's memory of the rule.
- **Each witness is its own function** feeding evidence through one of
  the two doors, provenance attached at the door. A new witness is a new
  function and a choice of door, and nothing else changes.

D4 carries the decode consolidation for the same reason: today
`collect_transfer_targets` decodes every function, and `split_once`
decodes every function again per round. The operand harvest belongs in
that same pass, so D4 merges the decode loops into one that yields branch
targets, address-taken operands, and instruction boundaries together —
three consumers, one sweep — rather than adding a fourth loop.

## Milestones

**D1 — Diagnose the busybox unwind hole before building around it.**
Ghidra reaches near-total function recall on Linux binaries from
`.eh_frame` alone (Pang et al.), so a genuine 640 KB hole is unusual.
Cross-check `src/eh_frame.rs::frames` against `readelf
--debug-dump=frames` and against `.eh_frame_hdr`'s own FDE count on the
stripped busybox. Two outcomes: a parse or layout artifact (a terminator
read as the end, a second CIE shape) — a bug, fixed here, which may
shrink everything below; or the binary genuinely built without
asynchronous unwind tables — recorded, and D5 is confirmed necessary.
The verdict is a worklog entry either way. *Acceptance: the parser's FDE
count equals the external count, or the difference is explained and
recorded.*

**D2 — The invariant made structural.** The restructure described above:
discovery into `src/discover.rs`, the coverage type with its two doors,
each existing witness re-homed as its own function through the door its
stratum names — initialiser arrays split out of `discover_from_transfers`
and moved through `establish`, the transfer fixpoint through `fill`.
`Function` carries witness provenance; exec-map misses and discovery
refusals report it. No behavioral change to what is discovered — the
restructure is the kind the existing suite pins. *Acceptance: the
existing suite green unchanged; the discovered function list for the
stripped busybox and the static glibc hello byte-identical before and
after; a test asserts `fill` refuses a covered region; one test asserts a
transfer-discovered function reports transfer provenance and a
symbol-discovered one its symbol.*

**D3 — The free strong witnesses.** `e_entry`; the dynamic-relocation
discovery read (skip-unknown, never fatal), IRELATIVE addends and
RELATIVE text-targets as strong starts. *Acceptance: a stripped static
glibc binary's ifunc resolvers appear as functions with relocation
provenance, and the discovery read of a binary carrying an unmodelled
relocation type does not fail.*

**D4 — The operand harvest, and one decode pass.** RIP-relative and
immediate operands landing in text join the transfer fixpoint as weak
witnesses, padding-rejected — emitted by the same decode sweep that
collects branch targets and instruction boundaries, which
`split_at_interior_entries` then consumes instead of decoding everything
again per round. *Acceptance: a corpus fixture that registers a callback
by `lea` and calls it only indirectly translates without the callback
being reached by any other witness — deleting the harvest makes it fail;
and the merged sweep discovers and splits exactly what the separate loops
did on the standing fixtures.*

**D5 — The data-array witness and the negative filters.** *Built.* As
specified above, with the stride narrowed to eight by measurement and rule 4
refined to the stated/guessed distinction — which turned out to be the load-
bearing half: the applet table's targets are not in a *gap*, they are inside
an extent a transfer target guessed across a 631 KiB unwind hole, so a
gap-only witness alone would have found nothing. `busybox cat` and
`busybox ls` reach their applets. *Acceptance: the stripped busybox reaches its applet dispatch —
the indirect call through the applet table resolves — and a fixture
containing a computed-goto-shaped label table in data next to an
uncovered region is refused with the cluster log line, not shredded.*

**D6 — The floor: the saturated tier.** The design is the section above;
this is the work order. Read first, in this order: `emit_dispatcher`
(`src/structurer.rs:263`) and `translate_resume_function` above it — the
shape being reused; the plan loop at `src/transpile.rs:544–594` — how a
function grows a second body; `build_exec_map_table` and
`build_exec_map_lookup` (`src/transpile.rs:1973/2297`) — the map that
must not change; `emit_transfer`'s indirect arm (`src/translate.rs:635`)
— the call site that must not change; the fixpoint sweep
(`src/lifter.rs:167`).

*Step 0, the gate (S-gate, in the M0 style: hours, a yes/no, a recorded
verdict).* Nothing else starts until scale is measured. Generate a
synthetic module with ~150k trampolines calling ~40 piece-shaped
dispatcher functions of 4096 arms each, link it with `wasm-ld` alongside
a normal container, and instantiate under wasmtime. Record: link time,
validation time, compile time, memory, and any limit hit (function
count, table size, body size, `br_table` size). The piece cap and the
"one region body vs many" split are chosen from these numbers, not
assumed. If an engine limit refuses outright, the reroute is larger
pieces plus per-entry trampolines only for addresses some weak signal
touches — a smaller guarantee, recorded as such.

*Step 1.* `ControlFlowGraph::build_saturated` beside `build` and
`build_resumable` in `src/cfg.rs`: every instruction a leader, every
block one instruction, terminators as the existing kinds. Unit-test it
on a fixture the same way `build_resumable` is tested.

*Step 2.* Region plans in `transpile`: consume `Coverage::residue()`,
sweep each region (discovery has already done this — reuse its decode,
do not decode a third time), cut into pieces at the gated cap, and for
each piece push two bodies through the existing plan loop — ordinary
`(i32) -> ()` via `emit_dispatcher` with the parameter as state, resume
`(i32) -> i64` exactly as `plan.resume` bodies go today. Empty promotion
map for both.

*Step 3.* Trampolines: one `() -> ()` function per instruction start —
`i32.const k; call piece_n; end` — each given a table slot and
registered in `table_slots_by_location` under its `(section, offset)`.
That registration is the whole exec-map integration; diff the emitted
map against a hand-computed fixture entry to prove the builder needed
no change.

*Step 4.* The flag: `--saturate`, off in the relocatable pipeline
(meaningless there), on by default in `zaqaru-bake` once step 0's
numbers and step 5's fixtures are green — an arbitrary image is exactly
the caller who cannot be asked to opt in.

*Step 5, acceptance.* Three fixtures and a real one:

- A corpus binary built `-fno-asynchronous-unwind-tables`, stripped,
  whose `main` calls through a `.data` table of function pointers into
  functions reachable no other way — with the D5 data-array witness
  *disabled for the test*, so the tier is the only thing standing. It
  must run to the correct answer, its provenance must show the targets
  were tier-served, and the same binary with the tier off must die in
  `kisal_no_function_at` — the negative control that proves the fixture
  tests the tier and not the witnesses.
- A jump to a genuinely mid-instruction offset still dies loudly, with
  the miss naming the address — the boundary is a fact with a test, not
  a sentence.
- Byte-identical output for a container with no residue, and for
  everything with `--saturate` off — the tier is the empty set when the
  witnesses cover the binary, in the same way `--no-promote` is the
  empty map.
- The stripped busybox runs `busybox echo` end to end, whatever D1–D5
  left uncovered being served by the tier. This is the milestone's
  gate: the arbitrary-image guarantee demonstrated on the binary that
  posed the question.

## Pitfalls index

1. A data value that equals a covered function's interior address is not
   an error and not a discovery — it is skipped by the invariant. Do not
   "fix" the skip; it is the design (the CPython trap above).
2. ~~The applet-table shape is structs, not bare pointers.~~ **Measured
   and wrong** for this machine's busybox: 278 bare pointers at stride
   eight in `.data.rel.ro`. The generalisation to struct strides is real
   and unbuilt; this pitfall was a prediction about a binary nobody had
   looked at (D5 rule 2).
3. An immediate operand is a number. Without the padding check and the
   text-section check, D4 mints functions out of constants (D4).
4. The dynamic-relocation read must skip what it does not model. Refusal
   there re-breaks the stripped-busybox parse the worklog records
   (D3).
5. A weak witness discovered late must still feed the transfer fixpoint —
   a table-found function's own calls are direct evidence. Running D5
   once after the fixpoint instead of iterating with it under-discovers
   (pipeline step 4).
6. A symbol's stated size wins; the unwind extent sizes only *sizeless*
   symbols (`collect_functions`). A disagreement between a stated size
   and an FDE extent is not currently detected — worth a check under D2,
   since the two witnesses are independent and a disagreement means one
   of them is wrong about real bytes.
7. Only trampolines reach the function table and the exec map — never a
   piece body. Every `call_indirect` site in the program names
   `guest_type` (`() -> ()`), and a piece is `(i32) -> ()`; a piece in
   the table is a type mismatch trap at the first indirect entry (D6).
8. The region sweep must run in discovery, before translation, so its
   direct targets become witnesses and splits. A region translated from
   a private sweep emits direct calls into interiors the splitter never
   saw (D6, pipeline step 6).
9. A poisoned or mid-instruction offset gets no trampoline. Inventing an
   entry there to "improve coverage" is the silent answer — the decode
   says no instruction exists, and the loud miss is correct (D6).
10. A piece's resume body cannot `return_call` a sibling — the yield
    types disagree, the same rule as `src/translate.rs:556`. Ordinary
    body: `return_call`; resume body: plain call (D6).
11. Do not emit one piece per region. 150k `br_table` arms in one
    function is untested engine territory; the cap comes from the
    S-gate's measurements, not from optimism (D6, step 0).

## Sources

The survey this document rests on, in reading order of usefulness:

- Pang et al., *SoK: All You Ever Wanted to Know About x86/x64 Binary
  Disassembly But Were Afraid to Ask*, IEEE S&P 2021 — the measured
  account of every tool's heuristics; the precision numbers cited above.
  <https://arxiv.org/pdf/2007.14266>
- Flores-Montoya & Schulte, *Datalog Disassembly*, USENIX Security 2020 —
  the weighted-rule formulation; the function-entry/code-pointer line
  this design adopts. Rules readable at
  `src/datalog/basic_function_inference.dl` in
  <https://github.com/GrammaTech/ddisasm>.
- Kim, Kim & Cha, *FunProbe*, ESEC/FSE 2023 — sixteen hints under
  belief propagation; the negative-hint inventory.
  <https://softsec.kaist.ac.kr/~sangkilc/papers/kim-fse23.pdf>
- Andriesse et al., *An In-Depth Analysis of Disassembly on Full-Scale
  x86/x64 Binaries*, USENIX Security 2016 — linear sweep's measured
  accuracy on gcc/clang output; the prologue-truncation failure mode.
- Bauman et al., *Superset Disassembly* (Multiverse), NDSS 2018 — the
  discovery-free alternative and its measured cost.
- Kim et al., *SURI*, ASPLOS 2025, and *TVA*, 2025 — the PIE+CET-scoped
  soundness results; why `endbr64` is only sufficient when CET is an
  input invariant.
- Wang et al., *Ramblr*, NDSS 2017 — refuse-when-uncertain as published
  practice.
