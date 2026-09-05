// The host side of a zaqaru container, in JavaScript: the two imports the
// module needs, the mount table behind them, a tape to replay from, and
// snapshot and restore. An ES module with no dependencies, so the same code
// runs a container in a browser Worker and under Node for the smoke test.
//
// The wire shapes are the canonical ABI's, as `crates/guest/src/wire.rs`
// states them and `crates/host/src/lib.rs` writes them: a `list<u8>` is a
// `(pointer, length)` pair of u32s; `ll_read`'s return area is sixteen bytes
// — a result discriminant, then either an option discriminant and the bytes'
// pair, or the error message's pair; `ll_write`'s is twelve — a discriminant
// and one pair. Bytes handed to the guest are placed by calling its own
// `cabi_realloc`.

const encoder = new TextEncoder();
const decoder = new TextDecoder();

export const KIND = { RUNNING: 0, IDLE: 1, FINISHED: 2, STOPPED: 3 };

export function text(bytes) {
  return decoder.decode(bytes);
}

export function bytes(string) {
  return encoder.encode(string);
}

function key(path) {
  return path.map((segment) => text(segment)).join("/");
}

function segmentsOf(path) {
  return path.map((segment) => (typeof segment === "string" ? bytes(segment) : segment));
}

function sameBytes(a, b) {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

function samePath(a, b) {
  return a.length === b.length && a.every((segment, i) => sameBytes(segment, b[i]));
}

// ---- stores -----------------------------------------------------------------
//
// A store answers `read(path) -> Uint8Array | null` (null: nothing at that
// path) or throws a string (the store failed), and `write(path, data) ->
// path`. `snapshot()` copies its state, or returns null for state that
// cannot be copied.

/// Keeps every write and hands it back on read: the console, the log.
export class Sink {
  constructor(entries = new Map(), echo = null) {
    this.entries = entries;
    this.echo = echo;
  }
  read(path) {
    const held = this.entries.get(key(path));
    return held ? held.slice() : null;
  }
  write(path, data) {
    const k = key(path);
    const held = this.entries.get(k) ?? new Uint8Array(0);
    const joined = new Uint8Array(held.length + data.length);
    joined.set(held);
    joined.set(data, held.length);
    this.entries.set(k, joined);
    if (this.echo) this.echo(path, data);
    return path;
  }
  place(path, data) {
    this.entries.set(key(segmentsOf(path)), data);
  }
  snapshot() {
    return new Sink(new Map([...this.entries].map(([k, v]) => [k, v.slice()])), this.echo);
  }
}

/// The host's clocks, as nanoseconds in decimal text.
export class Clock {
  constructor(started = performance.now()) {
    this.started = started;
  }
  read(path) {
    const last = text(path[path.length - 1]);
    if (last === "realtime_ns") return bytes(String(BigInt(Date.now()) * 1000000n));
    if (last === "monotonic_ns") return bytes(String(BigInt(Math.round((performance.now() - this.started) * 1e6))));
    return null;
  }
  write(path) {
    throw `${key(path)} is not writable`;
  }
  snapshot() {
    return new Clock(this.started);
  }
}

/// `/iso/shutdown`: whether the host asked the container to stop.
export class Shutdown {
  constructor(requested = false) {
    this.requested = requested;
    this.complete = null;
  }
  read(path) {
    if (text(path[path.length - 1]) === "requested") return this.requested ? bytes("1") : null;
    return null;
  }
  write(path, data) {
    if (text(path[path.length - 1]) === "complete") this.complete = text(data);
    return path;
  }
  snapshot() {
    const copy = new Shutdown(this.requested);
    copy.complete = this.complete;
    return copy;
  }
}

/// The runtime half of the isotope Server Protocol: the queue the host's
/// reads of the container's store wait in, and the Responses the kernel
/// writes back. Shared between a container and its snapshots, which is fine:
/// between turns the queue is empty.
export class Server {
  constructor() {
    this.next = 0;
    this.pending = [];
    this.responses = new Map();
  }
  ask(path) {
    const id = this.next++;
    this.pending.push({ id, path });
    return id;
  }
  answer(id) {
    const held = this.responses.get(id);
    this.responses.delete(id);
    return held;
  }
  read(path) {
    const [, , what, which] = path.map((s) => text(s));
    if (what === "requests" && which === "pending") {
      const batch = this.pending.map(
        ({ id, path }) =>
          `{"op":"read","path":${JSON.stringify(path)},"data":null,"respond_to":"/iso/server/responses/${id}"}`,
      );
      this.pending = [];
      return bytes(`[${batch.join(",")}]`);
    }
    if (what === "responses") {
      const held = this.responses.get(Number(which));
      return held ? held.slice() : null;
    }
    return null;
  }
  write(path, data) {
    const [, , what, which] = path.map((s) => text(s));
    if (what !== "responses") throw `${key(path)} is not writable`;
    this.responses.set(Number(which), data.slice());
    return path;
  }
  snapshot() {
    return this;
  }
}

// ---- the tape ---------------------------------------------------------------

/// Parses a tape `zaqaru run --record` wrote: `ZQT1`, a mode byte, a count,
/// then each entry as its path segments and its answer, length-prefixed.
export function parseTape(raw) {
  const view = new DataView(raw.buffer, raw.byteOffset, raw.byteLength);
  if (raw.length < 5 || text(raw.subarray(0, 4)) !== "ZQT1") throw "not a tape: the file does not begin with ZQT1";
  const bytecode = raw[4] !== 0;
  let at = 5;
  const word = () => {
    const held = view.getUint32(at, true);
    at += 4;
    return held;
  };
  const piece = () => {
    const length = word();
    const held = raw.subarray(at, at + length);
    at += length;
    return held;
  };
  const count = word();
  const entries = [];
  for (let i = 0; i < count; i++) {
    const segments = word();
    const path = [];
    for (let j = 0; j < segments; j++) path.push(piece());
    const kind = raw[at++];
    let answer;
    if (kind === 1) answer = { ok: piece() };
    else if (kind === 0) answer = { ok: null };
    else if (kind === 2) answer = { error: text(piece()) };
    else throw `a tape with an answer of kind ${kind}`;
    entries.push({ path, answer });
  }
  return { bytecode, entries };
}

// ---- the mount table --------------------------------------------------------

export class MountTable {
  constructor() {
    this.mounts = [];
    this.tape = null; // { entries, at } when replaying
    this.recording = null; // [] when recording
    this.server = null;
  }
  mount(prefix, store) {
    this.mounts.push({ prefix: segmentsOf(prefix), store });
    this.mounts.sort((a, b) => b.prefix.length - a.prefix.length);
  }
  serve() {
    this.server = new Server();
    this.mount(["iso", "server"], this.server);
    return this.server;
  }
  replay(tape) {
    this.tape = { entries: tape.entries, at: 0 };
  }
  resolve(path) {
    return this.mounts.find(({ prefix }) => prefix.length <= path.length && samePath(prefix, path.slice(0, prefix.length)));
  }
  resolves(path) {
    return this.resolve(segmentsOf(path)) !== undefined;
  }
  isServerPath(path) {
    return path.length >= 2 && text(path[0]) === "iso" && text(path[1]) === "server";
  }
  /// `{ ok: Uint8Array | null }` or `{ error: string }`, as the tape holds it.
  read(path) {
    if (!this.isServerPath(path)) {
      if (this.tape) {
        const entry = this.tape.entries[this.tape.at++];
        if (!entry) return { error: `the tape ran out at ${key(path)}` };
        if (!samePath(entry.path, path)) return { error: `the tape says ${key(entry.path)} and the run asked ${key(path)}` };
        return entry.answer;
      }
    }
    const found = this.resolve(path);
    let answer;
    if (!found) answer = { error: `nothing is mounted at ${key(path)}` };
    else {
      try {
        answer = { ok: found.store.read(path) };
      } catch (why) {
        answer = { error: String(why) };
      }
    }
    if (this.recording && !this.isServerPath(path)) this.recording.push({ path, answer });
    return answer;
  }
  write(path, data) {
    const found = this.resolve(path);
    if (!found) return { error: `nothing is mounted at ${key(path)}` };
    try {
      return { ok: found.store.write(path, data) };
    } catch (why) {
      return { error: String(why) };
    }
  }
  /// What a sink holds, for the host reading back what the container wrote.
  readback(path) {
    const found = this.resolve(segmentsOf(path));
    if (!found) return null;
    return found.store.read(segmentsOf(path));
  }
  snapshot() {
    const copy = new MountTable();
    for (const { prefix, store } of this.mounts) {
      const held = store.snapshot();
      if (!held) throw `the store at ${key(prefix)} holds state a snapshot cannot copy`;
      copy.mounts.push({ prefix, store: held });
    }
    copy.tape = this.tape ? { entries: this.tape.entries, at: this.tape.at } : null;
    copy.recording = this.recording ? this.recording.slice() : null;
    copy.server = this.server;
    return copy;
  }
}

/// The mounts a plain run needs: a console, a log, entropy, a clock, the
/// shutdown switch, the self store and the server. `seed` fixes the entropy.
export function standardMounts({ seed = 0x5a, echo = null, config = {} } = {}) {
  const mounts = new MountTable();
  mounts.mount(["iso", "console"], new Sink(new Map(), echo));
  mounts.mount(["iso", "log"], new Sink());
  mounts.mount(["iso", "self"], new Sink());
  const random = new Sink();
  random.place(["iso", "random", "bytes", "32"], new Uint8Array(32).fill(seed));
  mounts.mount(["iso", "random"], random);
  mounts.mount(["iso", "time"], new Clock());
  mounts.mount(["iso", "shutdown"], new Shutdown());
  const settings = new Sink();
  for (const [name, value] of Object.entries(config)) settings.place(["iso", "config", name], bytes(String(value)));
  mounts.mount(["iso", "config"], settings);
  mounts.serve();
  return mounts;
}

// ---- the container ----------------------------------------------------------

export class Container {
  constructor(module, instance, mounts) {
    this.module = module;
    this.instance = instance;
    this.mounts = mounts;
  }

  /// Compiles (if given bytes) and instantiates a container module against
  /// a mount table.
  static async instantiate(moduleOrBytes, mounts) {
    const module = moduleOrBytes instanceof WebAssembly.Module ? moduleOrBytes : await WebAssembly.compile(moduleOrBytes);
    if (!mounts.resolves(["iso", "log", "error"])) throw "nothing is mounted at /iso/log, so the kernel could not report an unimplemented syscall";
    const container = new Container(module, null, mounts);
    container.instance = await WebAssembly.instantiate(module, { env: container.imports() });
    return container;
  }

  get memory() {
    return this.instance.exports.memory;
  }

  /// A view of memory, fetched per use: the buffer detaches when the guest
  /// grows its memory.
  view() {
    return new DataView(this.memory.buffer);
  }

  readBytes(pointer, length) {
    return new Uint8Array(this.memory.buffer, pointer, length).slice();
  }

  readPath(pointer, count) {
    const view = this.view();
    const path = [];
    for (let i = 0; i < count; i++) {
      const segmentPointer = view.getUint32(pointer + i * 8, true);
      const segmentLength = view.getUint32(pointer + i * 8 + 4, true);
      path.push(this.readBytes(segmentPointer, segmentLength));
    }
    return path;
  }

  /// Places bytes in guest memory through the guest's own allocator.
  place(data, align = 1) {
    const pointer = this.instance.exports.cabi_realloc(0, 0, align, data.length);
    if (data.length) new Uint8Array(this.memory.buffer, pointer, data.length).set(data);
    return pointer;
  }

  imports() {
    return {
      ll_read: (path, pathLength, result) => {
        const answer = this.mounts.read(this.readPath(path, pathLength));
        const view = this.view();
        if (answer.error !== undefined) {
          const message = bytes(answer.error);
          const placed = this.place(message);
          const v = this.view();
          v.setUint32(result, 1, true);
          v.setUint32(result + 4, placed, true);
          v.setUint32(result + 8, message.length, true);
          v.setUint32(result + 12, 0, true);
        } else if (answer.ok === null) {
          view.setUint32(result, 0, true);
          view.setUint32(result + 4, 0, true);
          view.setUint32(result + 8, 0, true);
          view.setUint32(result + 12, 0, true);
        } else {
          const placed = this.place(answer.ok);
          const v = this.view();
          v.setUint32(result, 0, true);
          v.setUint32(result + 4, 1, true); // option: some
          v.setUint32(result + 8, placed, true);
          v.setUint32(result + 12, answer.ok.length, true);
        }
      },
      ll_write: (path, pathLength, data, dataLength, result) => {
        const answer = this.mounts.write(this.readPath(path, pathLength), this.readBytes(data, dataLength));
        if (answer.error !== undefined) {
          const message = bytes(answer.error);
          const placed = this.place(message);
          const v = this.view();
          v.setUint32(result, 1, true);
          v.setUint32(result + 4, placed, true);
          v.setUint32(result + 8, message.length, true);
        } else {
          // The result path as a list of pairs, which the guest does not
          // read; placed anyway so the area is a well-formed value.
          const segments = answer.ok;
          const records = new Uint8Array(segments.length * 8);
          const recordsView = new DataView(records.buffer);
          segments.forEach((segment, i) => {
            const placed = this.place(segment);
            recordsView.setUint32(i * 8, placed, true);
            recordsView.setUint32(i * 8 + 4, segment.length, true);
          });
          const placed = this.place(records, 4);
          const v = this.view();
          v.setUint32(result, 0, true);
          v.setUint32(result + 4, placed, true);
          v.setUint32(result + 8, segments.length, true);
        }
      },
    };
  }

  decode(word) {
    const kind = word & 0xff;
    return { kind, status: kind === KIND.FINISHED ? (word >> 8) & 0xff : null };
  }

  /// Runs until the container has retired `until` instructions in total,
  /// finished, or idled once; negative runs to completion. The first call
  /// boots.
  step(until) {
    return this.decode(this.instance.exports.zaqaru_run(BigInt(until)));
  }

  /// Runs to exactly `target` retired instructions and holds the machine
  /// there, for reading; not for continuing.
  stopAt(target) {
    return this.decode(this.instance.exports.zaqaru_stop_at(BigInt(target)));
  }

  /// Runs to completion; answers the exit status.
  boot() {
    for (;;) {
      const turn = this.step(-1);
      if (turn.kind === KIND.FINISHED) return turn.status;
    }
  }

  /// Reads a path of the container's own store; answers the Response text.
  ask(path) {
    if (!this.mounts.server) throw "nothing is mounted at /iso/server";
    const id = this.mounts.server.ask(path);
    this.step(0);
    const answer = this.mounts.server.answer(id);
    if (!answer) throw `the container did not answer the read of ${path}`;
    return text(answer);
  }

  /// The container's answer to a read, parsed: the value on `ok`, or a
  /// thrown error.
  value(path) {
    const response = JSON.parse(this.ask(path));
    if (response.result !== "ok") throw `${path}: ${response.error?.type}: ${response.error?.message}`;
    return response.value;
  }

  readback(path) {
    return this.mounts.readback(path);
  }

  /// The Block's manifest, as the module declares it.
  manifest() {
    const pointer = this.instance.exports.manifest();
    const all = new Uint8Array(this.memory.buffer, pointer);
    let end = 0;
    while (all[end] !== 0) end++;
    return text(all.subarray(0, end));
  }

  get stackPointer() {
    return this.instance.exports.__stack_pointer.value;
  }

  /// The whole container, between turns: memory, the one global, the host's
  /// side of the boundary.
  snapshot() {
    return {
      memory: new Uint8Array(this.memory.buffer).slice(),
      stackPointer: this.stackPointer,
      mounts: this.mounts.snapshot(),
    };
  }

  /// A container standing where the snapshot was taken: a fresh instance of
  /// the same module, grown to the snapshot's size, memory copied back.
  ///
  /// The snapshot is either dense — `memory`, as [`snapshot`] makes — or
  /// sparse — `pages`, a map from 4 KiB page index to bytes, and `length` —
  /// as a checkpoint store keeps. A fresh memory is zero, so sparse pages
  /// are written straight in.
  async restore(snapshot) {
    const mounts = snapshot.mounts.snapshot();
    const container = new Container(this.module, null, mounts);
    container.instance = await WebAssembly.instantiate(this.module, { env: container.imports() });
    const have = container.memory.buffer.byteLength;
    const want = snapshot.memory ? snapshot.memory.length : snapshot.length;
    if (want < have) throw "the snapshot's memory is smaller than a fresh instance's";
    if (want > have) container.memory.grow(Math.ceil((want - have) / 65536));
    const target = new Uint8Array(container.memory.buffer);
    if (snapshot.memory) target.set(snapshot.memory);
    else for (const [page, bytes] of snapshot.pages) target.set(bytes.subarray(0, Math.min(bytes.length, want - page * 4096)), page * 4096);
    if (container.stackPointer !== snapshot.stackPointer) throw "the guest left state on its stack";
    return container;
  }
}
