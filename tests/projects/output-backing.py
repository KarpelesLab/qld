#!/usr/bin/env python3
"""Time qld's output backings (QLD_OUTPUT_BACKING) on large real links.

Usage:
  output-backing.py QLD --outdir DIR [--outdir DIR2 ...] [--runs N]
                    [--threads 1,16,default] [--backings mmap,write,memory]
                    [--qld-args ARGS] [--workdir DIR] WORKLOAD...

WORKLOAD is one of:
  NAME=LINKS_LOG:OUTPUT   a link logged by the `ld` wrapper from common.sh
                          (see linktime.py), replayed with its `-o` pointed
                          into each --outdir
  NAME=synthetic:MIB      about MIB MiB of .debug_info/.debug_line in 256
                          assembled objects (built once in --workdir)

Every (outdir, workload, threads) cell links each backing N times (default
5), interleaved, and reports the minimum and median wall time. Before each
run the previous output is deleted and the file systems synced, outside the
timing. The last output of every backing is compared byte for byte; any
difference fails the script. The load average is printed before and after,
with the file system type of each directory (`df -T`).
"""

import argparse
import hashlib
import importlib.util
import os
import shlex
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))


def load_linktime():
    spec = importlib.util.spec_from_file_location("linktime", os.path.join(HERE, "linktime.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def fs_type(path):
    out = subprocess.run(["df", "-T", path], capture_output=True, text=True).stdout
    lines = out.strip().splitlines()
    return lines[-1].split()[1] if len(lines) > 1 else "?"


def synthetic(workdir, mib):
    """Assembles 256 objects totalling about `mib` MiB of debug sections."""
    count = 256
    per = max(1, mib * (1 << 20) // count)
    info, line = per * 4 // 5, per // 5
    base = os.path.join(workdir, f"synthetic-{mib}")
    stamp = os.path.join(base, "done")
    objects = [os.path.join(base, f"o{i}.o") for i in range(count)]
    if not os.path.exists(stamp):
        os.makedirs(base, exist_ok=True)
        for i, obj in enumerate(objects):
            src = obj[:-2] + ".s"
            with open(src, "w") as f:
                if i == 0:
                    f.write(".globl _start\n.text\n_start:\n ret\n")
                f.write(f".text\n.globl f{i}\nf{i}:\n .fill 64, 1, 0x90\n ret\n")
                f.write(f'.section .debug_info,"",@progbits\n .quad f{i}\n')
                f.write(f" .fill {info}, 1, {(i * 7 + 1) & 0xff}\n")
                f.write(f'.section .debug_line,"",@progbits\n .fill {line}, 1, {(i * 3 + 5) & 0xff}\n')
            subprocess.run(["as", "-o", obj, src], check=True)
            os.remove(src)
        open(stamp, "w").close()
    return base, ["-o", "OUTPUT", "-e", "_start", *objects], 1


def logged(spec):
    log, _, output = spec.partition(":")
    found = load_linktime().find_link(log, output)
    if not found:
        raise SystemExit(f"no link of {output} in {log}")
    cwd, args, at = found
    return cwd, args, at


def run_once(qld, cwd, args, env):
    start = time.monotonic()
    result = subprocess.run([qld, *args], cwd=cwd, env=env, capture_output=True)
    elapsed = time.monotonic() - start
    if result.returncode != 0:
        sys.stderr.write(result.stderr.decode(errors="replace"))
        raise SystemExit(f"link failed: {shlex.join([qld, *args])}")
    return elapsed


def digest(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(1 << 24):
            h.update(chunk)
    return h.hexdigest()


def main():
    p = argparse.ArgumentParser()
    p.add_argument("qld")
    p.add_argument("workloads", nargs="+")
    p.add_argument("--outdir", action="append", required=True)
    p.add_argument("--runs", type=int, default=5)
    p.add_argument("--threads", default="1,16,default")
    p.add_argument("--backings", default="mmap,write,memory")
    p.add_argument("--qld-args", default="")
    p.add_argument("--workdir", default=None)
    o = p.parse_args()
    qld = os.path.abspath(o.qld)
    backings = o.backings.split(",")
    threads = o.threads.split(",")
    extra = shlex.split(o.qld_args)

    print(f"load average before: {os.getloadavg()}")
    print("| file system | workload | size | threads | " + " | ".join(
        f"{b} min / median" for b in backings) + " |")
    print("| --- | --- | --- | --- | " + " | ".join("---" for _ in backings) + " |")
    failed = False
    for outdir in o.outdir:
        os.makedirs(outdir, exist_ok=True)
        fstype = fs_type(outdir)
        for workload in o.workloads:
            name, _, spec = workload.partition("=")
            if spec.startswith("synthetic:"):
                workdir = o.workdir or outdir
                cwd, args, at = synthetic(workdir, int(spec.split(":", 1)[1]))
            else:
                cwd, args, at = logged(spec)
            out = os.path.join(outdir, f"qld-backing-{name}.out")
            args = list(args)
            args[at] = out
            args = [a for a in args if a != "--color-diagnostics"] + extra
            for t in threads:
                targs = args + ([] if t == "default" else [f"--threads={t}"])
                times = {b: [] for b in backings}
                sums = {}
                for _ in range(o.runs):
                    for b in backings:
                        if os.path.exists(out):
                            os.remove(out)
                        os.sync()
                        env = dict(os.environ, QLD_OUTPUT_BACKING=b)
                        times[b].append(run_once(qld, cwd, targs, env))
                        if _ == o.runs - 1:
                            sums[b] = digest(out)
                size = os.path.getsize(out)
                os.remove(out)
                cells = " | ".join(
                    f"{min(times[b]) * 1000:.0f} / {statistics.median(times[b]) * 1000:.0f} ms"
                    for b in backings)
                same = len(set(sums.values())) == 1
                failed |= not same
                print(f"| {fstype} | {name} | {size / (1 << 20):.0f} MiB | {t} | {cells} |"
                      + ("" if same else " OUTPUTS DIFFER |"), flush=True)
    print(f"load average after: {os.getloadavg()}")
    if failed:
        raise SystemExit("outputs differ between backings")


if __name__ == "__main__":
    main()
