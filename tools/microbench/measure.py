# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Native against the same code baked into a wasm container. Napkin math.

Two legs and one question: how much slower is a program when it runs as an
interpreted guest inside a wasm module than when Linux runs it directly.

  native   the ELF, run by Linux.
  wasm     the same ELF, baked into a container with the interpreter and
           run under wasmtime by `zaqaru run`.

**Three costs, kept apart, because they behave completely differently.**

  compile  wasmtime turning the module into machine code. Fixed per process
           and proportional to module size, not to what the program does --
           a container with a bigger filesystem pays more of it while
           computing exactly the same thing.
  start-up whatever else happens before the program's own work: the kernel
           booting, the ELF being mapped, libc initialising.
  steady   the workload itself, once everything is warm.

Steady state is measured as a difference: each kernel runs at scale S and at
2S, and the reported cost is min(2S) - min(S). Subtracting cancels compile
and start-up exactly, along with any setup a kernel does before its loop --
the array fill in `memory_sequential`, the 2M-element shuffle in
`memory_random`. Those are not small: the shuffle alone is larger than the
chase it sets up.

**On precision.** This is a laptop with a scaling governor, 24 cores and a
non-realtime kernel. Runs are pinned to one core and the *minimum* of
several is taken, because interference is one-sided -- nothing makes a run
finish faster than it should. That is worth about two significant figures,
which is fine, because the answer is a ratio in the hundreds and no amount
of scheduler noise moves it. Do not read the third digit.

The one exact number here is the instruction count: the engine increments a
counter as it decodes, so it is a property of the program rather than a
measurement of the machine. Rates are that exact count over a noisy second.
"""

import json
import statistics
import subprocess
import sys
import time
from pathlib import Path

REPEATS = 3
# Long enough that the core has clocked up. Native runs at these sizes
# otherwise finish during the frequency ramp, where time is not proportional
# to work: 150, 300 and 600 iterations of `calls` measured 11.0, 17.8 and
# 25.9 ms, which is not a straight line through anything.
MIN_NATIVE = 0.30
# One core, so a run is not migrated mid-measurement.
CORE = "2"

SCALES = {
    "alu": 7_000_000,
    "memory_sequential": 2,
    # Eight million, because the chase has to dominate the shuffle that
    # sets it up. At five hundred thousand the differential signal was a
    # couple of million instructions against a sixty-million-instruction
    # permutation, and the measurement swung between 1.1x and 2.1x on
    # reruns of identical code -- a fixed cost cancels in the subtraction,
    # but its variance does not.
    "memory_random": 8_000_000,
    "calls": 150,
    "branches": 1_500_000,
    "string": 2_500,
    "float": 1_000_000,
    "syscalls": 40_000,
    "alloc": 200_000,
}

REPO = Path(__file__).resolve().parents[2]
WORK = Path("/tmp/microbench")
ROOT = WORK / "root"


def build() -> None:
    WORK.mkdir(parents=True, exist_ok=True)
    ROOT.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["gcc", "-O2", "-static", "-o", str(ROOT / "init"),
         str(REPO / "tools/microbench/bench.c"), "-lm"],
        check=True,
    )


def bake(name: str, scale: int) -> Path:
    out = WORK / f"{name}.{scale}.wasm"
    if not out.is_file():
        subprocess.run(
            [str(REPO / "target/release/zaqaru"), "bake", str(ROOT), "-o", str(out),
             "--", "/init", name, str(scale)],
            check=True, capture_output=True,
        )
    return out


def run_once(leg: str, name: str, scale: int) -> dict:
    if leg == "native":
        command = [str(ROOT / "init"), name, str(scale)]
    else:
        command = [str(REPO / "target/release/zaqaru"), "run", str(bake(name, scale))]
    started = time.perf_counter()
    done = subprocess.run(["taskset", "-c", CORE] + command, capture_output=True, text=True)
    total = time.perf_counter() - started
    if done.returncode != 0:
        raise SystemExit(f"{leg}/{name} failed:\n{done.stderr[-2000:]}")
    text = done.stdout + done.stderr
    row = {"total": total, "answer": ""}
    for line in text.splitlines():
        if line.startswith(name + " ") or line.startswith("noop "):
            row["answer"] = line.strip()
        elif " instructions in " in line:
            row["retired"] = int(next(w for w in line.replace(":", " ").split() if w.isdigit()))
        elif "of module in" in line:
            row["compile"] = float(line.rsplit(" in ", 1)[1].rstrip("s\n"))
    return row


def best(leg: str, name: str, scale: int) -> dict:
    """Minimum of `REPEATS`, which is the sample least interfered with."""
    rows = [run_once(leg, name, scale) for _ in range(REPEATS)]
    # NOTE: callers must interleave the two scales -- see `measure`.
    fastest = min(rows, key=lambda row: row["total"])
    fastest["spread"] = max(r["total"] for r in rows) / min(r["total"] for r in rows)
    return fastest


def interleaved(leg: str, name: str, scale: int) -> tuple[dict, dict]:
    """Alternates the two scales so clock drift hits both equally."""
    low: list[dict] = []
    high: list[dict] = []
    for _ in range(REPEATS):
        low.append(run_once(leg, name, scale))
        high.append(run_once(leg, name, scale * 2))
    fastest = lambda rows: min(rows, key=lambda row: row["total"])
    pick_low, pick_high = fastest(low), fastest(high)
    for picked, rows in ((pick_low, low), (pick_high, high)):
        picked["spread"] = max(r["total"] for r in rows) / min(r["total"] for r in rows)
    return pick_low, pick_high


def calibrate(name: str, scale: int) -> int:
    while scale < 1 << 40 and run_once("native", name, scale)["total"] < MIN_NATIVE:
        scale *= 2
    return scale


def main() -> None:
    build()
    results: dict[str, dict] = {}

    print("fixed costs, from the same binary doing nothing:")
    for leg in ("native", "wasm"):
        row = best(leg, "noop", 0)
        results.setdefault("noop", {})[leg] = row
        compile_seconds = row.get("compile", 0.0)
        print(f"  {leg:7s} total {row['total'] * 1000:7.1f} ms"
              f"   of which compile {compile_seconds * 1000:6.1f} ms"
              f"   start-up {(row['total'] - compile_seconds) * 1000:6.1f} ms", flush=True)

    print("\nsteady state, start-up and compile subtracted out:")
    print(f"  {'kernel':<20} {'native':>12} {'wasm':>12} {'ratio':>8} {'MIPS':>7}  answers agree")
    for name, scale in SCALES.items():
        row: dict[str, dict] = {}
        for leg in ("native", "wasm"):
            at = calibrate(name, scale) if leg == "native" else scale
            # Interleaved, not one scale then the other. A long kernel
            # holds the core for tens of seconds and the clock decays
            # across the measurement, so taking every low sample before
            # every high one subtracts a later slower run from an earlier
            # faster one. That measured `calls` at 25.5, 96.8 and 47.1 MIPS
            # on three runs of identical code; alternating brought the
            # spread to 2.5%.
            low, high = interleaved(leg, name, at)
            seconds = high["total"] - low["total"]
            row[leg] = {
                "scale": at,
                "seconds": seconds,
                "per_unit": seconds / at,
                "spread": max(low["spread"], high["spread"]),
                "answers": [low["answer"], high["answer"]],
            }
            if "retired" in low:
                row[leg]["retired"] = high["retired"] - low["retired"]
        results[name] = row
        ratio = row["wasm"]["per_unit"] / row["native"]["per_unit"]
        mips = row["wasm"].get("retired", 0) / row["wasm"]["seconds"] / 1e6
        # The checksums, compared at a *matched* scale. The native leg runs
        # at a calibrated scale of its own, so its answers are answers to a
        # different question and comparing them directly reports a mismatch
        # on every row -- which this did, until the scales were matched.
        matched = {run_once("native", name, scale)["answer"],
                   run_once("native", name, scale * 2)["answer"]}
        agree = matched == set(row["wasm"]["answers"])
        row["checksums"] = sorted(matched)
        print(f"  {name:<20} {row['native']['per_unit'] * 1e9:9.1f} ns "
              f"{row['wasm']['per_unit'] * 1e9:9.1f} ns "
              f"{ratio:7.0f}x {mips:6.1f}  {'yes' if agree else 'NO -- DIFFERENT'}",
              flush=True)

    (WORK / "results.json").write_text(json.dumps(results, indent=2))
    print(f"\nper unit of each kernel's own scale, so the two columns are "
          f"comparable within a row and not across rows.")
    print(f"written to {WORK / 'results.json'}")


if __name__ == "__main__":
    main()
