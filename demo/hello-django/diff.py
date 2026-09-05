"""Diff an interpreted syscall trace against N0's native baseline.

N5's acceptance, and V2's long-outstanding one: *the same stack, traced
both ways, and the difference accounted for.*

Not a line-by-line diff, which would be noise — addresses differ, orders
differ, and two correct kernels interleave five processes differently on
purpose. What is comparable is the **surface**: which calls each run makes
at all, and roughly how often. A call the native run makes and the
interpreted one does not is either work we avoided or work we are missing,
and the difference between those two is a judgement somebody has to write
down. A call only the interpreted run makes is something this kernel adds.
"""

import collections
import pathlib
import re
import sys

# `strace -f`: "1234  name(args) = result".
NATIVE = re.compile(r"^(\d+)\s+([a-z_0-9]+)\(")
# the kernel's own renderer: "[1] name(args) = result".
INTERPRETED = re.compile(r"^\[(\d+)\]\s+([a-z_0-9]+)\(")


def surface(path: str, pattern: re.Pattern) -> collections.Counter:
    counts: collections.Counter = collections.Counter()
    for line in pathlib.Path(path).read_text(errors="replace").splitlines():
        if found := pattern.match(line):
            counts[found.group(2)] += 1
    return counts


def main(native_path: str, interpreted_path: str) -> None:
    native = surface(native_path, NATIVE)
    interpreted = surface(interpreted_path, INTERPRETED)

    print(f"native:      {len(native):3d} syscalls, {sum(native.values()):6d} calls")
    print(f"interpreted: {len(interpreted):3d} syscalls, {sum(interpreted.values()):6d} calls")

    missing = sorted(set(native) - set(interpreted), key=lambda name: -native[name])
    added = sorted(set(interpreted) - set(native), key=lambda name: -interpreted[name])

    print(f"\n[only native]  {len(missing)} — work the interpreted run did not do")
    for name in missing:
        print(f"{native[name]:8d}  {name}")

    print(f"\n[only interpreted]  {len(added)} — work it does instead")
    for name in added:
        print(f"{interpreted[name]:8d}  {name}")

    # Shared calls whose counts differ by more than a factor of two *and* by
    # more than a handful, which is where a real behavioural difference
    # hides. A ratio alone flags every call made twice instead of once.
    print("\n[both, but far apart]")
    for name in sorted(set(native) & set(interpreted)):
        there, here = native[name], interpreted[name]
        if abs(there - here) < 20:
            continue
        if max(there, here) < 2 * min(there, here):
            continue
        print(f"{there:8d} -> {here:8d}  {name}")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
