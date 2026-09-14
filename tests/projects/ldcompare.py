#!/usr/bin/env python3
"""Link with qld, then with GNU ld from the same inputs, and record both.

Usage: ldcompare.py QLD ARGS...   (called by the `ld` wrapper of common.sh
when QLD_LINK_COMPARE is set to a directory)

Runs qld with ARGS as the real link and exits with its status. When the
output's size is at least QLD_LINK_COMPARE_MIN bytes (default 1 MiB), it
then links the same inputs with GNU ld (`ld.bfd`, or QLD_LINK_COMPARE_GNU)
into QLD_LINK_COMPARE, appends a tab-separated line to compare.tsv there
(output, qld seconds, GNU ld seconds, qld bytes, GNU ld bytes) and the
elfdiff.py differences to elfdiff.log, and deletes the GNU ld output. This
times links whose inputs are temporary (rustc's), which cannot be replayed.
"""

import os
import shlex
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))


def expand(args):
    out = []
    for a in args:
        if a.startswith("@") and os.path.isfile(a[1:]):
            with open(a[1:], encoding="utf-8", errors="surrogateescape") as f:
                out.extend(expand(shlex.split(f.read())))
        else:
            out.append(a)
    return out


def main():
    qld, args = sys.argv[1], sys.argv[2:]
    start = time.monotonic()
    status = subprocess.run([qld, *args]).returncode
    tq = time.monotonic() - start
    target = os.environ.get("QLD_LINK_COMPARE")
    if status != 0 or not target:
        return status
    full = expand(args)
    if "-o" not in full or "-r" in full or "--relocatable" in full:
        return status
    at = full.index("-o") + 1
    if at >= len(full):
        return status
    output = full[at]
    try:
        sq = os.path.getsize(output)
    except OSError:
        return status
    if sq < int(os.environ.get("QLD_LINK_COMPARE_MIN", str(1 << 20))):
        return status
    os.makedirs(target, exist_ok=True)
    gnu = os.environ.get("QLD_LINK_COMPARE_GNU", "ld.bfd")
    with tempfile.TemporaryDirectory(dir=target) as tmp:
        gnu_out = os.path.join(tmp, "out")
        gargs = [a for a in full if a != "--color-diagnostics"]
        gargs[at] = gnu_out
        start = time.monotonic()
        result = subprocess.run([gnu, *gargs], capture_output=True)
        tg = time.monotonic() - start
        if result.returncode != 0:
            with open(os.path.join(target, "gnu-failures.log"), "a") as f:
                f.write(f"{output}\n{result.stderr.decode(errors='replace')}\n")
            return status
        sg = os.path.getsize(gnu_out)
        diff = subprocess.run(
            [sys.executable, os.path.join(HERE, "elfdiff.py"), gnu_out, output],
            capture_output=True,
            text=True,
        ).stdout
    with open(os.path.join(target, "compare.tsv"), "a") as f:
        f.write(f"{output}\t{tq:.3f}\t{tg:.3f}\t{sq}\t{sg}\n")
    if diff:
        with open(os.path.join(target, "elfdiff.log"), "a") as f:
            f.write(f"== {output}\n{diff}")
    return status


if __name__ == "__main__":
    sys.exit(main())
