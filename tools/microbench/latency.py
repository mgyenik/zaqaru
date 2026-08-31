# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Latency of the nginx + gunicorn + Django stack, native against the module.

The same OCI image both ways: `docker run` on the left, `zaqaru-run` on the
right, the same `-p` convention, and the same client asking the same
question. What is being compared is the execution path and nothing else --
one image, built once, by an ordinary Dockerfile.

Four numbers, because a server has more than one kind of slow:

  start-up   how long until the stack answers 200 at all. Under
             interpretation this is Python's import graph, and it is the
             largest single cost here by a wide margin.
  cold       the first request after that, which still pays for whatever
             Django defers to first use.
  sequential the steady state, as a distribution. A median with no tail is
             a claim nobody can check -- p90 and p99 are where a server
             with one worker shows what queueing does to it.
  concurrent four clients at once. With one nginx worker and one sync
             gunicorn worker this measures *queueing*, not parallelism, and
             it is reported because that is what a reader would otherwise
             assume it disproves.
"""

import http.client
import json
import statistics
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor

WARMUP = 3
SEQUENTIAL = 30
CONCURRENCY = 4
EACH = 8


def fetch(port: int, timeout: float = 300.0) -> tuple[int, float, int]:
    started = time.perf_counter()
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    try:
        connection.request("GET", "/")
        response = connection.getresponse()
        body = response.read()
        return response.status, time.perf_counter() - started, len(body)
    finally:
        connection.close()


def await_ready(port: int, limit: float) -> float:
    """Seconds until the stack answers 200.

    Not until it answers *something*: nginx comes up before gunicorn does
    and says 502 in the meantime, which is correct of it and is not the
    stack being ready.
    """
    started = time.perf_counter()
    while time.perf_counter() - started < limit:
        try:
            status, _, _ = fetch(port, timeout=30.0)
            if status == 200:
                return time.perf_counter() - started
        except Exception:
            pass
        time.sleep(0.25)
    raise SystemExit(f"nothing answered 200 on port {port} within {limit}s")


def percentiles(samples: list[float]) -> dict[str, float]:
    ordered = sorted(samples)
    def at(fraction: float) -> float:
        return ordered[min(len(ordered) - 1, int(fraction * len(ordered)))]
    return {
        "min": ordered[0] * 1000,
        "p50": statistics.median(ordered) * 1000,
        "p90": at(0.90) * 1000,
        "p99": at(0.99) * 1000,
        "max": ordered[-1] * 1000,
    }


def measure(port: int, label: str, ready: float) -> dict:
    cold = fetch(port)[1]
    for _ in range(WARMUP):
        fetch(port)

    sequential = []
    for _ in range(SEQUENTIAL):
        status, seconds, size = fetch(port)
        if status != 200:
            raise SystemExit(f"{label}: got {status}")
        sequential.append(seconds)

    started = time.perf_counter()
    with ThreadPoolExecutor(max_workers=CONCURRENCY) as pool:
        futures = [pool.submit(fetch, port) for _ in range(CONCURRENCY * EACH)]
        concurrent = [future.result()[1] for future in futures]
    wall = time.perf_counter() - started

    result = {
        "ready_seconds": ready,
        "cold_ms": cold * 1000,
        "sequential": percentiles(sequential),
        "sequential_rps": len(sequential) / sum(sequential),
        "concurrent": percentiles(concurrent),
        "concurrent_rps": len(concurrent) / wall,
    }
    print(f"\n{label}")
    print(f"  start-up to first 200   {ready:8.2f} s")
    print(f"  first request after     {cold * 1000:8.1f} ms")
    s = result["sequential"]
    print(f"  sequential  min {s['min']:8.1f}  p50 {s['p50']:8.1f}  "
          f"p90 {s['p90']:8.1f}  p99 {s['p99']:8.1f}  max {s['max']:8.1f} ms")
    print(f"              {result['sequential_rps']:.2f} req/s")
    c = result["concurrent"]
    print(f"  {CONCURRENCY} at once   min {c['min']:8.1f}  p50 {c['p50']:8.1f}  "
          f"p90 {c['p90']:8.1f}  p99 {c['p99']:8.1f}  max {c['max']:8.1f} ms")
    print(f"              {result['concurrent_rps']:.2f} req/s", flush=True)
    return result


def main() -> None:
    which = sys.argv[1]
    port = int(sys.argv[2])
    limit = float(sys.argv[3]) if len(sys.argv) > 3 else 600.0
    ready = await_ready(port, limit)
    result = measure(port, which, ready)
    out = f"/tmp/microbench/latency.{which}.json"
    with open(out, "w") as file:
        json.dump(result, file, indent=2)
    print(f"written to {out}")


if __name__ == "__main__":
    main()
