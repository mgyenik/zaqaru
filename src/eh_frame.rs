//! Function extents recovered from `.eh_frame`.
//!
//! The symbol table is the obvious place to ask where the functions are, and
//! for an object a compiler just emitted it answers completely. A linked
//! executable is a weaker witness: hand-written assembly often carries no
//! `.size`, static functions may have been folded or renamed, and a stripped
//! binary has no `.symtab` at all. What it does have, because C is compiled
//! with asynchronous unwind tables by default, is one frame description
//! entry per function — each of which states exactly the two things function
//! discovery needs, where the function starts and how long it is.
//!
//! So this reads `.eh_frame` for its FDE headers and stops. The unwind
//! instructions in the body are for unwinding, which the translated module
//! does its own way; only the extents matter here.

use anyhow::{Result, bail};

/// One function's extent, as an unwind table describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Virtual address of the first instruction.
    pub address: u64,
    pub length: u64,
}

/// Every extent described by a `.eh_frame` section placed at `at`.
///
/// Entries whose pointer encoding cannot be followed to an address — an
/// indirect encoding, which names a slot the dynamic loader fills — are
/// refused rather than guessed at, because a wrong extent silently
/// translates the wrong bytes.
pub fn frames(bytes: &[u8], at: u64) -> Result<Vec<Frame>> {
    let mut frames = Vec::new();
    let mut encodings: std::collections::HashMap<usize, u8> = std::collections::HashMap::new();
    let mut offset = 0usize;

    while offset < bytes.len() {
        let start = offset;
        let mut cursor = Cursor::new(bytes, offset);
        let (length, is_extended) = {
            let first = cursor.u32()?;
            if first == 0xffff_ffff {
                (cursor.u64()?, true)
            } else {
                (u64::from(first), false)
            }
        };
        // A zero length is the terminator some linkers append.
        if length == 0 {
            break;
        }
        let body = cursor.offset;
        let end = body
            .checked_add(usize::try_from(length).unwrap_or(usize::MAX))
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| anyhow::anyhow!("an .eh_frame entry at {start:#x} runs off the end"))?;
        offset = end;

        // In `.eh_frame` (unlike `.debug_frame`) a zero here means the entry
        // is the CIE the following entries point back to.
        let identifier = if is_extended {
            cursor.u64()?
        } else {
            u64::from(cursor.u32()?)
        };
        if identifier == 0 {
            encodings.insert(start, read_cie(&mut cursor, end)?);
            continue;
        }

        // Otherwise it is a distance backwards, from this very field, to the
        // CIE that describes how the entry is encoded.
        let cie = (body as u64)
            .checked_sub(identifier)
            .ok_or_else(|| anyhow::anyhow!("an FDE at {start:#x} points before .eh_frame"))?;
        let Some(&encoding) = encodings.get(&(cie as usize)) else {
            bail!("an FDE at {start:#x} names a CIE at {cie:#x} that is not one");
        };

        let address = cursor.pointer(encoding, at)?;
        // The length uses the same format as the address but never its
        // application: it is a count of bytes, not a place.
        let length = cursor.pointer(encoding & 0x0f, at)?;
        if length > 0 {
            frames.push(Frame { address, length });
        }
    }

    frames.sort_by_key(|frame| frame.address);
    frames.dedup();
    Ok(frames)
}

/// Reads a CIE far enough to learn how the FDEs that name it encode their
/// addresses, and returns that encoding.
fn read_cie(cursor: &mut Cursor<'_>, end: usize) -> Result<u8> {
    let version = cursor.u8()?;
    if version != 1 && version != 3 && version != 4 {
        bail!("an .eh_frame CIE of version {version}, which this does not read");
    }
    let augmentation = cursor.string()?;
    if augmentation.first() == Some(&b'e') {
        bail!("an .eh_frame CIE with the obsolete `eh` augmentation");
    }
    if version == 4 {
        let _address_size = cursor.u8()?;
        let _segment_size = cursor.u8()?;
    }
    let _code_alignment = cursor.uleb()?;
    let _data_alignment = cursor.sleb()?;
    if version == 1 {
        cursor.u8()?;
    } else {
        cursor.uleb()?;
    }

    // Without a `z` there is no augmentation data, so there is no `R`, and
    // the FDE's addresses are plain absolute pointers.
    if augmentation.first() != Some(&b'z') {
        return Ok(0x00);
    }
    let data = cursor.uleb()? as usize;
    let data_end = cursor.offset + data;
    if data_end > end {
        bail!("an .eh_frame CIE's augmentation data runs past the entry");
    }
    let mut encoding = 0x00;
    for letter in &augmentation[1..] {
        match letter {
            b'R' => encoding = cursor.u8()?,
            b'L' => {
                cursor.u8()?;
            }
            b'P' => {
                let personality = cursor.u8()?;
                cursor.skip_pointer(personality)?;
            }
            b'S' | b'B' | b'G' => {}
            other => bail!(
                "an .eh_frame CIE augmented with `{}`, which this does not read",
                *other as char
            ),
        }
    }
    cursor.offset = data_end;
    Ok(encoding)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8], offset: usize) -> Self {
        Self { bytes, offset }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| anyhow::anyhow!(".eh_frame ends in the middle of an entry"))?;
        let taken = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(taken)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    /// A nul-terminated string, without its terminator.
    fn string(&mut self) -> Result<&'a [u8]> {
        let start = self.offset;
        while self.u8()? != 0 {}
        Ok(&self.bytes[start..self.offset - 1])
    }

    fn uleb(&mut self) -> Result<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = self.u8()?;
            if shift < 64 {
                value |= u64::from(byte & 0x7f) << shift;
            }
            shift += 7;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            if shift > 70 {
                bail!("an unterminated LEB128 in .eh_frame");
            }
        }
    }

    fn sleb(&mut self) -> Result<i64> {
        let mut value = 0i64;
        let mut shift = 0u32;
        loop {
            let byte = self.u8()?;
            if shift < 64 {
                value |= i64::from(byte & 0x7f) << shift;
            }
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    value |= -1i64 << shift;
                }
                return Ok(value);
            }
            if shift > 70 {
                bail!("an unterminated LEB128 in .eh_frame");
            }
        }
    }

    /// Reads a pointer in one of the DWARF exception-handling encodings, and
    /// applies whatever it is relative to.
    ///
    /// `at` is where the section itself was placed, so that the position of
    /// the field being read is a virtual address — which is what a
    /// program-counter-relative encoding is measured from.
    fn pointer(&mut self, encoding: u8, at: u64) -> Result<u64> {
        const OMIT: u8 = 0xff;
        if encoding == OMIT {
            return Ok(0);
        }
        if encoding & 0x80 != 0 {
            bail!(
                "an .eh_frame pointer encoded indirectly ({encoding:#04x}), which names a \
                 slot rather than an address"
            );
        }
        let here = at + self.offset as u64;
        let value = match encoding & 0x0f {
            0x00 => self.u64()?,
            0x01 => self.uleb()?,
            0x02 => u64::from(self.u16()?),
            0x03 => u64::from(self.u32()?),
            0x04 => self.u64()?,
            0x09 => self.sleb()? as u64,
            0x0a => i64::from(self.u16()? as i16) as u64,
            0x0b => i64::from(self.u32()? as i32) as u64,
            0x0c => self.u64()?,
            other => bail!("an .eh_frame pointer in format {other:#x}, which this does not read"),
        };
        Ok(match encoding & 0x70 {
            // Absolute, and the two applications that are relative to a place
            // this reader knows. `textrel`, `datarel` and `funcrel` are
            // measured from bases only the producer knows, and no toolchain
            // emits them for an FDE's own addresses.
            0x00 => value,
            0x10 => here.wrapping_add(value),
            other => bail!(
                "an .eh_frame pointer relative to base {:#x}, which this does not know",
                other
            ),
        })
    }

    /// Steps over a pointer without needing to know where it points, which
    /// is all a personality routine's address is wanted for here.
    fn skip_pointer(&mut self, encoding: u8) -> Result<()> {
        if encoding == 0xff {
            return Ok(());
        }
        match encoding & 0x0f {
            0x00 | 0x04 | 0x0c => self.take(8).map(|_| ()),
            0x01 => self.uleb().map(|_| ()),
            0x09 => self.sleb().map(|_| ()),
            0x02 | 0x0a => self.take(2).map(|_| ()),
            0x03 | 0x0b => self.take(4).map(|_| ()),
            other => bail!("an .eh_frame pointer in format {other:#x}, which this does not read"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds one `.eh_frame` entry: a length, a body, and nothing else.
    fn entry(identifier: u32, body: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let length = 4 + body.len() as u32;
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.extend_from_slice(&identifier.to_le_bytes());
        bytes.extend_from_slice(body);
        bytes
    }

    /// The shape `gcc` and `clang` emit for C: version 1, augmented `zR`,
    /// with addresses stored as signed four-byte differences from the field
    /// that holds them.
    fn common_cie() -> Vec<u8> {
        let mut body = Vec::new();
        body.push(1); // version
        body.extend_from_slice(b"zR\0");
        body.push(1); // code alignment
        body.push(0x78); // data alignment, -8 as a signed LEB128
        body.push(16); // return address register
        body.push(1); // one byte of augmentation data
        body.push(0x1b); // pcrel | sdata4
        entry(0, &body)
    }

    fn fde(from_cie: u32, pc_offset: i32, length: u32) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&pc_offset.to_le_bytes());
        body.extend_from_slice(&length.to_le_bytes());
        body.push(0); // no augmentation data
        entry(from_cie, &body)
    }

    #[test]
    fn a_frame_description_says_where_a_function_is() {
        let cie = common_cie();
        // The FDE's pointer field sits eight bytes into the FDE, which
        // itself follows the CIE — so a difference of -8 from there names
        // the address the section was placed at.
        let mut bytes = cie.clone();
        let fde_start = bytes.len() as i32;
        bytes.extend_from_slice(&fde(bytes.len() as u32 + 4, -(fde_start + 8), 0x40));

        let frames = frames(&bytes, 0x1000).expect("read");
        assert_eq!(
            frames,
            vec![Frame {
                address: 0x1000,
                length: 0x40
            }]
        );
    }

    #[test]
    fn a_terminator_ends_the_section() {
        let mut bytes = common_cie();
        let fde_start = bytes.len() as i32;
        bytes.extend_from_slice(&fde(bytes.len() as u32 + 4, -(fde_start + 8), 0x10));
        bytes.extend_from_slice(&0u32.to_le_bytes());
        // Anything after the terminator is not an entry, and reading it as
        // one would produce a function out of padding.
        bytes.extend_from_slice(&[0xcc; 16]);

        assert_eq!(frames(&bytes, 0x2000).expect("read").len(), 1);
    }

    #[test]
    fn a_personality_routine_is_stepped_over_rather_than_read() {
        // `zPLR`, which is what a translation unit with a landing pad gets.
        // The `P` carries an encoding byte and a pointer of that width; if
        // the reader failed to skip exactly that much, the `R` encoding it
        // read next would be a byte of the personality's address.
        let mut body = Vec::new();
        body.push(1);
        body.extend_from_slice(b"zPLR\0");
        body.push(1);
        body.push(0x78);
        body.push(16);
        body.push(1 + 4 + 1 + 1); // augmentation data length
        body.push(0x03); // personality encoding: udata4
        body.extend_from_slice(&0xdead_beefu32.to_le_bytes());
        body.push(0x1b); // lsda encoding
        body.push(0x1b); // fde encoding: pcrel | sdata4
        let cie = entry(0, &body);

        let mut bytes = cie.clone();
        let fde_start = bytes.len() as i32;
        bytes.extend_from_slice(&fde(bytes.len() as u32 + 4, -(fde_start + 8) + 0x20, 0x8));

        assert_eq!(
            frames(&bytes, 0x3000).expect("read"),
            vec![Frame {
                address: 0x3020,
                length: 8
            }]
        );
    }

    #[test]
    fn an_unaugmented_cie_means_absolute_addresses() {
        let mut body = Vec::new();
        body.push(1);
        body.extend_from_slice(b"\0"); // no augmentation at all
        body.push(1);
        body.push(0x78);
        body.push(16);
        let cie = entry(0, &body);

        let mut fde_body = Vec::new();
        fde_body.extend_from_slice(&0x4010u64.to_le_bytes());
        fde_body.extend_from_slice(&0x30u64.to_le_bytes());
        let mut bytes = cie.clone();
        bytes.extend_from_slice(&entry(bytes.len() as u32 + 4, &fde_body));

        assert_eq!(
            frames(&bytes, 0x4000).expect("read"),
            vec![Frame {
                address: 0x4010,
                length: 0x30
            }]
        );
    }

    #[test]
    fn an_indirect_pointer_is_refused_rather_than_guessed_at() {
        let mut body = Vec::new();
        body.push(1);
        body.extend_from_slice(b"zR\0");
        body.push(1);
        body.push(0x78);
        body.push(16);
        body.push(1);
        body.push(0x9b); // indirect | pcrel | sdata4
        let cie = entry(0, &body);

        let mut bytes = cie.clone();
        bytes.extend_from_slice(&fde(bytes.len() as u32 + 4, 0, 0x10));

        let error = frames(&bytes, 0x5000).expect_err("an address that is not one");
        assert!(
            format!("{error:#}").contains("indirectly"),
            "the refusal does not say what was wrong: {error:#}"
        );
    }

    #[test]
    fn an_entry_that_runs_off_the_end_is_refused() {
        let mut bytes = common_cie();
        // A length far past what follows it.
        bytes.extend_from_slice(&0xfff0u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 8]);
        assert!(frames(&bytes, 0).is_err());
    }
}
