#!/usr/bin/env python3
"""Turns the argv logs written by capture.sh into tests/corpus/*.txt files."""
import os, re, subprocess

import sys

# Usage: normalize.py <build dir used by capture.sh> <output dir>
build = os.path.abspath(sys.argv[1])
raw = os.path.join(build, "raw")
out_dir = sys.argv[2]

def version(cmd):
    return subprocess.run(cmd, capture_output=True, text=True).stdout.splitlines()[0]

gcc = version(["gcc", "--version"])
clang = version(["clang", "--version"])
rustc = version(["rustc", "--version"])
cmake = version(["cmake", "--version"])
meson = "meson " + version(["meson", "--version"])
bfd = version(["ld.bfd", "--version"])

cases = {
    "gcc-dynamic-c": (f"{gcc}, {bfd}", "gcc -O2 -no-pie -o gcc-dynamic-c hello.c",
                      {"output": "gcc-dynamic-c", "kind": "Executable", "gc_sections": "false"}),
    "gcc-static-c": (f"{gcc}, {bfd}", "gcc -O2 -static -o gcc-static-c hello.c",
                     {"output": "gcc-static-c", "kind": "StaticExecutable"}),
    "gcc-pie-c": (f"{gcc}, {bfd}", "gcc -O2 -fPIE -pie -o gcc-pie-c hello.c",
                  {"output": "gcc-pie-c", "kind": "Pie", "dynamic_linker": "/lib64/ld-linux-x86-64.so.2"}),
    "gcc-static-pie-c": (f"{gcc}, {bfd}", "gcc -O2 -static-pie -o gcc-static-pie-c hello.c",
                         {"output": "gcc-static-pie-c", "kind": "StaticPie"}),
    "gcc-lto-gc-sections-c": (f"{gcc}, {bfd}",
                              "gcc -O2 -flto -Wl,--gc-sections -Wl,-O1 -Wl,--as-needed -o gcc-lto-gc-sections-c hello.c",
                              {"output": "gcc-lto-gc-sections-c", "kind": "Pie", "gc_sections": "true"}),
    "gxx-shared-lib": (f"{gcc}, {bfd}",
                       "g++ -O2 -fPIC -shared -Wl,-soname,libgreet.so.1 -Wl,-z,defs -o gxx-shared-lib lib.cpp",
                       {"output": "gxx-shared-lib", "kind": "Shared", "soname": "libgreet.so.1"}),
    "clang-pie-c": (f"{clang}, {bfd}", "clang -O2 -o clang-pie-c hello.c",
                    {"output": "clang-pie-c", "kind": "Pie"}),
    "clang-static-c": (f"{clang}, {bfd}", "clang -O2 -static -o clang-static-c hello.c",
                       {"output": "clang-static-c", "kind": "StaticExecutable"}),
    "clangxx-shared-lib": (f"{clang}, {bfd}",
                           "clang++ -O2 -fPIC -shared -Wl,-soname,libgreet.so -o clangxx-shared-lib lib.cpp",
                           {"output": "clangxx-shared-lib", "kind": "Shared", "soname": "libgreet.so"}),
    "rustc-bin-lld": (f"{rustc} (default linker: rust-lld via gcc -fuse-ld=lld)",
                      "rustc -O -o rustc-bin-lld main.rs",
                      {"output": "rustc-bin-lld", "kind": "Pie", "gc_sections": "true"}),
    "rustc-bin-bfd": (f"{rustc}, {bfd}", "rustc -O -C linker-features=-lld -o rustc-bin-bfd main.rs",
                      {"output": "rustc-bin-bfd", "kind": "Pie", "gc_sections": "true"}),
    "rustc-cdylib.so": (f"{rustc}, {bfd}",
                        "rustc -O -C linker-features=-lld --crate-type cdylib -o rustc-cdylib.so main.rs",
                        {"output": "rustc-cdylib.so", "kind": "Shared", "gc_sections": "true"}),
    "cmake-shared-lib": (f"{cmake}, Ninja, {gcc}", "add_library(greet SHARED lib.cpp) with VERSION 1.2.3 SOVERSION 1",
                         {"output": "libgreet.so.1.2.3", "kind": "Shared", "soname": "libgreet.so.1"}),
    "cmake-exe-rpath": (f"{cmake}, Ninja, {gcc}", "add_executable(app main.c) linked to the greet shared library",
                        {"output": "app", "kind": "Pie"}),
    "meson-shared-lib": (f"{meson}, {gcc}", "shared_library('greet', 'lib.cpp', version: '1.0.0')",
                         {"output": "libgreet.so.1.0.0", "kind": "Shared", "soname": "libgreet.so.1"}),
    "meson-exe": (f"{meson}, {gcc}", "executable('app', 'main.c', link_with: greet)",
                  {"output": "app", "kind": "Pie"}),
}

def normalize(arg):
    arg = arg.replace(build, "/build")
    arg = arg.replace(os.path.expanduser("~"), "/home/user")
    arg = re.sub(r"/tmp/cc[A-Za-z0-9]{6}\.", "/tmp/ccXXXXXX.", arg)
    arg = re.sub(r"/tmp/([A-Za-z0-9_]+)-[0-9a-f]{6}\.o", r"/tmp/\1-XXXXXX.o", arg)
    arg = re.sub(r"/build/rustc[A-Za-z0-9]{6}", "/build/rustcXXXXXX", arg)
    return arg

os.makedirs(out_dir, exist_ok=True)
for name, (tools, command, expect) in cases.items():
    with open(os.path.join(raw, name + ".args")) as f:
        args = f.read().split("\n")
    if args and args[-1] == "":
        args.pop()
    # gcc turns each -B directory into a -L too; the logging shim is not part
    # of a real build.
    args = [a for a in args if a != "-L" + os.path.join(build, "shim")]
    stem = name.removesuffix(".so")
    lines = [
        f"# Linker argv captured from: {command}",
        f"# Tools: {tools}",
        "# Host: x86_64-pc-linux-gnu (Gentoo). One argument per line; argv[0] is not included.",
        "# Machine-specific paths are normalized: build dir -> /build, $HOME -> /home/user,",
        "# temporary file names -> XXXXXX.",
    ]
    for key, value in expect.items():
        lines.append(f"#! {key} = {value}")
    lines += [normalize(a) for a in args]
    with open(os.path.join(out_dir, stem + ".txt"), "w") as f:
        f.write("\n".join(lines) + "\n")
    print(stem, len(args))
