#!/usr/bin/env python3
"""Compare the sections GNU ld and qld remove with --gc-sections.

Usage: gcdiff.py LINKS_LOG OUTPUT QLD [--gnu LD] [--filter REGEX]

Replays the logged link of OUTPUT (see linktime.py) with
--print-gc-sections under both linkers, into a temporary directory, and
prints the sections only one of them removed: `-` removed by GNU ld only,
`+` removed by qld only.
"""

import argparse
import os
import re
import subprocess
import tempfile

from linktime import find_link


def removed(linker, cwd, args):
    result = subprocess.run([linker, *args, "--print-gc-sections"], cwd=cwd, capture_output=True, text=True)
    out = set()
    for line in result.stderr.splitlines():
        m = re.search(r"removing unused section '([^']*)' in file '([^']*)'", line)
        if m:
            # GNU ld appends the group signature: `.text.f[f]`.
            section = re.sub(r"\[[^]]*\]$", "", m.group(1))
            out.add((os.path.normpath(os.path.join(cwd, m.group(2))), section))
    return out, result.returncode


def main():
    p = argparse.ArgumentParser()
    p.add_argument("log")
    p.add_argument("output")
    p.add_argument("qld")
    p.add_argument("--gnu", default="ld.bfd")
    p.add_argument("--filter", default="")
    o = p.parse_args()
    found = find_link(o.log, o.output)
    if not found:
        raise SystemExit(f"no link of {o.output} in {o.log}")
    cwd, args, at = found
    with tempfile.TemporaryDirectory() as tmp:
        qargs = list(args)
        qargs[at] = os.path.join(tmp, "out.qld")
        gargs = list(args)
        gargs[at] = os.path.join(tmp, "out.gnu")
        gargs = [a for a in gargs if a != "--color-diagnostics"]
        q, qs = removed(o.qld, cwd, qargs)
        g, gs = removed(o.gnu, cwd, gargs)
    rx = re.compile(o.filter)
    print(f"GNU ld removed {len(g)} (exit {gs}), qld removed {len(q)} (exit {qs})")
    for file, sec in sorted(g - q):
        if rx.search(sec):
            print(f"- {file}({sec})")
    for file, sec in sorted(q - g):
        if rx.search(sec):
            print(f"+ {file}({sec})")


if __name__ == "__main__":
    main()
