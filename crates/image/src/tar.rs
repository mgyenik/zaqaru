//! A tar reader, sized for what a container image is.
//!
//! `docker save` produces a tar of tars: an outer archive holding a manifest
//! and one archive per layer. Both are read with this.
//!
//! Written rather than depended on, for the same reason the wasm emitter is:
//! what an image archive actually contains is a small, fixed set of record
//! types, and the failure mode that matters is a record this does not
//! understand being *skipped* instead of refused. A general-purpose reader
//! has to be permissive about archives nobody will ever hand it; this one
//! can say "the archive uses a record type I do not implement" and stop,
//! which is the only honest answer when the alternative is an image missing
//! files nobody will notice are gone.
//!
//! What it implements: ustar and GNU-format headers, base-256 numeric fields
//! for values octal cannot hold, GNU long-name and long-link records, and
//! PAX extended headers — per-file and global — including the `SCHILY.xattr.*`
//! records that carry extended attributes through an image.

use anyhow::{Result, bail};

const BLOCK: usize = 512;

/// What a record is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Regular,
    HardLink,
    Symlink,
    Character,
    Block,
    Directory,
    Fifo,
}

/// One archive member, with its contents.
///
/// Paths are as the archive wrote them, trailing slash on directories and
/// all. Normalising is the consumer's job: what a path *means* depends on
/// which layer it is in and what came before it, and a reader that decided
/// that would be deciding it without the information.
#[derive(Clone, Debug)]
pub struct Entry {
    pub path: Vec<u8>,
    pub kind: Kind,
    /// For a symlink, its target; for a hardlink, the path it names.
    pub link: Vec<u8>,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    pub device_major: u32,
    pub device_minor: u32,
    pub uname: Vec<u8>,
    pub gname: Vec<u8>,
    /// Sorted, as the index stores them.
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    pub contents: Vec<u8>,
}

/// Reads every member of an archive.
pub fn read(archive: &[u8]) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut at = 0;
    // A PAX global header applies to every following member, and a per-file
    // one to the next member only. GNU long names work the same way.
    let mut global: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut pending: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut long_name: Option<Vec<u8>> = None;
    let mut long_link: Option<Vec<u8>> = None;

    // An archive ends with zero blocks. Reaching the last byte without one
    // means the archive was cut short, and the entries read so far are
    // whatever survived — which must be a refusal, not a shorter image.
    let mut ended = false;
    while at + BLOCK <= archive.len() {
        let header = &archive[at..at + BLOCK];
        if header.iter().all(|byte| *byte == 0) {
            // Two zero blocks end the archive; one is enough to stop at,
            // because nothing follows a header of zeros either way.
            ended = true;
            break;
        }
        verify_checksum(header, at)?;
        at += BLOCK;

        let size = numeric(&header[124..136], "size", at)? as usize;
        let contents_end = at
            .checked_add(size)
            .filter(|end| *end <= archive.len())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "the record at offset {at} declares {size} bytes, past the \
                     end of the archive"
                )
            })?;
        let contents = &archive[at..contents_end];
        at += size.next_multiple_of(BLOCK);

        let flag = header[156];
        match flag {
            // GNU long name and long link: the real value is this record's
            // contents, and it applies to the record that follows.
            b'L' => {
                long_name = Some(trim_nul(contents).to_vec());
                continue;
            }
            b'K' => {
                long_link = Some(trim_nul(contents).to_vec());
                continue;
            }
            b'x' => {
                pending = parse_pax(contents)?;
                continue;
            }
            b'g' => {
                global = parse_pax(contents)?;
                continue;
            }
            _ => {}
        }

        let kind = match flag {
            b'0' | b'\0' | b'7' => Kind::Regular,
            b'1' => Kind::HardLink,
            b'2' => Kind::Symlink,
            b'3' => Kind::Character,
            b'4' => Kind::Block,
            b'5' => Kind::Directory,
            b'6' => Kind::Fifo,
            other => bail!(
                "the archive carries a record of type `{}` at offset {}, which \
                 this reader does not implement — refused rather than skipped, \
                 because a skipped record is a file missing from the image",
                escape(&[other]),
                at
            ),
        };

        let mut entry = Entry {
            path: header_path(header),
            kind,
            link: trim_nul(&header[157..257]).to_vec(),
            mode: numeric(&header[100..108], "mode", at)? as u32,
            uid: numeric(&header[108..116], "uid", at)? as u32,
            gid: numeric(&header[116..124], "gid", at)? as u32,
            mtime_sec: numeric(&header[136..148], "mtime", at)?,
            mtime_nsec: 0,
            device_major: numeric_or_zero(&header[329..337], "devmajor", at)? as u32,
            device_minor: numeric_or_zero(&header[337..345], "devminor", at)? as u32,
            uname: trim_nul(&header[265..297]).to_vec(),
            gname: trim_nul(&header[297..329]).to_vec(),
            xattrs: Vec::new(),
            contents: contents.to_vec(),
        };
        if let Some(name) = long_name.take() {
            entry.path = name;
        }
        if let Some(link) = long_link.take() {
            entry.link = link;
        }
        // A per-file record wins over a global one, and both win over the
        // header — which is the whole point of the extension: the header's
        // fields are too narrow for what they carry.
        apply_pax(&mut entry, &global)?;
        apply_pax(&mut entry, &pending)?;
        pending.clear();
        entry.xattrs.sort();
        entries.push(entry);
    }
    if !ended {
        bail!(
            "the archive ends after {} bytes without an end-of-archive marker, \
             so it was cut short — the {} records read so far are whatever \
             survived, which is not an image",
            archive.len(),
            entries.len()
        );
    }
    if long_name.is_some() || long_link.is_some() || !pending.is_empty() {
        bail!("the archive ends with an extended header describing no record");
    }
    Ok(entries)
}

/// `name`, with `prefix` in front of it where ustar split a long path.
fn header_path(header: &[u8]) -> Vec<u8> {
    let name = trim_nul(&header[0..100]);
    let prefix = trim_nul(&header[345..500]);
    if prefix.is_empty() {
        return name.to_vec();
    }
    let mut path = prefix.to_vec();
    path.push(b'/');
    path.extend_from_slice(name);
    path
}

/// The header checksum, which is the only thing standing between a
/// misaligned read and an archive of plausible nonsense.
fn verify_checksum(header: &[u8], at: usize) -> Result<()> {
    let declared = numeric_or_zero(&header[148..156], "checksum", at)?;
    let mut sum: i64 = 0;
    for (index, byte) in header.iter().enumerate() {
        // The checksum field itself is counted as spaces.
        sum += if (148..156).contains(&index) {
            b' ' as i64
        } else {
            *byte as i64
        };
    }
    // Some historical writers signed the bytes. Both are accepted, which is
    // what every reader does.
    let mut signed: i64 = 0;
    for (index, byte) in header.iter().enumerate() {
        signed += if (148..156).contains(&index) {
            b' ' as i64
        } else {
            *byte as i8 as i64
        };
    }
    if declared != sum && declared != signed {
        bail!(
            "the header at offset {at} has checksum {declared}, and its bytes \
             sum to {sum} — the archive is damaged or this is not one"
        );
    }
    Ok(())
}

/// An octal field, or a base-256 one for values octal cannot hold.
fn numeric(field: &[u8], what: &str, at: usize) -> Result<i64> {
    if field[0] & 0x80 != 0 {
        // GNU base-256: the top bit marks it, the rest is big-endian two's
        // complement. Tar's octal fields cannot hold a size past 8 GiB or a
        // timestamp past 2242, and real archives use this.
        let mut value: i64 = if field[0] & 0x40 != 0 { -1 } else { 0 };
        for byte in &field[1..] {
            value = (value << 8) | (*byte as i64);
        }
        return Ok(value);
    }
    let digits: Vec<u8> = field
        .iter()
        .copied()
        .take_while(|byte| *byte != 0 && *byte != b' ')
        .skip_while(|byte| *byte == b' ')
        .collect();
    if digits.is_empty() {
        bail!("the {what} field of the header at offset {at} is empty");
    }
    let mut value: i64 = 0;
    for byte in digits {
        if !(b'0'..b'8').contains(&byte) {
            bail!(
                "the {what} field of the header at offset {at} contains `{}`, \
                 which is not an octal digit",
                escape(&[byte])
            );
        }
        value = value * 8 + (byte - b'0') as i64;
    }
    Ok(value)
}

/// The same, for a field a writer is allowed to leave blank.
fn numeric_or_zero(field: &[u8], what: &str, at: usize) -> Result<i64> {
    if field.iter().all(|byte| *byte == 0 || *byte == b' ') {
        return Ok(0);
    }
    numeric(field, what, at)
}

/// PAX records: `%d %s=%s\n`, where the number is the whole record's length
/// in bytes, itself included.
fn parse_pax(contents: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut records = Vec::new();
    let mut at = 0;
    while at < contents.len() {
        let space = contents[at..]
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or_else(|| anyhow::anyhow!("a PAX record at offset {at} has no length prefix"))?;
        let length: usize = std::str::from_utf8(&contents[at..at + space])
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "a PAX record at offset {at} has the length `{}`, which is \
                     not a number",
                    escape(&contents[at..at + space])
                )
            })?;
        let end = at.checked_add(length).filter(|end| *end <= contents.len());
        let Some(end) = end else {
            bail!("a PAX record at offset {at} declares {length} bytes, past the end");
        };
        if length <= space + 1 {
            bail!("a PAX record at offset {at} declares {length} bytes, which is too few");
        }
        // The record is `<length> <key>=<value>\n`, and the value may contain
        // anything at all, `=` and newlines included — which is why the
        // length is there.
        let body = &contents[at + space + 1..end - 1];
        let equals = body
            .iter()
            .position(|byte| *byte == b'=')
            .ok_or_else(|| anyhow::anyhow!("a PAX record at offset {at} has no `=`"))?;
        records.push((body[..equals].to_vec(), body[equals + 1..].to_vec()));
        at = end;
    }
    Ok(records)
}

fn apply_pax(entry: &mut Entry, records: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
    for (key, value) in records {
        match key.as_slice() {
            b"path" => entry.path = value.clone(),
            b"linkpath" => entry.link = value.clone(),
            b"uid" => entry.uid = pax_number(key, value)? as u32,
            b"gid" => entry.gid = pax_number(key, value)? as u32,
            b"size" => {
                // The header's size is what was actually read, so a PAX size
                // that disagrees means the archive said one thing and did
                // another.
                let declared = pax_number(key, value)? as usize;
                if declared != entry.contents.len() {
                    bail!(
                        "the PAX header for `{}` declares {declared} bytes and \
                         the record carries {}",
                        escape(&entry.path),
                        entry.contents.len()
                    );
                }
            }
            b"mtime" => {
                let (seconds, nanoseconds) = pax_time(value)?;
                entry.mtime_sec = seconds;
                entry.mtime_nsec = nanoseconds;
            }
            b"uname" => entry.uname = value.clone(),
            b"gname" => entry.gname = value.clone(),
            // `SCHILY.xattr.<name>` is how GNU tar and Docker carry extended
            // attributes, which is where `security.capability` lives — the
            // thing that lets `ping` work without being setuid.
            _ if key.starts_with(b"SCHILY.xattr.") => {
                entry
                    .xattrs
                    .push((key["SCHILY.xattr.".len()..].to_vec(), value.clone()));
            }
            // Records about the archive rather than the file: what produced
            // it, when, and the atime/ctime no image can honour anyway.
            b"comment" | b"charset" | b"atime" | b"ctime" | b"GNU.sparse.major"
            | b"GNU.sparse.minor" => {}
            other if other.starts_with(b"LIBARCHIVE.") || other.starts_with(b"SCHILY.") => {}
            other => bail!(
                "the archive carries the PAX record `{}`, which this reader \
                 does not understand — refused rather than ignored, because \
                 an ignored record is metadata silently dropped from the image",
                escape(other)
            ),
        }
    }
    Ok(())
}

fn pax_number(key: &[u8], value: &[u8]) -> Result<i64> {
    std::str::from_utf8(value)
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "the PAX record `{}` has the value `{}`, which is not a number",
                escape(key),
                escape(value)
            )
        })
}

/// `mtime` is seconds with an optional fractional part, and the fraction is
/// the reason it exists: tar's own field is whole seconds, and CPython
/// compares source timestamps to decide whether a `.pyc` is stale.
fn pax_time(value: &[u8]) -> Result<(i64, u32)> {
    let text =
        std::str::from_utf8(value).map_err(|_| anyhow::anyhow!("a PAX `mtime` is not text"))?;
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (text, ""),
    };
    let seconds: i64 = whole
        .parse()
        .map_err(|_| anyhow::anyhow!("a PAX `mtime` of `{text}` is not a number"))?;
    if fraction.is_empty() {
        return Ok((seconds, 0));
    }
    if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("a PAX `mtime` of `{text}` has a fraction that is not digits");
    }
    // Nine digits of nanoseconds: pad a shorter fraction, truncate a longer.
    let mut digits = fraction.as_bytes().to_vec();
    digits.resize(9, b'0');
    let nanoseconds: u32 = std::str::from_utf8(&digits[..9])
        .expect("ascii digits")
        .parse()
        .expect("nine digits fit a u32");
    Ok((seconds, nanoseconds))
}

fn trim_nul(field: &[u8]) -> &[u8] {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    &field[..end]
}

/// A byte string, readable in a diagnostic whether or not it is text.
fn escape(bytes: &[u8]) -> String {
    let mut text = String::new();
    for byte in bytes {
        if byte.is_ascii_graphic() || *byte == b' ' {
            text.push(*byte as char);
        } else {
            text.push_str(&format!("\\x{byte:02x}"));
        }
    }
    text
}
