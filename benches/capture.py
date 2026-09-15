#!/usr/bin/env python3
"""Capture a link as a replayable benchmark spec.

A spec is a directory `SPECS/NAME/` holding `link.json`:

    {"name": ..., "cwd": ..., "args": [...], "output_index": N,
     "smoke": "shell command with {out}", "env": {...}}

`args` is the linker argv (without argv[0]) with response files expanded;
`args[output_index]` is the `-o` value, which `run.py` replaces with a path
of its own. The link is replayed in `cwd`, from the build tree's own inputs,
so the tree must stay in place. Inputs that live under a temporary directory
(rustc's `symbols.o`, response files) are copied into the spec directory.

Usage:

  capture.py SPECS NAME --smoke CMD ninja BUILD_DIR TARGET
      Runs the last command of `ninja -t commands TARGET` in BUILD_DIR with
      the linker replaced by a capturing shim (`-fuse-ld=` for clang,
      `-B` for gcc). Nothing is linked.

  capture.py SPECS NAME --smoke CMD cmd -- COMMAND...
      Runs COMMAND with `QLD_BENCH_SHIM` set to the shim's directory, for
      builds that can be pointed at it (e.g. cargo with
      `-C link-arg=-B$QLD_BENCH_SHIM`). Every link is also done with GNU ld
      so that the build can go on; the last link is kept.

  capture.py SPECS NAME --smoke CMD argv --cwd DIR -- ARG...
      Records a linker argv directly (e.g. from a build log).
"""

import argparse
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile

SHIM = r'''#!/usr/bin/env python3
import json, os, re, shlex, shutil, sys, tempfile
spec = os.environ["QLD_BENCH_CAPTURE"]
tmp = os.path.realpath(tempfile.gettempdir())
copied = os.path.join(os.path.dirname(spec), "inputs")

def expand(args):
    out = []
    for a in args:
        if a.startswith("@") and os.path.isfile(a[1:]):
            with open(a[1:], encoding="utf-8", errors="surrogateescape") as f:
                out.extend(expand(shlex.split(f.read())))
        else:
            out.append(a)
    return out

def keep(a):
    # Temporary inputs do not outlive the build; keep a copy.
    p = os.path.realpath(a)
    # rustc deletes its codegen-unit objects and its temporary directory
    # (`rustcXXXXXX/symbols.o`, next to the output) after the link.
    rustc_tmp = re.search(r"/rustc[A-Za-z0-9]{6}/", p)
    if os.path.isfile(p) and (p.startswith(tmp + os.sep) or p.endswith(".rcgu.o") or rustc_tmp):
        os.makedirs(copied, exist_ok=True)
        dst = os.path.join(copied, str(len(os.listdir(copied))) + "-" + os.path.basename(p))
        shutil.copyfile(p, dst)
        return dst
    return a

args = [keep(a) for a in expand(sys.argv[1:])]
if any(a in ("-v", "--version", "-V") for a in args) and "-o" not in args:
    os.execvp("ld.bfd", ["ld.bfd", *sys.argv[1:]])
idx = [i + 1 for i, a in enumerate(args[:-1]) if a == "-o"]
if not idx:
    sys.exit("qld-bench shim: no -o in link")
with open(spec, "w") as f:
    json.dump({"cwd": os.getcwd(), "args": args, "output_index": idx[-1]}, f, indent=1)
real = os.environ.get("QLD_BENCH_REAL_LD")
if real:
    os.execvp(real, [real, *sys.argv[1:]])
'''


def write_shim(directory):
    path = os.path.join(directory, "ld")
    with open(path, "w") as f:
        f.write(SHIM)
    os.chmod(path, 0o755)
    for alias in ("ld.bfd", "ld.lld", "ld.gold", "ld.mold"):
        link = os.path.join(directory, alias)
        if not os.path.exists(link):
            os.symlink("ld", link)
    return path


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("specs")
    p.add_argument("name")
    p.add_argument("--smoke", required=True, help="shell command checking {out}")
    p.add_argument("--env", action="append", default=[], help="KEY=VALUE for the replayed link")
    sub = p.add_subparsers(dest="mode", required=True)
    n = sub.add_parser("ninja")
    n.add_argument("build")
    n.add_argument("target")
    c = sub.add_parser("cmd")
    c.add_argument("command", nargs=argparse.REMAINDER)
    a = sub.add_parser("argv")
    a.add_argument("--cwd", required=True)
    a.add_argument("args", nargs=argparse.REMAINDER)
    o = p.parse_args()

    specdir = os.path.abspath(os.path.join(o.specs, o.name))
    os.makedirs(specdir, exist_ok=True)
    shutil.rmtree(os.path.join(specdir, "inputs"), ignore_errors=True)
    raw = os.path.join(specdir, "captured.json")
    if os.path.exists(raw):
        os.remove(raw)

    if o.mode == "argv":
        args = [x for x in o.args if x != "--"]
        idx = [i + 1 for i, x in enumerate(args[:-1]) if x == "-o"]
        with open(raw, "w") as f:
            json.dump({"cwd": os.path.abspath(o.cwd), "args": args, "output_index": idx[-1]}, f)
    else:
        with tempfile.TemporaryDirectory() as shimdir:
            shim = write_shim(shimdir)
            env = dict(os.environ, QLD_BENCH_CAPTURE=raw, QLD_BENCH_SHIM=shimdir)
            if o.mode == "cmd":
                # Earlier links (build scripts) must produce working outputs.
                env["QLD_BENCH_REAL_LD"] = "ld.bfd"
            if o.mode == "ninja":
                cmds = subprocess.run(["ninja", "-C", o.build, "-t", "commands", o.target],
                                      check=True, capture_output=True, text=True).stdout
                cmd = cmds.strip().splitlines()[-1]
                if re.search(r"-fuse-ld=\S+", cmd):
                    cmd = re.sub(r"-fuse-ld=\S+", f"-fuse-ld={shim}", cmd)
                else:
                    cmd = cmd.replace(" -o ", f" -B{shimdir} -o ", 1)
                subprocess.run(cmd, shell=True, cwd=o.build, env=env, check=True)
            else:
                command = [x for x in o.command if x != "--"] if o.command[:1] == ["--"] else o.command
                subprocess.run(command, env=env, check=True)
    if not os.path.exists(raw):
        raise SystemExit("no link was captured")
    with open(raw) as f:
        spec = json.load(f)
    os.remove(raw)
    spec["name"] = o.name
    spec["smoke"] = o.smoke
    spec["env"] = dict(kv.split("=", 1) for kv in o.env)
    with open(os.path.join(specdir, "link.json"), "w") as f:
        json.dump(spec, f, indent=1)
    print(f"{o.name}: {len(spec['args'])} args, cwd {spec['cwd']}, output {spec['args'][spec['output_index']]}")


if __name__ == "__main__":
    sys.exit(main())
