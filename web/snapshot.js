// A container written to a file and read back: the pages of its memory
// that differ from a fresh instance's, the stack pointer, the retired count,
// and the host's side of the boundary as `MountTable.save()` renders it.
// This is how the demo starts from a booted Django rather than booting one
// in the browser: `preboot.mjs` runs a module until it is quiet and writes
// the file; the worker reads it and restores.
//
// The format, gzip-compressed as a whole:
//
//   "ZQS2"  u32 header-length  header (JSON, UTF-8)
//   then header.pages times:  u32 page-index  u32 same-as  [4096 bytes]
//
// `same-as` is 0xffffffff and the bytes follow, or the index of an earlier
// page in the file whose bytes this page repeats — a tenth of a booted
// Django's pages are copies of another: a forked process's page that never
// diverged, held in place for one process and displaced for the other.
//
// The header is { at, stackPointer, length, pages, refill, mounts }. Only
// pages the booted container changed are in the file, less what the kernel
// puts back on request: with `refill` set, whoever continues from the file
// writes `refill` to the container's `caches/decompressed` path, and the
// kernel decompresses its cached files into their buffers again — the
// largest part of a booted Django's memory, and a function of the image.

import { diff, PAGE } from "./checkpoints.js";

/// What a container continued from a file needs done before it runs: the
/// kernel refills its decompressed files. Answers what the kernel wrote.
export function refill(container) {
  return container.put("caches/decompressed", "refill");
}

/// What a quiet container is asked before it is written to a file, so that
/// the file holds less: the block caches are flushed (zeroing what they
/// free — the kernel decodes again on demand), the pooled page buffers are
/// zeroed, and the decompressed files' buffers and the unmapped guest
/// pages are named so `omit` can leave them out. Answers the ranges to omit
/// and what was done.
export function prepare(container) {
  const flushed = container.put("caches/blocks", "flush");
  const pooled = container.put("caches/pool", "zero").pages;
  const cache = container.value("caches/decompressed");
  return {
    flushed,
    pooled,
    cache,
    cacheRanges: cache.ranges.map((r) => [Number(BigInt(r.address)), r.length]),
    gaps: deadRanges(container, guestBlock(container)),
  };
}

const MAGIC = "ZQS2";
const FRESH = 0xffffffff;

/// A key for a page's bytes, for finding repeats: the whole page, as a
/// string, since a hash would need the whole page compared anyway.
function contentKey(bytes) {
  let key = "";
  for (let i = 0; i < bytes.length; i += 0x2000) key += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x2000));
  return key;
}

/// The file's bytes, before compression.
export function encode({ at, stackPointer, length, pages, refill = false, mounts }) {
  const header = new TextEncoder().encode(JSON.stringify({ at, stackPointer, length, pages: pages.size, refill, mounts }));
  const out = new Uint8Array(8 + header.length + pages.size * (8 + PAGE));
  const view = new DataView(out.buffer);
  out.set(new TextEncoder().encode(MAGIC), 0);
  view.setUint32(4, header.length, true);
  out.set(header, 8);
  let cursor = 8 + header.length;
  const seen = new Map();
  for (const [page, bytes] of [...pages].sort((a, b) => a[0] - b[0])) {
    view.setUint32(cursor, page, true);
    const key = contentKey(bytes);
    const earlier = seen.get(key);
    if (earlier !== undefined) {
      view.setUint32(cursor + 4, earlier, true);
      cursor += 8;
      continue;
    }
    seen.set(key, page);
    view.setUint32(cursor + 4, FRESH, true);
    out.set(bytes, cursor + 8);
    cursor += 8 + PAGE;
  }
  return out.subarray(0, cursor);
}

/// `{ at, stackPointer, length, pages: Map, relative: true, mounts }` from
/// the file's bytes, decompressed. `relative`: the pages are those that
/// differ from a fresh instance's, which is what `Container.fromSnapshot`
/// needs to know to fill in the rest.
export function decode(raw) {
  const view = new DataView(raw.buffer, raw.byteOffset, raw.byteLength);
  if (raw.length < 8 || new TextDecoder().decode(raw.subarray(0, 4)) !== MAGIC) throw `not a snapshot: the file does not begin with ${MAGIC}`;
  const headerLength = view.getUint32(4, true);
  const header = JSON.parse(new TextDecoder().decode(raw.subarray(8, 8 + headerLength)));
  const pages = new Map();
  let cursor = 8 + headerLength;
  for (let i = 0; i < header.pages; i++) {
    const page = view.getUint32(cursor, true);
    const sameAs = view.getUint32(cursor + 4, true);
    if (sameAs === FRESH) {
      pages.set(page, raw.subarray(cursor + 8, cursor + 8 + PAGE));
      cursor += 8 + PAGE;
    } else {
      const source = pages.get(sameAs);
      if (!source) throw `a snapshot page repeats page ${sameAs}, which the file has not given`;
      pages.set(page, source);
      cursor += 8;
    }
  }
  return { at: header.at, stackPointer: header.stackPointer, length: header.length, pages, relative: true, refill: header.refill === true, mounts: header.mounts };
}

/// The pages of `memory` a snapshot has to carry, given a fresh instance's
/// memory of the same module.
export function changedSince(fresh, memory) {
  const base = diff(null, fresh).changed;
  return diff(base, memory).changed;
}

/// Removes from `pages` what a restored container gets back without the
/// file: pages wholly inside a range the kernel can refill (its decompressed
/// files) or a range whose bytes nothing reads (guest pages no process
/// maps; the kernel fills a page before it is handed out again). Only pages
/// above `freshLength`, which a fresh instance has as zero; a page a range
/// covers only partly is kept whole. Answers how many pages went.
export function omit(pages, ranges, freshLength) {
  let omitted = 0;
  for (const [start, length] of ranges) {
    const first = Math.ceil(start / PAGE);
    const last = Math.floor((start + length) / PAGE); // exclusive
    for (let page = first; page < last; page++) {
      if (page * PAGE < freshLength) continue;
      if (pages.delete(page)) omitted++;
    }
  }
  return omitted;
}

/// The pages of the guest block no process can reach, as `[start, length]`
/// ranges: the block's bounds less every page any live process's permission
/// bits admit (`processes/{pid}/mapped`, which is what the interpreter
/// itself checks an access against). A page nobody can reach holds nothing
/// anybody will read — the kernel fills a page before it is mapped again —
/// so a snapshot can leave it out. The bits, not the rendered memory map:
/// the map is for people, and a heap or a vDSO it did not name would be a
/// live page zeroed.
export function deadRanges(container, block) {
  const reachable = [];
  for (const p of container.value("processes").processes) {
    for (const [a, b] of container.value(`processes/${p.pid}/mapped`).ranges) {
      const start = Number(BigInt(a));
      const end = Number(BigInt(b));
      if (end <= block.start || start >= block.end) continue;
      reachable.push([Math.max(start, block.start), Math.min(end, block.end)]);
    }
  }
  reachable.sort((x, y) => x[0] - y[0]);
  const dead = [];
  let end = block.start;
  for (const [a, b] of reachable) {
    if (a > end) dead.push([end, a - end]);
    end = Math.max(end, b);
  }
  if (end < block.end) dead.push([end, block.end - end]);
  return dead;
}

/// The guest block's bounds, from the container's `layout` path.
export function guestBlock(container) {
  const layout = container.value("layout").guest_block;
  return { start: Number(BigInt(layout.start)), end: Number(BigInt(layout.end)) };
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
