# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Latency of the nginx + gunicorn + Django stack, native against the module.

The same OCI image both ways: `docker run` on the left, `zaqaru run` on the
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


def cpu_ticks(pid: int) -> int:
    with open(f"/proc/{pid}/stat") as stat:
        fields = stat.read().rsplit(")", 1)[1].split()
    # utime and stime, in clock ticks, after the parenthesised command.
    return int(fields[11]) + int(fields[12])


def await_idle(pid: int, limit: float) -> float:
    """Seconds until the process has been busy and then gone quiet.

    For the module, whose boot is one process interpreting for as long as
    it takes. Readiness is *not* asked for with requests, and this is not
    fussiness: a request made while gunicorn's worker is still importing
    Django queues behind it and is served, in full, once the worker comes
    up -- so a poll every quarter second for thirty seconds becomes a
    hundred queued requests, the first 200 arrives only after most of them
    have been answered, and every timing taken afterwards is interleaved
    with the rest of the queue draining. The figures this tool used to
    report -- 24 s to the first 200 and 149 ms a request -- were that.
    """
    started = time.perf_counter()
    previous = cpu_ticks(pid)
    busy = False
    calm = 0
    while time.perf_counter() - started < limit:
        time.sleep(1.0)
        current = cpu_ticks(pid)
        used, previous = current - previous, current
        if used >= 40:
            busy, calm = True, 0
        elif busy:
            calm += 1
        if calm >= 2:
            # The two quiet seconds are not the boot's.
            return time.perf_counter() - started - 2.0
    raise SystemExit(f"process {pid} never went quiet within {limit}s")


def await_ready(port: int, limit: float) -> float:
    """Seconds until the stack answers 200, by asking it.

    For the native container, where a request costs a third of a
    millisecond and asking every couple of seconds queues nothing worth
    counting. Not until it answers *something*: nginx comes up before
    gunicorn does and says 502 in the meantime, which is correct of it and
    is not the stack being ready.
    """
    started = time.perf_counter()
    while time.perf_counter() - started < limit:
        try:
            status, _, _ = fetch(port, timeout=2.0)
            if status == 200:
                return time.perf_counter() - started
        except Exception:
            pass
        time.sleep(2.0)
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
    # A pid means "wait for it to go quiet" rather than "ask it".
    pid = int(sys.argv[4]) if len(sys.argv) > 4 else None
    ready = await_idle(pid, limit) if pid else await_ready(port, limit)
    status, _, _ = fetch(port)
    if status != 200:
        raise SystemExit(f"{which}: quiet, but answered {status} rather than 200")
    result = measure(port, which, ready)
    out = f"/tmp/microbench/latency.{which}.json"
    with open(out, "w") as file:
        json.dump(result, file, indent=2)
    print(f"written to {out}")


if __name__ == "__main__":
    main()
