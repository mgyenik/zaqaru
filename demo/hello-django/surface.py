"""Turn an `strace -f` of the demo stack into the traced syscall surface and worklist.

Split out of `trace.sh` so the extraction can be re-run over a trace that
already exists — which is most of the time, because tracing costs a docker
run and extracting costs milliseconds.
"""

import collections
import pathlib
import re
import sys

CALL = re.compile(r"^(\d+)\s+([a-z_0-9]+)\(")
SIGNAL = re.compile(r"^(\d+)\s+--- (SIG\w+)")


def main(trace_path: str, repo: str) -> None:
    trace = pathlib.Path(trace_path).read_text(errors="replace").splitlines()
    counts: collections.Counter = collections.Counter()
    processes = set()
    signals: collections.Counter = collections.Counter()
    for line in trace:
        if call := CALL.match(line):
            counts[call.group(2)] += 1
            processes.add(call.group(1))
        elif signal := SIGNAL.match(line):
            signals[signal.group(2)] += 1

    # What the kernel *names* is what it dispatches: an unnamed number reaches the
    # loud-error path, so the name table is the honest list of rows.
    source = pathlib.Path(repo, "kernel/src/syscall.rs").read_text()
    table = source[source.index("pub fn name(number: i64)") :]
    named = set(re.findall(r'=> "([a-z_0-9]+)"', table[: table.index("\n    }\n")]))
    missing = sorted(set(counts) - named, key=lambda name: (-counts[name], name))

    lines = [
        "# The traced syscall surface of nginx + gunicorn + django",
        "#",
        "# The image is `demo/hello-django`;",
        "# the trace is `strace -f -yy` over the whole process tree from boot,",
        "# through two `curl` requests, to a `SIGTERM` shutdown every process",
        "# exits 0 from. Regenerate with `demo/hello-django/trace.sh`.",
        "#",
        "# This is the worklist, and it is the baseline the interpreted run is",
        "# diffed against.",
        "#",
        f"# processes: {len(processes)}   distinct syscalls: {len(counts)}"
        f"   calls: {sum(counts.values())}",
        "# signals delivered: "
        + ", ".join(f"{name}×{n}" for name, n in sorted(signals.items())),
        "",
        "[surface]",
        "",
    ]
    lines += [
        f"{n:8d}  {name:<20} {'row' if name in named else 'MISSING'}"
        for name, n in counts.most_common()
    ]
    lines += ["", f"[missing]  {len(missing)} rows the stack calls and the kernel has none for", ""]
    lines += [f"{counts[name]:8d}  {name}" for name in missing]

    out = pathlib.Path(repo, "demo/hello-django/baseline/native-surface.txt")
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text("\n".join(lines) + "\n")
    print(
        f"{len(processes)} processes, {len(counts)} syscalls, "
        f"{sum(counts.values())} calls; {len(missing)} rows missing -> {out}"
    )


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
