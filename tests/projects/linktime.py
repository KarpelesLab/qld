#!/usr/bin/env python3
"""Replay a logged link with qld and GNU ld; compare wall time and size.

Usage: linktime.py LINKS_LOG OUTPUT QLD [--runs N] [--gnu LD] [--qld-args ARGS]

LINKS_LOG is a links.log written by the `ld` wrapper from common.sh (one
`cd 'DIR' && 'arg' 'arg' ...` line per link; response files were copied
next to the log). OUTPUT is the link's -o value as logged. The fastest of
N runs (default 3) is reported for each linker. Outputs are written to a
temporary directory and deleted, unless --keep names a directory to
keep them in (as OUTPUT-basename.qld and .gnu, for elfdiff.py).
"""

import argparse
import os
import shlex
import subprocess
import sys
import tempfile
import time


def expand(args):
    out = []
    for a in args:
        if a.startswith("@") and os.path.isfile(a[1:]):
            with open(a[1:], encoding="utf-8", errors="surrogateescape") as f:
                out.extend(expand(shlex.split(f.read())))
        else:
            out.append(a)
    return out


def find_link(log, output):
    found = None
    with open(log, encoding="utf-8", errors="surrogateescape") as f:
        for line in f:
            if not line.startswith("cd "):
                continue
            head, _, rest = line.partition(" && ")
            cwd = shlex.split(head)[1]
            if not os.path.isdir(cwd):
                continue
            args = expand(shlex.split(rest))
            for i, a in enumerate(args[:-1]):
                if a == "-o" and args[i + 1] == output:
                    found = (cwd, args, i + 1)
    return found


def best(linker, cwd, args, runs):
    times = []
    for _ in range(runs):
        start = time.monotonic()
        result = subprocess.run([linker, *args], cwd=cwd, capture_output=True)
        times.append(time.monotonic() - start)
        if result.returncode != 0:
            sys.stderr.write(result.stderr.decode(errors="replace"))
            raise SystemExit(f"{linker} failed")
    return min(times)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("log")
    p.add_argument("output")
    p.add_argument("qld")
    p.add_argument("--runs", type=int, default=3)
    p.add_argument("--gnu", default="ld.bfd")
    p.add_argument("--qld-args", default="")
    p.add_argument("--keep", help="copy both outputs into this directory")
    o = p.parse_args()
    found = find_link(o.log, o.output)
    if not found:
        raise SystemExit(f"no link of {o.output} in {o.log}")
    cwd, args, at = found
    if o.keep:
        os.makedirs(o.keep, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=o.keep) as tmp:
        qargs = list(args)
        qargs[at] = os.path.join(tmp, "out.qld")
        qargs += shlex.split(o.qld_args)
        gargs = [a for a in args if a != "--color-diagnostics"]
        gargs[gargs.index(o.output)] = os.path.join(tmp, "out.gnu")
        tq = best(o.qld, cwd, qargs, o.runs)
        tg = best(o.gnu, cwd, gargs, o.runs)
        sq = os.path.getsize(os.path.join(tmp, "out.qld"))
        sg = os.path.getsize(os.path.join(tmp, "out.gnu"))
        if o.keep:
            base = os.path.basename(o.output)
            for kind in ("qld", "gnu"):
                os.replace(os.path.join(tmp, f"out.{kind}"), os.path.join(o.keep, f"{base}.{kind}"))
    print(f"| {o.output} | {tq:.2f} s | {tg:.2f} s | {tg / tq:.1f}x | {sq} | {sg} |")


if __name__ == "__main__":
    main()
