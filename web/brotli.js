// Inflating brotli in the page, through a wasm module compiled from
// `crates/brotli` (build: `web/fixture.sh` or `web/demo.sh`, or
// `cargo build -p zaqaru-brotli --target wasm32-unknown-unknown --release`).
// Browsers inflate gzip natively and brotli not at all; a snapshot is a
// fifth smaller as brotli, which is what this pays for.
//
// A brotli snapshot is wrapped: "ZQB1", the inflated length as a u32, then
// the brotli stream — so the output can be placed at its exact size.

const MAGIC = "ZQB1";
let modulePromise = null;

/// Whether `bytes` is a wrapped brotli stream.
export function isBrotli(bytes) {
  return bytes.length >= 8 && new TextDecoder().decode(bytes.subarray(0, 4)) === MAGIC;
}

/// Wraps a brotli stream with its inflated length.
export function wrap(compressed, rawLength) {
  const out = new Uint8Array(8 + compressed.length);
  out.set(new TextEncoder().encode(MAGIC), 0);
  new DataView(out.buffer).setUint32(4, rawLength, true);
  out.set(compressed, 8);
  return out;
}

/// The decoder module, fetched once from beside this file.
function decoder() {
  if (!modulePromise) {
    const url = new URL("./brotli.wasm", import.meta.url);
    modulePromise = (typeof process !== "undefined" && process.versions?.node
      ? import("node:fs/promises").then((fs) => fs.readFile(url)).then((bytes) => WebAssembly.compile(bytes))
      : WebAssembly.compileStreaming(fetch(url))
    ).catch((why) => {
      modulePromise = null;
      throw `the brotli decoder (web/brotli.wasm) could not be loaded: ${why}`;
    });
  }
  return modulePromise;
}

/// The inflated bytes of a wrapped brotli stream.
export async function inflateBrotli(wrapped) {
  if (!isBrotli(wrapped)) throw "not a brotli snapshot";
  const rawLength = new DataView(wrapped.buffer, wrapped.byteOffset, wrapped.byteLength).getUint32(4, true);
  const compressed = wrapped.subarray(8);
  const instance = await WebAssembly.instantiate(await decoder(), {});
  const { brotli_alloc, brotli_free, brotli_decompress, memory } = instance.exports;
  const input = brotli_alloc(compressed.length);
  const output = brotli_alloc(rawLength);
  if (!input || !output) throw "the brotli decoder could not reserve memory";
  new Uint8Array(memory.buffer, input, compressed.length).set(compressed);
  const written = Number(brotli_decompress(input, compressed.length, output, rawLength));
  if (written < 0) throw written === -2 ? "the brotli snapshot inflates to more than its prefix says" : "the brotli snapshot is corrupt or truncated";
  if (written !== rawLength) throw `the brotli snapshot inflated to ${written} bytes, not the ${rawLength} its prefix says`;
  const inflated = new Uint8Array(memory.buffer, output, rawLength).slice();
  brotli_free(input, compressed.length);
  brotli_free(output, rawLength);
  return inflated;
}
