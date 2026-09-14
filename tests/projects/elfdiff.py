#!/usr/bin/env python3
"""Compare the dynamic linking view of two ELF files (GNU ld vs qld).

Usage: elfdiff.py REFERENCE CANDIDATE [--sections] [--relocs]

Prints the lines that differ, as `-` (reference) and `+` (candidate), for:
  dynsym:  name@version, type, binding, visibility, UND/ABS/DEF
  dynamic: DT_* tags, with values for the non-address tags
  reloc:   dynamic relocation type and symbol, with a count (--relocs)
  section: name, type and flags, not addresses (--sections)
It follows the normalization of tests/differential.rs (see tests/README.md).
Exit status 0 when nothing differs, 1 otherwise.
"""

import collections
import re
import subprocess
import sys

VALUE_TAGS = {
    "NEEDED", "SONAME", "RPATH", "RUNPATH", "FLAGS", "FLAGS_1", "BIND_NOW",
    "TEXTREL", "SYMBOLIC", "PLTREL", "VERDEFNUM", "VERNEEDNUM", "RELACOUNT",
    "RELCOUNT", "AUXILIARY", "FILTER",
}


def readelf(args, path):
    return subprocess.run(
        ["readelf", "-W", *args, path], capture_output=True, text=True, check=False
    ).stdout


def dynsym(path):
    out = collections.Counter()
    in_dynsym = False
    for line in readelf(["--dyn-syms"], path).splitlines():
        if line.startswith("Symbol table '.dynsym'"):
            in_dynsym = True
            continue
        if not in_dynsym:
            continue
        f = line.split()
        if len(f) < 8 or not f[0].rstrip(":").isdigit():
            continue
        name = f[7] if len(f) > 7 else ""
        if name == "":
            continue
        # readelf may append " (N)" version indices for hidden versions.
        name = re.sub(r" \(\d+\)$", "", " ".join(f[7:]))
        ndx = f[6]
        where = ndx if ndx in ("UND", "ABS", "COM") else "DEF"
        out[f"dynsym: {name} {f[3]} {f[4]} {f[5]} {where}"] += 1
    return out


def dynamic(path):
    out = collections.Counter()
    for line in readelf(["-d"], path).splitlines():
        m = re.match(r"\s*0x[0-9a-f]+\s+\((\w+)\)\s+(.*)", line)
        if not m:
            continue
        tag, value = m.group(1), m.group(2).strip()
        if tag in VALUE_TAGS:
            if tag in ("RELACOUNT", "RELCOUNT"):
                value = ""
            out[f"dynamic: {tag} {value}".rstrip()] += 1
        else:
            out[f"dynamic: {tag}"] += 1
    return out


def relocs(path):
    out = collections.Counter()
    section = None
    for line in readelf(["-r"], path).splitlines():
        m = re.match(r"Relocation section '([^']+)'", line)
        if m:
            section = m.group(1)
            continue
        f = line.split()
        if section and len(f) >= 3 and re.match(r"^[0-9a-f]{8,16}$", f[0]):
            sym = f[4] if len(f) >= 5 and not f[3].startswith("0") else ""
            if len(f) >= 5 and re.match(r"^[0-9a-f]+$", f[3]):
                sym = f[4]
            out[f"reloc: {section} {f[2]} {sym}".rstrip()] += 1
    return out


def sections(path):
    out = collections.Counter()
    for line in readelf(["-S"], path).splitlines():
        m = re.match(r"\s*\[\s*\d+\]\s+(\S+)\s+(\S+)\s+\S+\s+\S+\s+\S+\s+\S+\s+(\S*)", line)
        if m and m.group(1) != "NULL":
            flags = m.group(3) if not m.group(3).isdigit() else ""
            out[f"section: {m.group(1)} {m.group(2)} {flags}".rstrip()] += 1
    return out


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if len(args) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    kinds = [dynsym, dynamic]
    if "--relocs" in sys.argv:
        kinds.append(relocs)
    if "--sections" in sys.argv:
        kinds.append(sections)
    ref, cand = args
    differs = False
    for kind in kinds:
        a, b = kind(ref), kind(cand)
        for key in sorted(set(a) | set(b)):
            if a[key] != b[key]:
                differs = True
                if a[key]:
                    print(f"- {key}" + (f" x{a[key]}" if a[key] > 1 else ""))
                if b[key]:
                    print(f"+ {key}" + (f" x{b[key]}" if b[key] > 1 else ""))
    return 1 if differs else 0


if __name__ == "__main__":
    sys.exit(main())
