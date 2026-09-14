#!/usr/bin/env python3
"""Compare where two links put their dynamic relocations, by symbol.

Usage: relocdiff.py REFERENCE CANDIDATE [TYPE]

Addresses differ between linkers, so each dynamic relocation's offset is
named by the defined symbol (from .symtab) that contains it, as
`type symbol+delta`, and the two multisets are compared. TYPE restricts the
comparison to one relocation type, such as R_X86_64_RELATIVE. Prints the
differences as `-` (reference) and `+` (candidate) lines.
"""

import bisect
import collections
import re
import subprocess
import sys


def run(args):
    return subprocess.run(args, capture_output=True, text=True, check=False).stdout


def symbols(path):
    syms = []
    for line in run(["readelf", "-W", "-s", path]).splitlines():
        f = line.split()
        if len(f) < 8 or not f[0].rstrip(":").isdigit():
            continue
        if f[6] in ("UND", "ABS") or f[3] in ("SECTION", "FILE") or not f[7:]:
            continue
        value, size = int(f[1], 16), int(f[2], 0) if f[2].startswith("0x") else int(f[2])
        if size == 0:
            continue
        syms.append((value, size, f[7]))
    syms.sort()
    return syms


def sections(path):
    out = []
    for line in run(["readelf", "-W", "-S", path]).splitlines():
        m = re.match(r"\s*\[\s*\d+\]\s+(\S+)\s+\S+\s+([0-9a-f]+)\s+[0-9a-f]+\s+([0-9a-f]+)", line)
        if m and int(m.group(2), 16):
            out.append((int(m.group(2), 16), int(m.group(3), 16), m.group(1)))
    out.sort()
    return out


def name(addr, syms, starts, secs):
    i = bisect.bisect_right(starts, addr) - 1
    if i >= 0:
        value, size, sym = syms[i]
        if value <= addr < value + size:
            return f"{sym}+{addr - value:#x}"
    for start, size, sec in secs:
        if start <= addr < start + size:
            # Not inside a sized symbol: name the nearest symbol before it
            # in the same section.
            if i >= 0 and syms[i][0] >= start:
                return f"{sec}:after:{syms[i][2]}"
            return f"{sec}+?"
    return "?"


def relocs(path, only):
    syms = symbols(path)
    starts = [s[0] for s in syms]
    secs = sections(path)
    out = collections.Counter()
    for line in run(["readelf", "-W", "-r", path]).splitlines():
        f = line.split()
        if len(f) < 3 or not re.match(r"^[0-9a-f]{8,16}$", f[0]):
            continue
        if only and f[2] != only:
            continue
        target = f[4] if len(f) > 4 and not f[3].startswith(("+", "-")) else ""
        out[f"{f[2]} {name(int(f[0], 16), syms, starts, secs)} {target}".rstrip()] += 1
    return out


def main():
    if len(sys.argv) < 3:
        print(__doc__, file=sys.stderr)
        return 2
    only = sys.argv[3] if len(sys.argv) > 3 else None
    a, b = relocs(sys.argv[1], only), relocs(sys.argv[2], only)
    differs = 0
    for key in sorted(set(a) | set(b)):
        if a[key] != b[key]:
            differs += 1
            print(f"- {key} x{a[key]}\n+ {key} x{b[key]}")
    return 1 if differs else 0


if __name__ == "__main__":
    sys.exit(main())
