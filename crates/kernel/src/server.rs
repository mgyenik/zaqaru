//! The container as a store: the isotope Server Protocol, served by the
//! kernel.
//!
//! A Block has two faces. Inside it is a StructFS client, reading and
//! writing paths under `/iso`; outside it is a StructFS store that others
//! read and write. The runtime turns an outside read into a Request the
//! Block reads from `/iso/server/requests/pending`, and the Block answers by
//! writing a Response to the Request's `respond_to` path. This module is
//! the container's outside face: the paths it serves describe the machine
//! — every process and thread, a thread's registers, a process's memory
//! map and descriptors, the block cache, what the run has cost — and are
//! what a debugger reads.
//!
//! **Serving a Request never changes anything the guest can observe.** It
//! reads kernel state and writes a Response, and that is the whole of it.
//! The rule is what lets the host keep `/iso/server` outside the tape: the
//! answers to `requests/pending` are not inputs to the run, so a recording
//! does not keep them and a replay does not check them, and a debugger can
//! ask questions during a replay without the run diverging.
//!
//! Values are JSON, written and parsed by the few dozen lines below rather
//! than a library the module would otherwise never need.

use core::fmt::Write as _;

use crate::abi::{Store, StoreOutcome};
use crate::paths;

use super::System;

/// What the container's store serves, as the manifest and
/// `/iso/self/interface` declare it. The disassembly path is declared only
/// when it is compiled in.
macro_rules! interface {
    ($disassembly:literal) => {
        concat!(
            r#"{"name":"zaqaru-container","version":"0.1.0","serialization":"application/json","paths":{"statistics":{"read":"instructions retired and accelerated, blocks decoded, the current pid"},"processes":{"read":"every process and its threads, with what each is parked on"},"processes/{pid}/threads/{tid}/registers":{"read":"the general registers, rip, the segment base and the flags of one thread"},"processes/{pid}/maps":{"read":"the process's memory map, as /proc/self/maps renders it"},"processes/{pid}/descriptors":{"read":"the process's open descriptors"},"processes/{pid}/memory/{address}/{length}":{"read":"up to 4096 bytes of the running process's memory, hex"},"cache":{"read":"the running process's block cache"},"meta/":{"read":"which paths are readable"}"#,
            $disassembly,
            "}}"
        )
    };
}
#[cfg(feature = "disassembly")]
pub const INTERFACE: &str = interface!(
    r#","processes/{pid}/threads/{tid}/disassembly":{"read":"the instructions from the thread's rip, as text"}"#
);
#[cfg(not(feature = "disassembly"))]
pub const INTERFACE: &str = interface!("");

/// The most memory one read hands back.
const MEMORY_READ_CAP: u64 = 4096;

/// One Request, as the runtime queued it.
struct Request {
    op: String,
    path: String,
    respond_to: String,
}

impl<'a, S: Store + Clone> System<'a, S> {
    /// Answers every Request the host has queued.
    ///
    /// Called where the kernel already talks to the host between turns —
    /// once a slice, and whenever a run returns to the host — so a debugger
    /// holding the machine at an instant reads that instant.
    pub fn serve(&mut self) {
        let mut pending = Vec::new();
        let asked = self
            .current()
            .kernel
            .store
            .read(paths::SERVER_PENDING, &mut pending);
        if asked != StoreOutcome::Present || pending.is_empty() {
            return;
        }
        let requests = match parse_requests(&pending) {
            Some(requests) => requests,
            None => {
                let message = "kernel: the host's request batch is not the JSON the Server Protocol specifies";
                crate::report_to(&mut self.current().kernel, message);
                return;
            }
        };
        for request in requests {
            let response = match request.op.as_str() {
                "read" => match self.read(&request.path) {
                    Ok(value) => format!(r#"{{"result":"ok","value":{value}}}"#),
                    Err(Refusal::NotFound) => error("not_found", &format!("the container serves no {}", request.path)),
                    Err(Refusal::Unavailable(why)) => error("unavailable", &why),
                },
                "write" => error("not_writable", "the container's store is read-only"),
                other => error("invalid_path", &format!("unknown operation {other}")),
            };
            let path = segments(&request.respond_to);
            let path: Vec<&[u8]> = path.iter().map(Vec::as_slice).collect();
            let _ = self.current().kernel.store.write(&path, response.as_bytes());
        }
    }

    /// The value at a path of the container's store, as JSON.
    fn read(&mut self, path: &str) -> Result<String, Refusal> {
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        match parts.as_slice() {
            ["statistics"] => Ok(self.statistics()),
            ["processes"] => Ok(self.processes()),
            ["processes", pid, "maps"] => {
                let index = self.container_index(pid.parse().ok().ok_or(Refusal::NotFound)?).ok_or(Refusal::NotFound)?;
                let maps = self.containers[index].process.kernel.render_maps();
                Ok(quoted(&maps))
            }
            ["processes", pid, "descriptors"] => {
                let index = self.container_index(pid.parse().ok().ok_or(Refusal::NotFound)?).ok_or(Refusal::NotFound)?;
                Ok(self.descriptors(index))
            }
            ["processes", pid, "threads", tid, "registers"] => {
                let (index, position) = self.thread_index(pid, tid)?;
                Ok(registers(&self.containers[index].process.kernel.machine.threads.all()[position].tcb))
            }
            ["processes", pid, "threads", tid, "disassembly"] => {
                let (index, position) = self.thread_index(pid, tid)?;
                self.in_place(index)?;
                let rip = self.containers[index].process.kernel.machine.threads.all()[position].tcb.rip;
                Ok(disassembly(&self.containers[index].process.kernel.pages, rip))
            }
            ["processes", pid, "memory", address, length] => {
                let index = self.container_index(pid.parse().ok().ok_or(Refusal::NotFound)?).ok_or(Refusal::NotFound)?;
                self.in_place(index)?;
                let address = number(address).ok_or(Refusal::NotFound)?;
                let length = number(length).ok_or(Refusal::NotFound)?.min(MEMORY_READ_CAP);
                Ok(memory(&self.containers[index].process.kernel.pages, address, length))
            }
            ["cache"] => {
                let cache = &self.current().cache;
                Ok(format!(
                    r#"{{"decoded":{},"flushes":{},"live":{},"accelerates":{}}}"#,
                    cache.decoded,
                    cache.flushes,
                    cache.len(),
                    cache.accelerates()
                ))
            }
            ["meta"] | ["meta", ..] => Ok(String::from(
                r#"{"paths":{"statistics":{"readable":true,"writable":false},"processes":{"readable":true,"writable":false},"processes/{pid}/threads/{tid}/registers":{"readable":true,"writable":false},"processes/{pid}/threads/{tid}/disassembly":{"readable":true,"writable":false},"processes/{pid}/maps":{"readable":true,"writable":false},"processes/{pid}/descriptors":{"readable":true,"writable":false},"processes/{pid}/memory/{address}/{length}":{"readable":true,"writable":false},"cache":{"readable":true,"writable":false}}}"#,
            )),
            _ => Err(Refusal::NotFound),
        }
    }

    /// Which container and which of its threads a path names.
    fn thread_index(&self, pid: &str, tid: &str) -> Result<(usize, usize), Refusal> {
        let index = self.container_index(pid.parse().ok().ok_or(Refusal::NotFound)?).ok_or(Refusal::NotFound)?;
        let tid: i32 = tid.parse().ok().ok_or(Refusal::NotFound)?;
        let position = self.containers[index]
            .process
            .kernel
            .machine
            .threads
            .all()
            .iter()
            .position(|thread| thread.tid == tid)
            .ok_or(Refusal::NotFound)?;
        Ok((index, position))
    }

    /// Whether a process's memory is the memory in place. Every process
    /// maps the same range, and only the running one's bytes are at those
    /// addresses — see `resident` — so the store reads memory only for the
    /// process whose turn it is, and says so otherwise.
    fn in_place(&self, index: usize) -> Result<(), Refusal> {
        if self.containers[index].pid == self.current_pid() {
            Ok(())
        } else {
            Err(Refusal::Unavailable(format!(
                "pid {} is not running; only the running process's memory ({}) is in place",
                self.containers[index].pid,
                self.current_pid()
            )))
        }
    }

    fn container_index(&self, pid: i32) -> Option<usize> {
        self.containers.iter().position(|container| container.pid == pid)
    }

    /// What the run has cost so far, and whose turn it is.
    fn statistics(&self) -> String {
        format!(
            r#"{{"retired":{},"accelerated":{},"decoded":{},"current":{}}}"#,
            self.retired(),
            self.accelerated(),
            self.decoded(),
            self.current_pid()
        )
    }

    /// Every process and every thread: the stall report, structured.
    fn processes(&self) -> String {
        let mut out = String::from(r#"{"current":"#);
        let _ = write!(out, "{},\"processes\":[", self.current_pid());
        for (index, container) in self.containers.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                r#"{{"pid":{},"parent":{},"state":{},"threads":["#,
                container.pid,
                container.parent,
                match container.status {
                    Some(super::Ending::Exited(code)) => format!(r#"{{"exited":{code}}}"#),
                    Some(super::Ending::Signalled(signal)) => format!(r#"{{"signalled":{signal}}}"#),
                    None => String::from(r#""live""#),
                }
            );
            for (position, thread) in container.process.kernel.machine.threads.all().iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                let _ = write!(
                    out,
                    r#"{{"tid":{},"rip":"{:#x}","retired":{},"state":{}}}"#,
                    thread.tid,
                    thread.tcb.rip,
                    thread.tcb.retired,
                    quoted(&self.describe_state(container, thread))
                );
            }
            out.push_str("]}");
        }
        out.push_str("]}");
        out
    }

    fn descriptors(&self, index: usize) -> String {
        let files = &self.containers[index].process.kernel.files;
        let mut out = String::from("[");
        for (position, (fd, what)) in files.open_descriptors().enumerate() {
            if position > 0 {
                out.push(',');
            }
            let (offset, flags, cloexec) = match files.description(fd) {
                Ok(open) => (open.offset, open.flags, files.close_on_exec(fd).unwrap_or(false)),
                Err(_) => (0, 0, false),
            };
            let _ = write!(
                out,
                r#"{{"fd":{fd},"what":{},"offset":{offset},"flags":{flags},"cloexec":{cloexec}}}"#,
                quoted(&what)
            );
        }
        out.push(']');
        out
    }
}

/// A thread's registers. Sixty-four-bit values are hex strings, because
/// a JSON number is a double and would lose the high bits.
fn registers(tcb: &cpu::state::Tcb) -> String {
    const NAMES: [&str; 16] = [
        "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12",
        "r13", "r14", "r15",
    ];
    let mut out = String::from("{");
    for (name, value) in NAMES.iter().zip(tcb.registers.iter()) {
        let _ = write!(out, r#""{name}":"{value:#x}","#);
    }
    let _ = write!(
        out,
        r#""rip":"{:#x}","fs_base":"{:#x}","flags":"{:#x}","flags_stale":{},"retired":{}}}"#,
        tcb.rip,
        tcb.fs_base,
        tcb.flags.materialized(),
        match tcb.flags_staleness {
            cpu::state::Staleness::Unknown => "null",
            cpu::state::Staleness::Fresh => "false",
            cpu::state::Staleness::Stale => "true",
        },
        tcb.retired
    );
    out
}

/// Why a read was not answered with a value.
enum Refusal {
    /// The store serves nothing at the path.
    NotFound,
    /// The path exists, but not now, and this is why.
    Unavailable(String),
}

/// A number in a path: hex with `0x`, or decimal.
fn number(text: &str) -> Option<u64> {
    match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

/// The instructions from `rip`, as the disassembly feature renders them:
/// an array of `{address, bytes, text}`; empty when the feature is not
/// compiled in, which the manifest also says.
fn disassembly(pages: &cpu::space::Space, rip: u64) -> String {
    #[cfg(feature = "disassembly")]
    {
        let mut out = String::from("[");
        for (position, line) in cpu::disassembly::disassemble(pages, rip, 40).iter().enumerate() {
            if position > 0 {
                out.push(',');
            }
            let _ = write!(out, r#"{{"address":"{:#x}","bytes":"{}","text":{}}}"#, line.address, hex(&line.bytes), quoted(&line.text));
        }
        out.push(']');
        out
    }
    #[cfg(not(feature = "disassembly"))]
    {
        let _ = (pages, rip);
        String::from("[]")
    }
}

/// `length` bytes at `address` in the running process's memory, as far as
/// they are readable: `{address, bytes}` with the bytes in hex, stopping at
/// the first page the process may not read.
fn memory(pages: &cpu::space::Space, address: u64, length: u64) -> String {
    const PAGE: u64 = 4096;
    let mut held = Vec::with_capacity(length as usize);
    let mut at = address;
    let end = address.saturating_add(length);
    while at < end {
        let page_end = ((at / PAGE) + 1) * PAGE;
        let take = page_end.min(end) - at;
        let mut chunk = vec![0u8; take as usize];
        if pages.read(at, &mut chunk).is_err() {
            break;
        }
        held.extend_from_slice(&chunk);
        at += take;
    }
    format!(r#"{{"address":"{address:#x}","bytes":"{}"}}"#, hex(&held))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn error(kind: &str, message: &str) -> String {
    format!(
        r#"{{"result":"error","error":{{"type":"{kind}","message":{},"retryable":false}}}}"#,
        quoted(message)
    )
}

/// A path string as store segments: `/iso/server/responses/3` is
/// `["iso", "server", "responses", "3"]`.
fn segments(path: &str) -> Vec<Vec<u8>> {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.as_bytes().to_vec())
        .collect()
}

/// A JSON string literal, escaped.
pub fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Parses the batch `requests/pending` answers: a JSON array of objects
/// with string `op`, `path` and `respond_to` fields. Any other field —
/// `data`, which a read carries as `null` — is skipped whatever its shape.
fn parse_requests(bytes: &[u8]) -> Option<Vec<Request>> {
    let text = core::str::from_utf8(bytes).ok()?;
    let mut parser = Parser { text, at: 0 };
    parser.skip_space();
    parser.expect('[')?;
    let mut requests = Vec::new();
    parser.skip_space();
    if parser.peek() == Some(']') {
        return Some(requests);
    }
    loop {
        parser.skip_space();
        parser.expect('{')?;
        let (mut op, mut path, mut respond_to) = (None, None, None);
        loop {
            parser.skip_space();
            if parser.peek() == Some('}') {
                parser.at += 1;
                break;
            }
            let key = parser.string()?;
            parser.skip_space();
            parser.expect(':')?;
            parser.skip_space();
            match key.as_str() {
                "op" => op = Some(parser.string()?),
                "path" => path = Some(parser.string()?),
                "respond_to" => respond_to = Some(parser.string()?),
                _ => parser.skip_value()?,
            }
            parser.skip_space();
            if parser.peek() == Some(',') {
                parser.at += 1;
            }
        }
        requests.push(Request {
            op: op?,
            path: path?,
            respond_to: respond_to?,
        });
        parser.skip_space();
        match parser.peek()? {
            ',' => parser.at += 1,
            ']' => return Some(requests),
            _ => return None,
        }
    }
}

struct Parser<'t> {
    text: &'t str,
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.text[self.at..].chars().next()
    }

    fn skip_space(&mut self) {
        while let Some(c) = self.peek() {
            if !c.is_whitespace() {
                break;
            }
            self.at += c.len_utf8();
        }
    }

    fn expect(&mut self, wanted: char) -> Option<()> {
        (self.peek()? == wanted).then(|| self.at += wanted.len_utf8())
    }

    fn string(&mut self) -> Option<String> {
        self.expect('"')?;
        let mut out = String::new();
        loop {
            let c = self.peek()?;
            self.at += c.len_utf8();
            match c {
                '"' => return Some(out),
                '\\' => {
                    let escaped = self.peek()?;
                    self.at += escaped.len_utf8();
                    match escaped {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'u' => {
                            let hex = self.text.get(self.at..self.at + 4)?;
                            self.at += 4;
                            out.push(char::from_u32(u32::from_str_radix(hex, 16).ok()?)?);
                        }
                        other => out.push(other),
                    }
                }
                other => out.push(other),
            }
        }
    }

    /// Skips one value of any shape.
    fn skip_value(&mut self) -> Option<()> {
        self.skip_space();
        match self.peek()? {
            '"' => self.string().map(|_| ()),
            '{' | '[' => {
                let close = if self.peek()? == '{' { '}' } else { ']' };
                self.at += 1;
                loop {
                    self.skip_space();
                    match self.peek()? {
                        c if c == close => {
                            self.at += 1;
                            return Some(());
                        }
                        ',' | ':' => self.at += 1,
                        _ => self.skip_value()?,
                    }
                }
            }
            _ => {
                while let Some(c) = self.peek() {
                    if c == ',' || c == '}' || c == ']' || c.is_whitespace() {
                        break;
                    }
                    self.at += c.len_utf8();
                }
                Some(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_of_requests_parses_and_other_fields_are_skipped() {
        let requests = parse_requests(
            br#"[{"op":"read","path":"processes","data":null,"respond_to":"/iso/server/responses/1"},
                {"op": "write", "path": "x/y", "data": {"a": [1, 2, {"b": "c"}]}, "respond_to": "/iso/server/responses/2"}]"#,
        )
        .expect("parses");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].op, "read");
        assert_eq!(requests[0].path, "processes");
        assert_eq!(requests[1].respond_to, "/iso/server/responses/2");
        assert_eq!(segments(&requests[1].respond_to), vec![b"iso".to_vec(), b"server".to_vec(), b"responses".to_vec(), b"2".to_vec()]);
        assert!(parse_requests(b"[]").expect("empty").is_empty());
        assert!(parse_requests(b"not json").is_none());
    }

    #[test]
    fn strings_are_escaped_for_json() {
        assert_eq!(quoted("a\"b\\c\nd"), r#""a\"b\\c\nd""#);
    }
}
