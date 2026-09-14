#!/bin/sh
# Bootstrap rustc (stage 1) with qld as the host linker, then run a subset of
# the test suite with the stage 1 compiler (which also links with qld).
# Usage: tests/projects/rust.sh /path/to/qld /path/to/scratch [test paths...]
# Default test paths: tests/ui library/std. Set RUST_CLEAN=1 to rebuild
# stage 1 from scratch (a rerun is otherwise incremental). Set LLVM_CONFIG to use an
# external LLVM (default: the system LLVM 22, to avoid building LLVM).
QLD=$1
SCRATCH=$2
shift 2
VERSION=${RUST_VERSION:-1.98.1}
. "$(dirname "$0")/common.sh"

tarball=$(fetch "https://static.rust-lang.org/dist/rustc-$VERSION-src.tar.xz")
dir="$SCRATCH/build/rustc-$VERSION-src"
if [ ! -d "$dir" ]; then
  tar -xf "$tarball" -C "$SCRATCH/build"
fi
cd "$dir"
export QLD_LINK_LOG="$dir/links.log"
llvm_config=${LLVM_CONFIG:-/usr/lib/llvm/22/bin/llvm-config}
jobs=${JOBS:-$(nproc)}

# rust.lld = false and default-linker-linux-override = "off" keep rust-lld
# out; the target linker is a clang wrapper whose --ld-path (which wins over
# any -fuse-ld=lld rustc adds) is qld.
cat > bootstrap.toml <<EOF
change-id = "ignore"

[build]
jobs = $jobs
extended = false
docs = false

[rust]
lld = false
bootstrap-override-lld = false
debuginfo-level = 0
channel = "stable"

[llvm]
# The system LLVM ships as a shared libLLVM only.
link-shared = true

[target.x86_64-unknown-linux-gnu]
llvm-config = "$llvm_config"
cc = "clang"
cxx = "clang++"
linker = "$QLD_BIN/qclang"
default-linker-linux-override = "off"
EOF

# RUST_CLEAN=1 rebuilds (and relinks) stage 1 from scratch.
if [ -n "${RUST_CLEAN:-}" ]; then
  rm -rf build/x86_64-unknown-linux-gnu/stage1 build/x86_64-unknown-linux-gnu/stage1-*
fi
: > "$QLD_LINK_LOG"

t0=$(now)
python3 x.py build --stage 1 > build.log 2>&1
t1=$(now)
check_linked_by_qld build/x86_64-unknown-linux-gnu/stage1 build/x86_64-unknown-linux-gnu/stage1-rustc || true
set +e
if [ $# -eq 0 ]; then set -- tests/ui library/std; fi
python3 x.py test --stage 1 --no-fail-fast "$@" > test.log 2>&1
status=$?
t2=$(now)
grep -E '^test result:|^failures:$|^    \[ui\]|^Build completed|^Build failed' test.log | head -60
echo "build $((t1 - t0))s, test $((t2 - t1))s, test exit $status"
exit $status
