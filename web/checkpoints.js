// Checkpoints of a container's memory, kept as pages.
//
// A container's memory is hundreds of megabytes even when the program is
// small: the kernel reserves its guest block at boot, and a wasm memory
// grows and never shrinks. Nearly all of it is zero, and nearly all of what
// is not is still between two checkpoints — the image blob never changes,
// and page ownership keeps a dormant process's pages in place. So a
// checkpoint is a map from 4 KiB page index to the page's bytes, holding
// only pages that are not zero; the pages are immutable and shared between
// checkpoints; and a checkpoint after the first is recorded as the pages
// that changed, found by comparing memory against the previous map word by
// word. Keeping a full map every so often costs only the map itself, since
// its pages are the ones already held, and bounds how many deltas a
// reconstruction applies.
//
// Restoring never builds a dense image: a fresh wasm memory is zero, so the
// pages are written into it directly.

export const PAGE = 4096;
const WORDS = PAGE >>> 2;

/// The pages of `memory` that differ from `pages` (a page absent from the
/// map is all zero), as a delta: `{ length, changed: Map<page, bytes> }`.
export function diff(pages, memory) {
  const words = new Uint32Array(memory.buffer, memory.byteOffset, memory.byteLength >>> 2);
  const count = Math.ceil(memory.length / PAGE);
  const changed = new Map();
  for (let page = 0; page < count; page++) {
    const start = page * WORDS;
    const end = Math.min(start + WORDS, words.length);
    const before = pages?.get(page);
    let same = true;
    if (before) {
      const beforeWords = new Uint32Array(before.buffer, before.byteOffset, before.byteLength >>> 2);
      for (let i = start, j = 0; i < end; i++, j++) {
        if (beforeWords[j] !== words[i]) {
          same = false;
          break;
        }
      }
    } else {
      for (let i = start; i < end; i++) {
        if (words[i] !== 0) {
          same = false;
          break;
        }
      }
    }
    if (!same) {
      const from = page * PAGE;
      changed.set(page, memory.slice(from, Math.min(from + PAGE, memory.length)));
    }
  }
  return { length: memory.length, changed };
}

/// `pages` with a delta applied: a new map sharing the unchanged pages.
export function apply(pages, delta) {
  const next = new Map(pages);
  for (const [page, bytes] of delta.changed) next.set(page, bytes);
  return next;
}

/// A dense image from a page map, for comparing against a full snapshot.
export function dense(pages, length) {
  const image = new Uint8Array(length);
  for (const [page, bytes] of pages) image.set(bytes.subarray(0, Math.min(bytes.length, length - page * PAGE)), page * PAGE);
  return image;
}

export class Checkpoints {
  constructor({ fullEvery = 16 } = {}) {
    this.fullEvery = fullEvery;
    this.entries = []; // { at, length, stackPointer, mounts, delta, full?: Map }
    this.latest = null; // the page map of the last checkpoint added
    this.materialised = null; // { index, pages }
    this.held = 0; // bytes of page data kept
  }

  /// Adds a checkpoint taken at `at` from a container standing between
  /// turns. Reads its memory in place; copies only the pages that changed.
  add(at, container) {
    const memory = new Uint8Array(container.memory.buffer);
    const delta = diff(this.latest, memory);
    for (const bytes of delta.changed.values()) this.held += bytes.length;
    this.latest = apply(this.latest ?? new Map(), delta);
    const entry = { at, length: delta.length, stackPointer: container.stackPointer, mounts: container.mounts.snapshot(), delta };
    if (this.entries.length % this.fullEvery === 0) entry.full = this.latest;
    this.entries.push(entry);
    return this.entries.length - 1;
  }

  /// The checkpoint at or before `at`: its index.
  before(at) {
    let found = 0;
    this.entries.forEach((entry, index) => {
      if (entry.at <= at) found = index;
    });
    return found;
  }

  get length() {
    return this.entries.length;
  }

  at(index) {
    return this.entries[index].at;
  }

  /// What the checkpoints would cost as full copies of memory.
  get naive() {
    return this.entries.reduce((sum, entry) => sum + entry.length, 0);
  }

  /// The page map of checkpoint `index`: the nearest full map at or before
  /// it, with every delta since applied in order. The last answer is kept.
  pages(index) {
    if (this.materialised?.index === index) return this.materialised.pages;
    let base = index;
    while (!this.entries[base].full) base--;
    let pages = this.entries[base].full;
    if (this.materialised && this.materialised.index > base && this.materialised.index < index) {
      base = this.materialised.index;
      pages = this.materialised.pages;
    }
    for (let i = base + 1; i <= index; i++) pages = apply(pages, this.entries[i].delta);
    this.materialised = { index, pages };
    return pages;
  }

  /// What a container is restored from, for checkpoint `index`.
  snapshot(index) {
    const entry = this.entries[index];
    return { pages: this.pages(index), length: entry.length, stackPointer: entry.stackPointer, mounts: entry.mounts };
  }

  /// The memory of checkpoint `index` as one image, for a test.
  memory(index) {
    return dense(this.pages(index), this.entries[index].length);
  }
}
