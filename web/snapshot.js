// A container written to a file and read back: the pages of its memory
// that differ from a fresh instance's, the stack pointer, the retired count,
// and the host's side of the boundary as `MountTable.save()` renders it.
// This is how the demo starts from a booted Django rather than booting one
// in the browser: `preboot.mjs` runs a module until it is quiet and writes
// the file; the worker reads it and restores.
//
// The format, gzip-compressed as a whole:
//
//   "ZQS1"  u32 header-length  header (JSON, UTF-8)
//   then header.pages times:  u32 page-index  4096 bytes
//
// The header is { at, stackPointer, length, pages, mounts }. Only pages the
// booted container changed are in the file, so a 200 MB memory whose image
// is 70 MB of it is a file of what booting touched.

import { diff, PAGE } from "./checkpoints.js";

const MAGIC = "ZQS1";

/// The file's bytes, before compression.
export function encode({ at, stackPointer, length, pages, mounts }) {
  const header = new TextEncoder().encode(JSON.stringify({ at, stackPointer, length, pages: pages.size, mounts }));
  const out = new Uint8Array(8 + header.length + pages.size * (4 + PAGE));
  const view = new DataView(out.buffer);
  out.set(new TextEncoder().encode(MAGIC), 0);
  view.setUint32(4, header.length, true);
  out.set(header, 8);
  let cursor = 8 + header.length;
  for (const [page, bytes] of [...pages].sort((a, b) => a[0] - b[0])) {
    view.setUint32(cursor, page, true);
    out.set(bytes, cursor + 4);
    cursor += 4 + PAGE;
  }
  return out;
}

/// `{ at, stackPointer, length, pages: Map, relative: true, mounts }` from
/// the file's bytes, decompressed. `relative`: the pages are those that
/// differ from a fresh instance's, which is what `Container.fromSnapshot`
/// needs to know to fill in the rest.
export function decode(raw) {
  const view = new DataView(raw.buffer, raw.byteOffset, raw.byteLength);
  if (raw.length < 8 || new TextDecoder().decode(raw.subarray(0, 4)) !== MAGIC) throw "not a snapshot: the file does not begin with ZQS1";
  const headerLength = view.getUint32(4, true);
  const header = JSON.parse(new TextDecoder().decode(raw.subarray(8, 8 + headerLength)));
  const pages = new Map();
  let cursor = 8 + headerLength;
  for (let i = 0; i < header.pages; i++) {
    const page = view.getUint32(cursor, true);
    pages.set(page, raw.subarray(cursor + 4, cursor + 4 + PAGE));
    cursor += 4 + PAGE;
  }
  return { at: header.at, stackPointer: header.stackPointer, length: header.length, pages, relative: true, mounts: header.mounts };
}

/// The pages of `memory` a snapshot has to carry, given a fresh instance's
/// memory of the same module.
export function changedSince(fresh, memory) {
  const base = diff(null, fresh).changed;
  return diff(base, memory).changed;
}

async function through(stream, bytes) {
  const compressed = new Blob([bytes]).stream().pipeThrough(stream);
  return new Uint8Array(await new Response(compressed).arrayBuffer());
}

export function gzip(bytes) {
  return through(new CompressionStream("gzip"), bytes);
}

export function gunzip(bytes) {
  return through(new DecompressionStream("gzip"), bytes);
}
