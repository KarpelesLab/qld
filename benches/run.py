#!/usr/bin/env python3
"""Replay captured links (see capture.py) with several linkers and time them.

Usage:
  run.py SPECS [NAME...] [--linker NAME=PATH ...] [--threads default,1,8,64]
         [--runs N] [--outdir DIR] [--json FILE] [--rusage-runs N]
         [--only LINKER,...] [--qld-args ARGS]
  run.py SPECS [NAME...] --determinism QLD [--det-threads 1,2,8,64]
         [--hashes FILE]
  run.py SPECS [NAME...] --laps [--linker NAME=PATH ...] [--only qld,...]
         [--threads 16] [--runs N] [--perf PERF]

For every spec and every (linker, thread count) configuration, each link
is run N times (default 5). The configurations are interleaved run by run
(run 1 of every configuration, then run 2, ...), so a change in machine
load affects all of them alike. Before each run the previous output is
deleted and the file system synced, outside the timing.

Each run records wall time, the load average (1 minute) just before it,
and, through wait4(), user+system CPU time and peak RSS of the linker
process. mold, wild and qld fork by default and return as soon as the
output is written, leaving the child to clean up; the child's CPU time and
RSS are not visible to wait4(), so for those linkers CPU and RSS come from
a separate `--no-fork` run (--rusage-runs, default 1) while wall time is
taken in their default mode. A linker name that starts with one of theirs
(`qld-base`, given with --linker and --only) counts as forking too.

After the timed runs, the output of every configuration is checked with
the spec's smoke command (`{out}` is replaced by the output path); a
configuration whose output fails it is reported as BROKEN.

--determinism QLD links every spec with QLD at 1, 2, 8 and 64 threads
(--det-threads) and checks that the outputs are byte-identical (no timing).
With --hashes FILE, the output hashes are also compared with FILE, or
written to it if it does not exist: that is how an optimization is checked
to leave every output unchanged.

--laps runs only qld-like linkers (names starting with "qld"), with
`--no-fork` and QLD_TIMING=1, interleaved like the timed runs, and reports
per configuration the minimum and median of each stage lap (the stage's
own time, from the cumulative `qld: STAGE: N ms` lines), of any
`qld-lap: NAME: N ms` line (a duration printed by instrumentation), of
wall and CPU time, and, with --perf PATH, the user-space instruction
count from `perf stat`. Stage laps and instruction counts vary much less
than wall time on a loaded machine.

Linker defaults: gnu=ld.bfd, lld, mold and wild from ~/.cache/qld-bench/tools
and the static LLVM tree, qld=target/release/qld. Thread flags: GNU ld has
none (only "default" is run); the others take `--threads=N`.
"""

import argparse
import hashlib
import json
import os
import re
import shlex
import statistics
import subprocess
import sys
import time

HOME = os.path.expanduser("~")
HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_LINKERS = {
    "gnu": "ld.bfd",
    "lld": f"{HOME}/.cache/qld-projects/build/llvm-23.1.1-static/bin/ld.lld",
    "mold": f"{HOME}/.cache/qld-bench/tools/mold/bin/mold",
    "wild": f"{HOME}/.cache/qld-bench/tools/wild/bin/wild",
    "qld": os.path.join(os.path.dirname(HERE), "target/release/qld"),
}
FORKING = ("mold", "wild", "qld")
# Options a replayed argv may carry that only change diagnostics or checks
# (not the output), and that some linkers reject.
STRIP = {"--color-diagnostics"}
STRIP_FOR = {
    "lld": {"--no-warn-rwx-segments"},
    "mold": {"--no-warn-rwx-segments", "--discard-none", "--orphan-handling=error"},
    "wild": {"--no-warn-rwx-segments", "--orphan-handling=error", "--discard-none"},
}


def load_specs(root, names):
    specs = []
    for name in names or sorted(os.listdir(root)):
        path = os.path.join(root, name, "link.json")
        if os.path.exists(path):
            with open(path) as f:
                specs.append(json.load(f))
        elif names:
            raise SystemExit(f"no spec {path}")
    return specs


def argv_for(spec, kind, linker, threads, out, extra=()):
    args = list(spec["args"])
    args[spec["output_index"]] = out
    strip = STRIP | STRIP_FOR.get(kind, set())
    args = [a for a in args if a not in strip]
    if threads != "default":
        args.append(f"--threads={threads}")
    return [linker, *args, *extra]


def run_one(argv, cwd, env):
    load = os.getloadavg()[0]
    start = time.monotonic()
    proc = subprocess.Popen(argv, cwd=cwd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    stderr = proc.stderr.read()
    _, status, usage = os.wait4(proc.pid, 0)
    wall = time.monotonic() - start
    proc.returncode = os.waitstatus_to_exitcode(status)
    return {
        "ok": proc.returncode == 0,
        "stderr": stderr.decode(errors="replace"),
        "wall": wall,
        "cpu": usage.ru_utime + usage.ru_stime,
        "sys": usage.ru_stime,
        "minflt": usage.ru_minflt,
        "rss_kib": usage.ru_maxrss,
        "load": load,
    }


def remove(path):
    try:
        os.remove(path)
    except FileNotFoundError:
        pass


def digest(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(1 << 24):
            h.update(chunk)
    return h.hexdigest()


def smoke(spec, out, env):
    cmd = spec["smoke"].replace("{out}", shlex.quote(out))
    r = subprocess.run(cmd, shell=True, cwd=spec["cwd"], env=env, capture_output=True, timeout=300)
    return r.returncode == 0


def fmt_ms(s):
    return f"{s * 1000:.0f}" if s < 10 else f"{s:.1f}s"


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("specs")
    p.add_argument("names", nargs="*")
    p.add_argument("--linker", action="append", default=[])
    p.add_argument("--only", default="gnu,lld,mold,wild,qld")
    p.add_argument("--threads", default="default,1,8,64")
    p.add_argument("--runs", type=int, default=5)
    p.add_argument("--rusage-runs", type=int, default=1)
    p.add_argument("--outdir", default=f"{HOME}/.cache/qld-bench/out")
    p.add_argument("--json")
    p.add_argument("--determinism", metavar="QLD")
    p.add_argument("--det-threads", default="1,2,8,64")
    p.add_argument("--hashes", help="with --determinism: compare with (or, if absent, write) this file")
    p.add_argument("--qld-args", default="")
    p.add_argument("--laps", action="store_true")
    p.add_argument("--perf", help="with --laps: perf binary, to count instructions")
    o = p.parse_args()

    linkers = dict(DEFAULT_LINKERS)
    for kv in o.linker:
        k, _, v = kv.partition("=")
        linkers[k] = v
    specs = load_specs(o.specs, o.names)
    os.makedirs(o.outdir, exist_ok=True)

    if o.determinism:
        failed = False
        expected = {}
        if o.hashes and os.path.exists(o.hashes):
            with open(o.hashes) as f:
                expected = json.load(f)
        found = {}
        print("| benchmark | threads 1 / 2 / 8 / 64 | sha256 | vs --hashes |\n| --- | --- | --- | --- |")
        for spec in specs:
            env = dict(os.environ, **spec.get("env", {}))
            sums = []
            for t in o.det_threads.split(","):
                out = os.path.join(o.outdir, f"{spec['name']}.det{t}")
                remove(out)
                r = run_one(argv_for(spec, "qld", o.determinism, t, out, shlex.split(o.qld_args)), spec["cwd"], env)
                if not r["ok"]:
                    sys.stderr.write(r["stderr"][-2000:])
                    raise SystemExit(f"{spec['name']}: qld failed at {t} threads")
                sums.append(digest(out))
                remove(out)
            same = len(set(sums)) == 1
            found[spec["name"]] = sums[0]
            want = expected.get(spec["name"])
            versus = "-" if want is None else ("same" if want == sums[0] else "CHANGED")
            failed |= not same or versus == "CHANGED"
            print(f"| {spec['name']} | {'identical' if same else 'DIFFER'} | {sums[0][:16]} | {versus} |",
                  flush=True)
        if o.hashes and not expected:
            with open(o.hashes, "w") as f:
                json.dump(found, f, indent=1)
        return 1 if failed else 0

    if o.laps:
        return laps(o, linkers, specs)

    wanted = o.only.split(",")
    threads = o.threads.split(",")
    results = []
    print(f"load average before: {os.getloadavg()}", flush=True)
    for spec in specs:
        env = dict(os.environ, **spec.get("env", {}))
        configs = []
        for kind in wanted:
            for t in threads:
                if kind == "gnu" and t != "default":
                    continue
                configs.append((kind, t))
        cells = {c: {"runs": [], "rusage": [], "broken": None} for c in configs}

        def out_of(c):
            return os.path.join(o.outdir, f"{spec['name']}.{c[0]}.{c[1]}")

        extra = {c: (shlex.split(o.qld_args) if c[0].startswith("qld") else []) for c in configs}
        for i in range(o.runs):
            for c in configs:
                cell = cells[c]
                if cell["broken"]:
                    continue
                out = out_of(c)
                remove(out)
                os.sync()
                r = run_one(argv_for(spec, c[0], linkers[c[0]], c[1], out, extra[c]), spec["cwd"], env)
                if not r["ok"]:
                    cell["broken"] = "link failed: " + r["stderr"].strip().splitlines()[-1] if r["stderr"].strip() else "link failed"
                    continue
                cell["runs"].append(r)
        for c in configs:
            cell = cells[c]
            if cell["broken"]:
                continue
            out = out_of(c)
            cell["size"] = os.path.getsize(out)
            if not smoke(spec, out, env):
                cell["broken"] = "smoke check failed"
            if c[0].startswith(FORKING):
                for _ in range(o.rusage_runs):
                    remove(out)
                    os.sync()
                    r = run_one(argv_for(spec, c[0], linkers[c[0]], c[1], out, ["--no-fork"]), spec["cwd"], env)
                    if r["ok"]:
                        cell["rusage"].append(r)
            remove(out)

        print(f"\n### {spec['name']}\n")
        print("| linker | threads | wall min | wall median | CPU (median) | peak RSS | output | load avg |")
        print("| --- | --- | --- | --- | --- | --- | --- | --- |")
        for c in configs:
            cell = cells[c]
            row = {"bench": spec["name"], "linker": c[0], "threads": c[1]}
            if cell["broken"]:
                row["broken"] = cell["broken"]
                print(f"| {c[0]} | {c[1]} | BROKEN: {cell['broken'][:80]} | | | | | |")
            else:
                walls = [r["wall"] for r in cell["runs"]]
                usage = cell["rusage"] if c[0].startswith(FORKING) else cell["runs"]
                cpu = statistics.median(r["cpu"] for r in usage) if usage else float("nan")
                rss = max(r["rss_kib"] for r in usage) if usage else 0
                loads = [r["load"] for r in cell["runs"]]
                row.update(wall_min=min(walls), wall_median=statistics.median(walls), cpu=cpu,
                           rss_kib=rss, size=cell["size"], load_min=min(loads), load_max=max(loads),
                           walls=walls)
                print(f"| {c[0]} | {c[1]} | {fmt_ms(min(walls))} | {fmt_ms(statistics.median(walls))} | "
                      f"{cpu:.2f}s | {rss / 1024:.0f} MiB | {cell['size'] / (1 << 20):.1f} MiB | "
                      f"{min(loads):.0f}–{max(loads):.0f} |", flush=True)
            results.append(row)
    print(f"\nload average after: {os.getloadavg()}")
    if o.json:
        with open(o.json, "w") as f:
            json.dump(results, f, indent=1)
    return 0


LAP = re.compile(r"^qld: (.+): ([0-9.]+) ms$")
DURATION = re.compile(r"^qld-lap: (.+): ([0-9.]+) ms$")


def laps(o, linkers, specs):
    """The --laps mode; see the module documentation."""
    wanted = [k for k in o.only.split(",") if k.startswith("qld")]
    threads = o.threads.split(",")
    for spec in specs:
        env = dict(os.environ, **spec.get("env", {}), QLD_TIMING="1")
        configs = [(k, t) for k in wanted for t in threads]
        cells = {c: [] for c in configs}
        out = os.path.join(o.outdir, f"{spec['name']}.laps")
        for _ in range(o.runs):
            for c in configs:
                remove(out)
                os.sync()
                argv = argv_for(spec, c[0], linkers[c[0]], c[1], out, ["--no-fork", *shlex.split(o.qld_args)])
                stat = os.path.join(o.outdir, "perf-stat.csv")
                if o.perf:
                    argv = [o.perf, "stat", "-x", ",", "-e", "instructions:u", "-o", stat, "--", *argv]
                r = run_one(argv, spec["cwd"], env)
                if not r["ok"]:
                    sys.stderr.write(r["stderr"][-2000:])
                    raise SystemExit(f"{spec['name']}: {c[0]} failed at {c[1]} threads")
                values = {}
                previous = 0.0
                for line in r["stderr"].splitlines():
                    if m := LAP.match(line):
                        at = float(m.group(2))
                        values[m.group(1)] = at - previous
                        previous = at
                    elif m := DURATION.match(line):
                        values["· " + m.group(1)] = values.get("· " + m.group(1), 0.0) + float(m.group(2))
                values["wall"] = r["wall"] * 1000
                values["CPU"] = r["cpu"] * 1000
                values["CPU in the kernel"] = r["sys"] * 1000
                values["page faults (k)"] = r["minflt"] / 1000
                if o.perf:
                    with open(stat) as f:
                        for line in f:
                            fields = line.split(",")
                            if len(fields) > 2 and fields[2].startswith("instructions"):
                                values["instructions (M)"] = float(fields[0]) / 1e6
                cells[c].append(values)
        remove(out)
        print(f"\n### {spec['name']} (stage laps in ms, min / median of {o.runs}, `--no-fork`)\n")
        names = []
        for c in configs:
            for v in cells[c]:
                for k in v:
                    if k not in names:
                        names.append(k)
        print("| stage | " + " | ".join(f"{k} {t}" for k, t in configs) + " |")
        print("| --- |" + " --- |" * len(configs))
        for name in names:
            row = []
            for c in configs:
                xs = [v[name] for v in cells[c] if name in v]
                row.append(f"{min(xs):.1f} / {statistics.median(xs):.1f}" if xs else "-")
            print(f"| {name} | " + " | ".join(row) + " |", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
