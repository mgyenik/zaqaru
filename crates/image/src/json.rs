//! Just enough JSON to read an image manifest.
//!
//! A `docker save` archive's `manifest.json` is a short, machine-written
//! document: an array of objects with a `Layers` array of strings in each.
//! What is needed from it is that one list, in order.
//!
//! Written rather than depended on for the reason the tar reader is: the
//! failure that matters is reading the *wrong* list — a scanner that hunts
//! for `"Layers"` in a byte stream finds it inside a string literal too, and
//! an image built from the wrong layer order is an image that is subtly,
//! silently not the one that was asked for. A parser that refuses what it
//! does not understand cannot do that.

use anyhow::{Result, bail};

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    /// Kept as text. Nothing here needs a number's value, and parsing one
    /// into a float would quietly change what a manifest said.
    Number(String),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    /// The value of a key, for an object.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(fields) => fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(values) => Some(values),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(text) => Some(text),
            _ => None,
        }
    }
}

pub fn parse(bytes: &[u8]) -> Result<Value> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("the document is not UTF-8, so it is not JSON"))?;
    let mut parser = Parser {
        text: text.as_bytes(),
        at: 0,
    };
    parser.skip_whitespace();
    let value = parser.value()?;
    parser.skip_whitespace();
    if parser.at != parser.text.len() {
        bail!(
            "the document has {} bytes of trailing content after the value \
             at offset {}",
            parser.text.len() - parser.at,
            parser.at
        );
    }
    Ok(value)
}

struct Parser<'a> {
    text: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn value(&mut self) -> Result<Value> {
        match self.peek()? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => Ok(Value::String(self.string()?)),
            b't' => self.literal("true", Value::Bool(true)),
            b'f' => self.literal("false", Value::Bool(false)),
            b'n' => self.literal("null", Value::Null),
            byte if byte == b'-' || byte.is_ascii_digit() => self.number(),
            byte => bail!(
                "the byte `{}` at offset {} does not begin a JSON value",
                byte as char,
                self.at
            ),
        }
    }

    fn object(&mut self) -> Result<Value> {
        self.at += 1; // `{`
        let mut fields = Vec::new();
        self.skip_whitespace();
        if self.peek()? == b'}' {
            self.at += 1;
            return Ok(Value::Object(fields));
        }
        loop {
            self.skip_whitespace();
            if self.peek()? != b'"' {
                bail!("an object key at offset {} is not a string", self.at);
            }
            let key = self.string()?;
            self.skip_whitespace();
            if self.peek()? != b':' {
                bail!("the key `{key}` at offset {} has no `:` after it", self.at);
            }
            self.at += 1;
            self.skip_whitespace();
            let value = self.value()?;
            fields.push((key, value));
            self.skip_whitespace();
            match self.peek()? {
                b',' => self.at += 1,
                b'}' => {
                    self.at += 1;
                    return Ok(Value::Object(fields));
                }
                byte => bail!(
                    "the byte `{}` at offset {} is neither `,` nor `}}`",
                    byte as char,
                    self.at
                ),
            }
        }
    }

    fn array(&mut self) -> Result<Value> {
        self.at += 1; // `[`
        let mut values = Vec::new();
        self.skip_whitespace();
        if self.peek()? == b']' {
            self.at += 1;
            return Ok(Value::Array(values));
        }
        loop {
            self.skip_whitespace();
            values.push(self.value()?);
            self.skip_whitespace();
            match self.peek()? {
                b',' => self.at += 1,
                b']' => {
                    self.at += 1;
                    return Ok(Value::Array(values));
                }
                byte => bail!(
                    "the byte `{}` at offset {} is neither `,` nor `]`",
                    byte as char,
                    self.at
                ),
            }
        }
    }

    fn string(&mut self) -> Result<String> {
        self.at += 1; // `"`
        let mut text = String::new();
        loop {
            let byte = self.next()?;
            match byte {
                b'"' => return Ok(text),
                b'\\' => {
                    let escape = self.next()?;
                    match escape {
                        b'"' => text.push('"'),
                        b'\\' => text.push('\\'),
                        b'/' => text.push('/'),
                        b'b' => text.push('\u{8}'),
                        b'f' => text.push('\u{c}'),
                        b'n' => text.push('\n'),
                        b'r' => text.push('\r'),
                        b't' => text.push('\t'),
                        b'u' => text.push(self.unicode_escape()?),
                        other => bail!(
                            "`\\{}` at offset {} is not an escape",
                            other as char,
                            self.at
                        ),
                    }
                }
                // A raw control character is not legal in a JSON string, and
                // accepting one would accept a document nothing else does.
                byte if byte < 0x20 => {
                    bail!("a raw control byte {byte:#04x} at offset {}", self.at)
                }
                byte => {
                    // The input is known UTF-8, so a multi-byte sequence is
                    // copied through byte by byte.
                    text.push(byte as char);
                    if byte >= 0x80 {
                        // Re-decode: pushing the lead byte as a `char` above
                        // was wrong for a multi-byte sequence, so undo it and
                        // take the whole sequence.
                        text.pop();
                        let start = self.at - 1;
                        let length = utf8_length(byte);
                        if start + length > self.text.len() {
                            bail!("a truncated UTF-8 sequence at offset {start}");
                        }
                        let sequence = std::str::from_utf8(&self.text[start..start + length])
                            .map_err(|_| {
                                anyhow::anyhow!("an invalid UTF-8 sequence at offset {start}")
                            })?;
                        text.push_str(sequence);
                        self.at = start + length;
                    }
                }
            }
        }
    }

    fn unicode_escape(&mut self) -> Result<char> {
        let start = self.at;
        if start + 4 > self.text.len() {
            bail!("a truncated `\\u` escape at offset {start}");
        }
        let digits = std::str::from_utf8(&self.text[start..start + 4])
            .map_err(|_| anyhow::anyhow!("a `\\u` escape at offset {start} is not text"))?;
        let value = u32::from_str_radix(digits, 16)
            .map_err(|_| anyhow::anyhow!("`\\u{digits}` at offset {start} is not hexadecimal"))?;
        self.at = start + 4;
        // Surrogate pairs: a manifest has no use for one, and decoding half
        // of a pair into a replacement character would silently change a
        // digest string.
        char::from_u32(value).ok_or_else(|| {
            anyhow::anyhow!(
                "`\\u{digits}` at offset {start} is not a character — a surrogate \
                 pair is not supported here"
            )
        })
    }

    fn number(&mut self) -> Result<Value> {
        let start = self.at;
        if self.peek()? == b'-' {
            self.at += 1;
        }
        while self.at < self.text.len()
            && (self.text[self.at].is_ascii_digit()
                || matches!(self.text[self.at], b'.' | b'e' | b'E' | b'+' | b'-'))
        {
            self.at += 1;
        }
        if self.at == start {
            bail!("a number at offset {start} has no digits");
        }
        Ok(Value::Number(
            std::str::from_utf8(&self.text[start..self.at])
                .expect("ascii")
                .to_string(),
        ))
    }

    fn literal(&mut self, word: &str, value: Value) -> Result<Value> {
        if self.text[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            return Ok(value);
        }
        bail!("the bytes at offset {} are not `{word}`", self.at)
    }

    fn skip_whitespace(&mut self) {
        while self.at < self.text.len()
            && matches!(self.text[self.at], b' ' | b'\t' | b'\n' | b'\r')
        {
            self.at += 1;
        }
    }

    fn peek(&self) -> Result<u8> {
        self.text
            .get(self.at)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("the document ends where a value was expected"))
    }

    fn next(&mut self) -> Result<u8> {
        let byte = self.peek()?;
        self.at += 1;
        Ok(byte)
    }
}

fn utf8_length(lead: u8) -> usize {
    match lead {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}
