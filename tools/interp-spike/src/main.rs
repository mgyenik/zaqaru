//! Native timing for the spike: the same interpreter, same guest, measured
//! with the host clock — the baseline the wasm number is compared against.

fn time(name: &str, run: extern "C" fn(u64) -> u64, iterations: u64) {
    let start = std::time::Instant::now();
    let retired = run(iterations);
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "{name}: {retired} instructions in {elapsed:.3}s = {:.1} MIPS (rax={:#x})",
        retired as f64 / elapsed / 1e6,
        interp_spike::checksum(),
    );
}

fn main() {
    let iterations: u64 = std::env::args()
        .nth(1)
        .and_then(|argument| argument.parse().ok())
        .unwrap_or(20_000_000);
    time("decode-every-time", interp_spike::run_decode, iterations);
    time("cached           ", interp_spike::run_cached, iterations);
}
