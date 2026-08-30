//! Loading a linked program: its segments, and the stack it starts on.
//!
//! A relocatable guest is never loaded — `wasm-ld` places its data and its
//! operands resolve to symbols. A linked executable is the opposite: its
//! operands are addresses, and the only place they can be right is at those
//! addresses. So the segments are copied to their virtual addresses, which
//! linear memory can do because linear memory is ours; `baker::layout` is
//! what keeps the module's own data out of the way.
//!
//! Everything here is a pure function of bytes: parsing the headers,
//! deciding where things go, and building the block of memory a program
//! starts on. The one thing that is not — copying it all into the guest —
//! is a handful of stores at the end, so that the arithmetic that is easy to
//! get wrong is the part that gets tested natively.

use std::string::String;
use std::vec::Vec;

/// What went wrong loading a program.
///
/// Not an errno. Every one of these means the container cannot start, and a
/// plausible-looking failure code here would be a mystery two layers away.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Not an ELF at all, or not one this can run.
    NotLoadable(&'static str),
    /// A header points outside the file.
    Truncated(&'static str),
    /// The program headers are not in any loaded segment, so nothing can
    /// tell the program where they are. musl's own TLS setup reads them.
    HeadersUnmapped,
    /// The program wants addresses the module's own data occupies. This is
    /// a bake that did not reserve the region — see `baker::layout`.
    RegionOccupied { top: u64, data: u64 },
    /// The stack does not fit what has to go on it.
    StackTooSmall { needed: u64, region: u64 },
}

impl Error {
    pub fn message(&self, into: &mut String) {
        into.push_str("kisal: cannot load the program: ");
        match self {
            Error::NotLoadable(why) => into.push_str(why),
            Error::Truncated(what) => {
                into.push_str("the file ends inside its ");
                into.push_str(what);
            }
            Error::HeadersUnmapped => into.push_str(
                "its program headers are in no loaded segment, so `AT_PHDR` \
                 would name nothing",
            ),
            Error::RegionOccupied { top, data } => {
                into.push_str("it reaches ");
                push_hex(into, *top);
                into.push_str(", but the module's own data starts at ");
                push_hex(into, *data);
                into.push_str(" — the bake did not reserve the region");
            }
            Error::StackTooSmall { needed, region } => {
                into.push_str("its arguments need ");
                push_hex(into, *needed);
                into.push_str(" bytes of stack and the region is ");
                push_hex(into, *region);
            }
        }
    }
}

fn push_hex(into: &mut String, value: u64) {
    into.push_str("0x");
    let mut started = false;
    for shift in (0..16).rev() {
        let digit = ((value >> (shift * 4)) & 0xf) as u8;
        if digit == 0 && !started && shift != 0 {
            continue;
        }
        started = true;
        into.push(char::from(match digit {
            0..=9 => b'0' + digit,
            _ => b'a' + digit - 10,
        }));
    }
}

/// One `PT_LOAD`: bytes from the file, then zeros to the end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Load {
    pub offset: u64,
    pub address: u64,
    pub file_size: u64,
    pub memory_size: u64,
    pub readable: bool,
    pub writable: bool,
    pub executable: bool,
}

/// A linked executable or shared object, read far enough to place it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    pub entry: u64,
    /// Where the program headers will be *in memory*, which is what
    /// `AT_PHDR` means and is not the same as their file offset.
    pub headers: u64,
    pub header_size: u64,
    pub header_count: u64,
    pub loads: Vec<Load>,
    /// Whether the file states its own addresses or expects a base.
    pub kind: Kind,
    /// `PT_INTERP`'s bytes, as a file range: the path of the dynamic loader
    /// this program must be started through. `None` for a static one, and
    /// that absence is the whole of how the two are told apart at boot.
    pub interpreter: Option<(u64, u64)>,
    /// `PT_DYNAMIC`'s address and size, already at the bias. What holds
    /// `DT_NEEDED`, which is how a bake finds the libraries to translate.
    pub dynamic: Option<(u64, u64)>,
}

/// A file read, parsed, and given somewhere to go.
struct Loaded {
    file: Vec<u8>,
    program: Program,
    /// Whether *this* kernel chose the base, rather than the bake or the
    /// file itself. Only an unplaced file has to fit in a reserved region.
    placed: bool,
}

/// Whether a file states its own addresses or is placed at a base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `ET_EXEC`: the addresses in the file are the addresses, and the only
    /// bias it can be read at is zero.
    Fixed,
    /// `ET_DYN`: a shared object or position-independent executable, whose
    /// addresses are relative to a base someone else chooses. Here that
    /// someone is the bake — see the prelink design in `container-plan.md`.
    PositionIndependent,
}

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const CLASS_64: u8 = 2;
const LITTLE_ENDIAN: u8 = 1;
const TYPE_EXECUTABLE: u16 = 2;
const TYPE_SHARED: u16 = 3;
const MACHINE_X86_64: u16 = 62;

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PT_PHDR: u32 = 6;

const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

impl Program {
    /// Reads a file that states its own addresses.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        Self::parse_at(bytes, 0)
    }

    /// Reads a file as though a loader had placed it at `bias`.
    ///
    /// The mirror of `zaqaru::reader::ObjectFile::parse_at`, and it has to
    /// be: the translator resolved this file's operands at a base, and the
    /// loader has to put the bytes at the same one or every one of those
    /// operands points at nothing. Both halves take the same number from
    /// the bake, and this is the half that copies the bytes.
    pub fn parse_at(bytes: &[u8], bias: u64) -> Result<Self, Error> {
        if bytes.len() < 64 {
            return Err(Error::Truncated("header"));
        }
        if bytes[..4] != ELF_MAGIC {
            return Err(Error::NotLoadable("it is not an ELF file"));
        }
        if bytes[4] != CLASS_64 {
            return Err(Error::NotLoadable("it is not 64-bit"));
        }
        if bytes[5] != LITTLE_ENDIAN {
            return Err(Error::NotLoadable("it is not little-endian"));
        }
        let kind = match read_u16(bytes, 16) {
            TYPE_EXECUTABLE => Kind::Fixed,
            TYPE_SHARED => Kind::PositionIndependent,
            _ => {
                return Err(Error::NotLoadable(
                    "it is neither an executable nor a shared object",
                ));
            }
        };
        // A file that states its own addresses cannot be moved: its operands
        // were translated as the addresses they hold, so a bias would move
        // the program out from under its own code.
        if kind == Kind::Fixed && bias != 0 {
            return Err(Error::NotLoadable(
                "it is a fixed-address executable, which cannot be placed \
                 anywhere but at the addresses it states",
            ));
        }
        if read_u16(bytes, 18) != MACHINE_X86_64 {
            return Err(Error::NotLoadable("it is not x86-64"));
        }

        let entry = read_u64(bytes, 24) + bias;
        let table = read_u64(bytes, 32);
        let header_size = u64::from(read_u16(bytes, 54));
        let header_count = u64::from(read_u16(bytes, 56));
        if header_size < 56 {
            return Err(Error::NotLoadable("its program headers are too small"));
        }
        let span = header_size
            .checked_mul(header_count)
            .and_then(|span| span.checked_add(table))
            .ok_or(Error::Truncated("program header table"))?;
        if span > bytes.len() as u64 {
            return Err(Error::Truncated("program header table"));
        }

        let mut loads = Vec::new();
        let mut stated_headers = None;
        let mut interpreter = None;
        let mut dynamic = None;
        for index in 0..header_count {
            let at = (table + index * header_size) as usize;
            let segment = read_u32(bytes, at);
            if segment == PT_PHDR {
                stated_headers = Some(read_u64(bytes, at + 16) + bias);
                continue;
            }
            if segment == PT_INTERP {
                let offset = read_u64(bytes, at + 8);
                let size = read_u64(bytes, at + 32);
                let end = offset
                    .checked_add(size)
                    .ok_or(Error::Truncated("interpreter path"))?;
                if end > bytes.len() as u64 {
                    return Err(Error::Truncated("interpreter path"));
                }
                interpreter = Some((offset, size));
                continue;
            }
            if segment == PT_DYNAMIC {
                dynamic = Some((read_u64(bytes, at + 16) + bias, read_u64(bytes, at + 40)));
                continue;
            }
            if segment != PT_LOAD {
                continue;
            }
            let flags = read_u32(bytes, at + 4);
            let load = Load {
                offset: read_u64(bytes, at + 8),
                address: read_u64(bytes, at + 16) + bias,
                file_size: read_u64(bytes, at + 32),
                memory_size: read_u64(bytes, at + 40),
                readable: flags & PF_R != 0,
                writable: flags & PF_W != 0,
                executable: flags & PF_X != 0,
            };
            if load.file_size > load.memory_size {
                return Err(Error::NotLoadable(
                    "a segment claims more bytes in the file than in memory",
                ));
            }
            let end = load
                .offset
                .checked_add(load.file_size)
                .ok_or(Error::Truncated("segment"))?;
            if end > bytes.len() as u64 {
                return Err(Error::Truncated("segment"));
            }
            load.address
                .checked_add(load.memory_size)
                .ok_or(Error::NotLoadable("a segment wraps the address space"))?;
            loads.push(load);
        }
        if loads.is_empty() {
            return Err(Error::NotLoadable("it has no loadable segments"));
        }

        // Where the headers end up in memory. `PT_PHDR` states it when the
        // linker emitted one; otherwise it is wherever the segment covering
        // their file offset puts them. A program whose headers are in no
        // segment cannot be told where they are, and musl reads them to find
        // `PT_TLS` — so this is refused rather than filled in with a lie.
        let headers = stated_headers
            .or_else(|| {
                loads.iter().find_map(|load| {
                    (table >= load.offset && table < load.offset + load.file_size)
                        .then(|| load.address + (table - load.offset))
                })
            })
            .ok_or(Error::HeadersUnmapped)?;

        Ok(Self {
            entry,
            headers,
            header_size,
            header_count,
            loads,
            kind,
            interpreter,
            dynamic,
        })
    }

    /// The interpreter's path, as the bytes of `PT_INTERP` without its
    /// terminating null.
    pub fn interpreter_path<'a>(&self, bytes: &'a [u8]) -> Option<&'a [u8]> {
        let (offset, size) = self.interpreter?;
        let path = bytes.get(offset as usize..(offset + size) as usize)?;
        Some(match path.iter().position(|byte| *byte == 0) {
            Some(end) => &path[..end],
            None => path,
        })
    }

    /// The lowest address any segment occupies.
    pub fn base(&self) -> u64 {
        self.loads
            .iter()
            .map(|load| load.address)
            .min()
            .unwrap_or(0)
    }

    /// One past the highest address any segment occupies.
    pub fn top(&self) -> u64 {
        self.loads
            .iter()
            .map(|load| load.address + load.memory_size)
            .max()
            .unwrap_or(0)
    }
}

fn read_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn read_u64(bytes: &[u8], at: usize) -> u64 {
    let mut value = [0u8; 8];
    value.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(value)
}

/// The auxiliary vector's tags, as `_start` and every libc read them.
pub mod auxv {
    pub const NULL: u64 = 0;
    pub const PHDR: u64 = 3;
    pub const PHENT: u64 = 4;
    pub const PHNUM: u64 = 5;
    pub const PAGESZ: u64 = 6;
    /// The dynamic loader's own base. Zero here says there is not one,
    /// which is the truth for a static executable.
    pub const BASE: u64 = 7;
    pub const ENTRY: u64 = 9;
    pub const UID: u64 = 11;
    pub const EUID: u64 = 12;
    pub const GID: u64 = 13;
    pub const EGID: u64 = 14;
    pub const CLKTCK: u64 = 17;
    /// Sixteen bytes libc uses to seed stack canaries and hash tables.
    pub const RANDOM: u64 = 25;
    pub const SECURE: u64 = 23;
    /// The vDSO's base, which is *deliberately never supplied*. Without it
    /// libc has no user-space clock to call and issues `clock_gettime` as a
    /// real syscall — which is the only form kisal can answer.
    pub const SYSINFO_EHDR: u64 = 33;
}

/// How many random bytes `AT_RANDOM` points at, fixed by the ABI.
pub const RANDOM_BYTES: usize = 16;

/// The block of memory a program starts on, and where its stack pointer
/// begins in it.
pub struct Stack {
    /// Where in the guest these bytes go.
    pub address: u64,
    pub bytes: Vec<u8>,
    /// What `%rsp` holds when the entry point runs.
    pub stack_pointer: u64,
}

/// Builds the initial process stack.
///
/// The layout is the one `_start` is compiled against, and it is a layout
/// rather than a convention: `%rsp` points at `argc`, the argument pointers
/// follow, then a null, then the environment pointers, then a null, then the
/// auxiliary vector as tag/value pairs ending in `AT_NULL`. Above all of
/// that live the strings the pointers point at, because they are variable
/// length and the fixed part has to be indexable.
///
/// `%rsp` is sixteen-byte aligned, which is not decoration either: the SysV
/// ABI promises it at process entry and compilers emit aligned SSE stores
/// against that promise from the first function onward.
pub fn build_stack(
    region: (u64, u64),
    argv: &[&[u8]],
    envp: &[&[u8]],
    program: &Program,
    interpreter: Option<u64>,
    random: &[u8; RANDOM_BYTES],
) -> Result<Stack, Error> {
    let (low, high) = region;
    let size = high - low;

    // The strings first, descending from the top, since the fixed part below
    // them has to know where each one landed.
    let mut strings: Vec<u8> = Vec::new();
    let mut top = high;

    let place = |bytes: &[u8], strings: &mut Vec<u8>, top: &mut u64| -> u64 {
        // Grows downward, so each new string goes in front of the ones
        // already placed.
        let mut next = Vec::with_capacity(bytes.len() + 1 + strings.len());
        next.extend_from_slice(bytes);
        next.push(0);
        next.extend_from_slice(strings);
        *strings = next;
        *top -= bytes.len() as u64 + 1;
        *top
    };

    let random_at = {
        let mut next = Vec::with_capacity(RANDOM_BYTES + strings.len());
        next.extend_from_slice(random);
        next.extend_from_slice(&strings);
        strings = next;
        top -= RANDOM_BYTES as u64;
        top
    };
    let environment: Vec<u64> = envp
        .iter()
        .rev()
        .map(|entry| place(entry, &mut strings, &mut top))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let arguments: Vec<u64> = argv
        .iter()
        .rev()
        .map(|entry| place(entry, &mut strings, &mut top))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let vector: Vec<(u64, u64)> = std::vec![
        (auxv::PHDR, program.headers),
        (auxv::PHENT, program.header_size),
        (auxv::PHNUM, program.header_count),
        (auxv::PAGESZ, crate::space::PAGE),
        // Where the dynamic loader was placed, or zero when there is none.
        // `ld.so` reads this to find itself, and the *program's* entry has
        // to be `AT_ENTRY` rather than the loader's, because the loader
        // finishes its work by jumping there.
        (auxv::BASE, interpreter.unwrap_or(0)),
        (auxv::ENTRY, program.entry),
        (auxv::UID, 0),
        (auxv::EUID, 0),
        (auxv::GID, 0),
        (auxv::EGID, 0),
        (auxv::SECURE, 0),
        (auxv::CLKTCK, 100),
        (auxv::RANDOM, random_at),
        (auxv::NULL, 0),
    ];

    let words = 1 + arguments.len() + 1 + environment.len() + 1 + vector.len() * 2;
    let fixed = words as u64 * 8;
    let strings_at = top;
    // Aligned downward, so the gap between the fixed part and the strings
    // absorbs the padding rather than the entry point receiving a stack
    // pointer that is off by eight.
    let pointer = strings_at.checked_sub(fixed).ok_or(Error::StackTooSmall {
        needed: fixed,
        region: size,
    })? & !0xfu64;
    if pointer < low {
        return Err(Error::StackTooSmall {
            needed: high - pointer,
            region: size,
        });
    }

    let mut bytes = Vec::with_capacity((high - pointer) as usize);
    let word = |bytes: &mut Vec<u8>, value: u64| bytes.extend_from_slice(&value.to_le_bytes());
    word(&mut bytes, arguments.len() as u64);
    for argument in &arguments {
        word(&mut bytes, *argument);
    }
    word(&mut bytes, 0);
    for entry in &environment {
        word(&mut bytes, *entry);
    }
    word(&mut bytes, 0);
    for (tag, value) in &vector {
        word(&mut bytes, *tag);
        word(&mut bytes, *value);
    }
    debug_assert_eq!(bytes.len() as u64, fixed);

    // The padding the alignment introduced, then the strings on top.
    bytes.resize((strings_at - pointer) as usize, 0);
    bytes.extend_from_slice(&strings);
    debug_assert_eq!(bytes.len() as u64, high - pointer);

    Ok(Stack {
        address: pointer,
        bytes,
        stack_pointer: pointer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but real ELF64 executable header plus program headers, so
    /// that the refusals can be provoked one field at a time.
    fn elf(kind: u16, machine: u16, headers: &[[u64; 6]], trailing: usize) -> Vec<u8> {
        let entry_size = 56usize;
        let table = 64u64;
        let mut bytes = std::vec![0u8; 64 + headers.len() * entry_size + trailing];
        bytes[..4].copy_from_slice(&ELF_MAGIC);
        bytes[4] = CLASS_64;
        bytes[5] = LITTLE_ENDIAN;
        bytes[16..18].copy_from_slice(&kind.to_le_bytes());
        bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        bytes[24..32].copy_from_slice(&0x401000u64.to_le_bytes());
        bytes[32..40].copy_from_slice(&table.to_le_bytes());
        bytes[54..56].copy_from_slice(&(entry_size as u16).to_le_bytes());
        bytes[56..58].copy_from_slice(&(headers.len() as u16).to_le_bytes());
        for (index, header) in headers.iter().enumerate() {
            let at = 64 + index * entry_size;
            // type, flags, offset, vaddr, filesz, memsz
            bytes[at..at + 4].copy_from_slice(&(header[0] as u32).to_le_bytes());
            bytes[at + 4..at + 8].copy_from_slice(&(header[1] as u32).to_le_bytes());
            bytes[at + 8..at + 16].copy_from_slice(&header[2].to_le_bytes());
            bytes[at + 16..at + 24].copy_from_slice(&header[3].to_le_bytes());
            bytes[at + 32..at + 40].copy_from_slice(&header[4].to_le_bytes());
            bytes[at + 40..at + 48].copy_from_slice(&header[5].to_le_bytes());
        }
        bytes
    }

    /// One segment covering the headers, which is the ordinary shape.
    fn ordinary() -> Vec<u8> {
        elf(
            TYPE_EXECUTABLE,
            MACHINE_X86_64,
            &[
                [PT_LOAD as u64, u64::from(PF_R), 0, 0x400000, 0x200, 0x200],
                [
                    PT_LOAD as u64,
                    u64::from(PF_R | PF_X),
                    0x200,
                    0x401000,
                    0x40,
                    0x1000,
                ],
            ],
            0x240,
        )
    }

    #[test]
    fn a_program_says_where_its_segments_and_headers_go() {
        let program = Program::parse(&ordinary()).expect("parse");
        assert_eq!(program.entry, 0x401000);
        assert_eq!(program.base(), 0x400000);
        // The second segment's memory size exceeds its file size, which is
        // what `.bss` looks like, and the top has to follow memory.
        assert_eq!(program.top(), 0x402000);
        // No `PT_PHDR`, so the headers are found through the segment that
        // covers their file offset: offset 64 inside a segment mapped at
        // 0x400000 from offset 0.
        assert_eq!(program.headers, 0x400040);
        assert_eq!(program.header_count, 2);
        assert!(program.loads[1].executable && !program.loads[1].writable);
    }

    #[test]
    fn a_stated_header_address_is_preferred_to_a_deduced_one() {
        let mut bytes = ordinary();
        // Add a `PT_PHDR` naming somewhere else entirely, which is what a
        // linker emits and what the program will believe.
        let table = 64u64 + 2 * 56;
        bytes[56..58].copy_from_slice(&3u16.to_le_bytes());
        bytes.resize(bytes.len() + 56, 0);
        // Shift the trailing bytes is unnecessary here: the new header goes
        // straight after the two that exist.
        let at = table as usize;
        bytes[at..at + 4].copy_from_slice(&PT_PHDR.to_le_bytes());
        bytes[at + 16..at + 24].copy_from_slice(&0x4000c0u64.to_le_bytes());
        assert_eq!(Program::parse(&bytes).expect("parse").headers, 0x4000c0);
    }

    #[test]
    fn headers_in_no_segment_are_refused() {
        // One segment, mapped from a file offset past the header table, so
        // nothing covers it.
        let bytes = elf(
            TYPE_EXECUTABLE,
            MACHINE_X86_64,
            &[[
                PT_LOAD as u64,
                u64::from(PF_R | PF_X),
                0x200,
                0x401000,
                0x40,
                0x40,
            ]],
            0x240,
        );
        assert_eq!(Program::parse(&bytes), Err(Error::HeadersUnmapped));
    }

    /// A shared object is placed at the bias the bake assigned it, and
    /// every address it states moves with it.
    #[test]
    fn a_shared_object_is_placed_at_its_bias() {
        let bytes = elf(
            TYPE_SHARED,
            MACHINE_X86_64,
            &[[PT_LOAD as u64, 4, 0, 0, 0x200, 0x200]],
            0x200,
        );
        let program = Program::parse_at(&bytes, 0x1000_0000).expect("parse");
        assert_eq!(program.kind, Kind::PositionIndependent);
        assert_eq!(program.base(), 0x1000_0000);
        assert_eq!(program.entry, 0x1000_0000 + 0x401000);
        // The headers are at file offset 64 inside a segment mapped from
        // offset zero, so they move with everything else.
        assert_eq!(program.headers, 0x1000_0040);
    }

    /// A fixed-address executable cannot be moved, and asking is an error
    /// rather than a silent relocation: its operands were translated as the
    /// addresses they hold.
    #[test]
    fn a_fixed_executable_refuses_a_bias() {
        assert!(matches!(
            Program::parse_at(&ordinary(), 0x1000_0000),
            Err(Error::NotLoadable(_))
        ));
    }

    #[test]
    fn a_segment_running_past_the_file_is_refused() {
        let bytes = elf(
            TYPE_EXECUTABLE,
            MACHINE_X86_64,
            &[[PT_LOAD as u64, 4, 0, 0x400000, 0x10000, 0x10000]],
            0x100,
        );
        assert_eq!(Program::parse(&bytes), Err(Error::Truncated("segment")));
    }

    fn read_word(stack: &Stack, address: u64) -> u64 {
        let at = (address - stack.address) as usize;
        let mut value = [0u8; 8];
        value.copy_from_slice(&stack.bytes[at..at + 8]);
        u64::from_le_bytes(value)
    }

    fn read_string(stack: &Stack, address: u64) -> &[u8] {
        let at = (address - stack.address) as usize;
        let end = at
            + stack.bytes[at..]
                .iter()
                .position(|byte| *byte == 0)
                .expect("a terminator");
        &stack.bytes[at..end]
    }

    #[test]
    fn the_initial_stack_is_what_start_is_compiled_against() {
        let program = Program::parse(&ordinary()).expect("parse");
        let region = (0x1000_0000u64, 0x1080_0000u64);
        let argv: [&[u8]; 3] = [b"python3", b"-c", b"print(1)"];
        let envp: [&[u8]; 2] = [b"PATH=/usr/bin", b"HOME=/root"];
        let random = [0x5au8; RANDOM_BYTES];
        let stack = build_stack(region, &argv, &envp, &program, None, &random).expect("build");

        assert_eq!(
            stack.stack_pointer % 16,
            0,
            "the ABI promises sixteen-byte alignment at entry"
        );
        assert!(stack.stack_pointer >= region.0);
        assert_eq!(stack.address + stack.bytes.len() as u64, region.1);

        let mut at = stack.stack_pointer;
        assert_eq!(read_word(&stack, at), argv.len() as u64, "argc");
        at += 8;
        for expected in argv {
            assert_eq!(read_string(&stack, read_word(&stack, at)), expected);
            at += 8;
        }
        assert_eq!(read_word(&stack, at), 0, "argv is not terminated");
        at += 8;
        for expected in envp {
            assert_eq!(read_string(&stack, read_word(&stack, at)), expected);
            at += 8;
        }
        assert_eq!(read_word(&stack, at), 0, "envp is not terminated");
        at += 8;

        let mut vector = std::vec::Vec::new();
        loop {
            let tag = read_word(&stack, at);
            let value = read_word(&stack, at + 8);
            at += 16;
            if tag == auxv::NULL {
                break;
            }
            vector.push((tag, value));
        }

        let of = |tag: u64| {
            vector
                .iter()
                .find(|(candidate, _)| *candidate == tag)
                .map(|(_, value)| *value)
        };
        assert_eq!(of(auxv::PHDR), Some(program.headers));
        assert_eq!(of(auxv::PHENT), Some(program.header_size));
        assert_eq!(of(auxv::PHNUM), Some(program.header_count));
        assert_eq!(of(auxv::ENTRY), Some(program.entry));
        assert_eq!(of(auxv::PAGESZ), Some(crate::space::PAGE));
        assert_eq!(of(auxv::BASE), Some(0), "a static program has no loader");
        assert_eq!(
            of(auxv::SYSINFO_EHDR),
            None,
            "a vDSO was advertised, so libc will not issue clock syscalls"
        );

        let random_at = of(auxv::RANDOM).expect("AT_RANDOM");
        let at = (random_at - stack.address) as usize;
        assert_eq!(&stack.bytes[at..at + RANDOM_BYTES], &random);
    }

    #[test]
    fn a_program_reaching_into_the_modules_data_is_refused() {
        let program = Program::parse(&ordinary()).expect("parse");
        // The bake that reserved the region: data above everything the
        // program occupies.
        assert_eq!(check_region(&program, program.top()), Ok(()));
        assert_eq!(check_region(&program, 0x8000_0000), Ok(()));
        // And the bake that did not, which is the linker's own default and
        // would put `__image_blob` across the program's addresses.
        assert_eq!(
            check_region(&program, 1024),
            Err(Error::RegionOccupied {
                top: 0x402000,
                data: 1024
            })
        );
    }

    #[test]
    fn every_refusal_says_what_it_was() {
        // The messages are the whole point of these not being errnos, so
        // they are checked rather than assumed to exist.
        for error in [
            Error::NotLoadable("it is not x86-64"),
            Error::Truncated("segment"),
            Error::HeadersUnmapped,
            Error::RegionOccupied {
                top: 0x402000,
                data: 1024,
            },
            Error::StackTooSmall {
                needed: 0x2000,
                region: 0x800,
            },
        ] {
            let mut message = String::new();
            error.message(&mut message);
            assert!(message.starts_with("kisal: cannot load the program: "));
            assert!(message.len() > 40, "a refusal that says nothing: {message}");
        }
        let mut message = String::new();
        Error::RegionOccupied {
            top: 0x402000,
            data: 1024,
        }
        .message(&mut message);
        assert!(
            message.contains("0x402000") && message.contains("0x400"),
            "the numbers that identify the bake are missing: {message}"
        );
    }

    #[test]
    fn a_stack_too_small_for_its_arguments_is_refused() {
        let program = Program::parse(&ordinary()).expect("parse");
        let huge = std::vec![b'x'; 4096];
        let argv: [&[u8]; 1] = [&huge];
        assert!(matches!(
            build_stack((0x1000, 0x1800), &argv, &[], &program, None, &[0; RANDOM_BYTES]),
            Err(Error::StackTooSmall { .. })
        ));
    }
}

/// Where the module's own data begins.
///
/// `wasm-ld` defines this, and `baker::layout` is what decides its value: a
/// container carrying a program to load has it placed above everything that
/// program will occupy. Reading it here turns "the bake reserved the region"
/// from an assumption into a check, at the one moment when saying so is
/// still useful.
#[cfg(target_arch = "wasm32")]
pub fn module_data_base() -> u64 {
    unsafe extern "C" {
        #[link_name = "__global_base"]
        static GLOBAL_BASE: u8;
    }
    // The address of a linker-defined symbol, which is all this is: the
    // byte itself is never read, so nothing is dereferenced.
    (&raw const GLOBAL_BASE) as u64
}

/// Natively there is no module and nothing to collide with, so the region is
/// unbounded and the check below always passes.
#[cfg(not(target_arch = "wasm32"))]
pub fn module_data_base() -> u64 {
    u64::MAX
}

/// Refuses a position-independent file the bake never placed.
///
/// A base of zero is a real answer for a fixed-address executable and a
/// meaningless one for a shared object: its code was resolved at some
/// address by the translator, and if the image carries no record of which,
/// then this file was never translated. Loading it anyway would put bytes at
/// address zero that no exec-map entry describes, and the first indirect
/// call through them would report a miss for an address that was never the
/// question. Saying so here names the file instead.
fn untranslated(program: &Program, base: u64) -> Result<(), Error> {
    match program.kind == Kind::PositionIndependent && base == 0 {
        true => Err(Error::NotLoadable(
            "it is a shared object the bake did not translate, so there is no \
             address its code was resolved at",
        )),
        false => Ok(()),
    }
}

/// Whether a program fits in the region reserved for it.
pub fn check_region(program: &Program, data: u64) -> Result<(), Error> {
    let top = program.top();
    if top > data {
        return Err(Error::RegionOccupied { top, data });
    }
    Ok(())
}

/// A segment's protection, as the address space records it.
fn prot_of(load: &Load) -> i32 {
    let mut prot = crate::space::prot::NONE;
    if load.readable {
        prot |= crate::space::prot::READ;
    }
    if load.writable {
        prot |= crate::space::prot::WRITE;
    }
    if load.executable {
        prot |= crate::space::prot::EXEC;
    }
    prot
}

/// How much stack a process starts with, matching Linux's default
/// `RLIMIT_STACK`. It is an ordinary mapping, so `/proc/self/maps` shows it
/// and `pthread_getattr_np` can find its bounds like any other.
pub const STACK_BYTES: u64 = 8 * 1024 * 1024;

impl<S: crate::abi::Store, M: crate::machine::Machine> crate::syscall::Kernel<'_, S, M> {
    /// Loads a program and leaves the machine ready to enter it.
    ///
    /// Returns the entry point's virtual address, which the boot path hands
    /// to the module's trampoline. Every register except `%rsp` is already
    /// what Linux hands `_start` — zero — because a wasm global starts at
    /// zero and nothing has run yet.
    pub fn exec(&mut self, path: &[u8], argv: &[&[u8]], envp: &[&[u8]]) -> Result<u64, Error> {
        // A transient copy, because the segments are then written into
        // guest memory while the image is still borrowed. It is only safe
        // to allocate this much at all because the bake put the allocator
        // above the region the program is about to occupy — see
        // `baker::layout`.
        let loaded = self.load(path)?;
        let (file, program) = (loaded.file, loaded.program);

        // A dynamic program is not entered directly. `PT_INTERP` names the
        // loader that must run first — it maps the libraries, applies the
        // relocations, and only then jumps to the program's own entry — and
        // the loader is an ordinary translated module like any other, placed
        // by the same bake at its own base. So both files are loaded, the
        // auxiliary vector says where each of them went, and control goes to
        // the loader.
        let interpreter = match program.interpreter_path(&file) {
            Some(interpreter) => {
                let loader = self.load(interpreter).map_err(|_| {
                    Error::NotLoadable(
                        "the dynamic loader its `PT_INTERP` names is not in the image",
                    )
                })?;
                Some((loader.file, loader.program, loader.placed))
            }
            None => None,
        };

        // The region check applies to whatever the *bake* had to reserve
        // room for, which is only a file that states its own addresses. A
        // position-independent one was placed a moment ago by
        // `reserve_object`, out of the address space above the module's own
        // data — so checking it against the module's data base would be
        // checking whether the address space is below itself, and a Python
        // container fails that check by two hundred megabytes.
        let stated = core::iter::once((&program, loaded.placed))
            .chain(
                interpreter
                    .iter()
                    .map(|(_, loader, placed)| (loader, *placed)),
            )
            .filter(|(_, placed)| !placed)
            .map(|(program, _)| program.top())
            .max();
        if let Some(top) = stated
            && top > module_data_base()
        {
            return Err(Error::RegionOccupied {
                top,
                data: module_data_base(),
            });
        }

        for (file, program) in core::iter::once((&file, &program))
            .chain(interpreter.iter().map(|(bytes, loader, _)| (bytes, loader)))
        {
            self.place(file, program)?;
        }

        let stack = self.reserve_stack()?;
        let random = self.random_bytes();
        let built = build_stack(
            stack,
            argv,
            envp,
            &program,
            interpreter.as_ref().map(|(_, loader, _)| loader.base()),
            &random,
        )?;
        // SAFETY: the region came from the address space, which grew memory
        // to cover it, and the block ends exactly at the region's top.
        unsafe {
            self.memory_mut()
                .write(built.address, &built.bytes)
                .map_err(|_| Error::StackTooSmall {
                    needed: built.bytes.len() as u64,
                    region: stack.1 - stack.0,
                })?;
        }
        self.machine.set_stack_pointer(built.stack_pointer as i64);
        // A new program gets a new floating-point unit. Nothing else resets
        // it, and its state does not live in the register file the rest of
        // this function sets up — it is inside the `x87` crate, which is why
        // this is a call rather than a store.
        self.machine.reset_floating_point();

        // `/proc/self/exe` is a fact about the program that was started, and
        // this is the moment there is one.
        if let Ok(text) = core::str::from_utf8(path) {
            self.set_executable(text);
        }
        Ok(interpreter
            .as_ref()
            .map_or(program.entry, |(_, loader, _)| loader.entry))
    }

    /// Copies one file's segments to the addresses they belong at.
    fn place(&mut self, file: &[u8], program: &Program) -> Result<(), Error> {
        for load in &program.loads {
            // Recorded as a mapping before anything is written into it. The
            // guest's own memory is not special to it: glibc applies
            // read-only relocation protection with `mprotect` over its data
            // segment moments after starting, and a region nothing has
            // mapped has nothing to protect. `/proc/self/maps` renders the
            // same tree, so this is also what makes a program visible to
            // itself.
            let start = load.address - load.address % crate::space::PAGE;
            let end = (load.address + load.memory_size).next_multiple_of(crate::space::PAGE);
            let machine = &mut self.machine;
            let pages = &mut self.pages;
            let enforcement = self.enforcement;
            let request = crate::space::Request {
                hint: start,
                length: end - start,
                prot: prot_of(load),
                // Fixed, because the address is the program's and not ours
                // to choose; private and anonymous, because the bytes are
                // copied in rather than shared with anything.
                flags: crate::space::map::FIXED
                    | crate::space::map::PRIVATE
                    | crate::space::map::ANONYMOUS,
                backing: crate::space::Backing::Anonymous,
            };
            self.space
                .map(&request, &mut |to| crate::syscall::grow_memory(machine, pages, enforcement, to))
                .map_err(|_| Error::RegionOccupied {
                    top: program.top(),
                    data: module_data_base(),
                })?;
            // The page table learns the segment too, and with the
            // protections the ELF asked for: a program's text is
            // read-execute, and under the interpreter that is what makes an
            // instruction fetch from its data a fault rather than a
            // surprise.
            self.sync_pages(start, end);

            // Read before the address space is borrowed: the error path
            // wants it, and the borrow lasts until the segment is placed.
            let limit = self.machine.memory_limit();
            let mut memory = self.memory_mut();
            memory
                .check(load.address, load.memory_size)
                .map_err(|_| Error::RegionOccupied {
                    top: program.top(),
                    data: limit,
                })?;
            // Zeros first, then the file's bytes over the top: the part of a
            // segment past its file size is `.bss`, and it has to read as
            // zeros whatever was there before.
            //
            // The kernel's own write, not the guest's: a program's text is
            // read-execute and its rodata read-only, and both still have to
            // be loaded.
            memory
                .place_fill(load.address, load.memory_size, 0)
                .and_then(|()| {
                    memory.place(
                        load.address,
                        &file[load.offset as usize..(load.offset + load.file_size) as usize],
                    )
                })
                .map_err(|_| Error::NotLoadable("a segment does not fit in memory"))?;
        }
        Ok(())
    }

    /// Reads a file and decides where it goes.
    ///
    /// Two worlds meet here. In the ahead-of-time world a
    /// position-independent file has one possible address — the one the bake
    /// resolved its code at — so the base comes out of the image and a file
    /// without one cannot be loaded at all: its operands point at nothing.
    ///
    /// Under the interpreter there is nothing to resolve. Code is data, so a
    /// shared object goes wherever there is room, which is what a real
    /// kernel does and what the prelink design existed to avoid having to
    /// do. **This is where prelink disappears**: the bake stops assigning
    /// bases, the image stops carrying them, and the address space answers
    /// the question at load time instead.
    fn load(&mut self, path: &[u8]) -> Result<Loaded, Error> {
        let (file, recorded) = self.read_whole(path)?;
        let program = Program::parse_at(&file, recorded)?;
        if recorded != 0 || program.kind != Kind::PositionIndependent {
            // Either the bake placed it, or it states its own addresses.
            untranslated(&program, recorded)?;
            return Ok(Loaded {
                file,
                program,
                placed: false,
            });
        }
        if self.enforcement != crate::syscall::Enforcement::Mapped {
            // The ahead-of-time world, where an unplaced shared object has
            // no address its code was resolved at.
            untranslated(&program, recorded)?;
            unreachable!("`untranslated` refuses exactly this case");
        }
        let base = self.reserve_object(&program)?;
        let program = Program::parse_at(&file, base)?;
        Ok(Loaded {
            file,
            program,
            placed: true,
        })
    }

    /// Finds room for a position-independent file, and answers the base.
    ///
    /// Reserved as one `PROT_NONE` mapping spanning the whole object, which
    /// is what a real loader does and what makes the object's segments
    /// contiguous: `place` then maps each segment over its part of the
    /// reservation with the protections the ELF asked for, and the gaps
    /// between segments stay unreachable rather than becoming somebody
    /// else's mapping.
    fn reserve_object(&mut self, program: &Program) -> Result<u64, Error> {
        let span = program.top().saturating_sub(program.base());
        if span == 0 {
            return Err(Error::NotLoadable("it has no loadable segments"));
        }
        let request = crate::space::Request {
            hint: 0,
            length: span.next_multiple_of(crate::space::PAGE),
            prot: crate::space::prot::NONE,
            flags: crate::space::map::PRIVATE | crate::space::map::ANONYMOUS,
            backing: crate::space::Backing::Anonymous,
        };
        let machine = &mut self.machine;
        let pages = &mut self.pages;
        let enforcement = self.enforcement;
        let (address, _) = self
            .space
            .map(&request, &mut |to| {
                crate::syscall::grow_memory(machine, pages, enforcement, to)
            })
            .map_err(|_| Error::NotLoadable("there is no room for it in the address space"))?;
        // The base is the reservation's start less whatever the object's own
        // lowest segment address is, so that a file whose segments start
        // above zero still lands where its own addresses say.
        Ok(address.wrapping_sub(program.base()))
    }

    /// The whole of a file, as bytes, and the base the bake placed it at.
    fn read_whole(&self, path: &[u8]) -> Result<(Vec<u8>, u64), Error> {
        let root = self.vfs.root();
        let vnode = self
            .vfs
            .resolve(root, path, crate::vfs::Lookup::FOLLOW)
            .map_err(|_| Error::NotLoadable("there is no such file in the image"))?;
        let inode = self
            .vfs
            .inode(vnode)
            .map_err(|_| Error::NotLoadable("its inode cannot be read"))?;
        if !inode.is_regular() {
            return Err(Error::NotLoadable("it is not a regular file"));
        }
        let filesystem = self
            .vfs
            .filesystem_of(vnode)
            .map_err(|_| Error::NotLoadable("its filesystem cannot be read"))?;
        let contents = filesystem
            .contents(&inode, vnode.inode)
            .map_err(|_| Error::NotLoadable("its contents cannot be read"))?;
        // A file the bake did not translate has no base, and a
        // position-independent one then has nowhere it can go — its code was
        // never resolved at any address. A fixed-address executable is the
        // other case and needs no base, which is why zero is the answer
        // rather than a refusal here: `Program::parse_at` is what refuses a
        // shared object it cannot place.
        let base = filesystem.prelink_base(vnode.inode).unwrap_or(0);
        Ok((contents.to_vec(), base))
    }

    /// The initial stack's region, as an ordinary anonymous mapping.
    fn reserve_stack(&mut self) -> Result<(u64, u64), Error> {
        let request = crate::space::Request {
            hint: 0,
            length: STACK_BYTES,
            prot: crate::space::prot::READ | crate::space::prot::WRITE,
            flags: crate::space::map::PRIVATE
                | crate::space::map::ANONYMOUS
                | crate::space::map::STACK,
            backing: crate::space::Backing::Anonymous,
        };
        let machine = &mut self.machine;
        let pages = &mut self.pages;
        let enforcement = self.enforcement;
        let (address, fill) = self
            .space
            .map(&request, &mut |to| crate::syscall::grow_memory(machine, pages, enforcement, to))
            .map_err(|_| Error::StackTooSmall {
                needed: STACK_BYTES,
                region: 0,
            })?;
        self.sync_pages(address, address + STACK_BYTES);
        if fill.length != 0 {
            // SAFETY: the range was just reserved inside the guest's memory.
            unsafe {
                self.memory_mut()
                    .fill(fill.start, fill.length, 0)
                    .map_err(|_| Error::StackTooSmall {
                        needed: STACK_BYTES,
                        region: 0,
                    })?;
            }
        }
        Ok((address, address + STACK_BYTES))
    }

    /// The sixteen bytes `AT_RANDOM` points at.
    ///
    /// From the kernel's own CSPRNG, so they are part of the same seeded
    /// stream everything else draws from and a run replays identically.
    fn random_bytes(&mut self) -> [u8; RANDOM_BYTES] {
        let mut bytes = [0u8; RANDOM_BYTES];
        // A container with no `/iso/random` mount has no entropy, and the
        // canary being zeros is then visible rather than invented.
        let _ = self.random.fill(&mut bytes);
        bytes
    }
}
