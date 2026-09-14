#!/bin/sh
# Release build of LLVM, clang and lld with qld as the linker, then
# check-llvm, check-clang and check-lld.
# Usage: tests/projects/llvm.sh /path/to/qld /path/to/scratch [static|shared] [targets...]
#   static: the default component libraries (.a), one big clang/lld binary
#   shared: -DBUILD_SHARED_LIBS=ON (one .so per component library)
# Without targets, builds `all` and runs check-llvm check-clang check-lld.
# Compiles with the system clang; set LLVM_CC/LLVM_CXX to change that.
# LLVM_LTO=Thin or Full builds with LTO (-DLLVM_ENABLE_LTO) through the
# compiler's LLVMgold.so, archives made by llvm-ar, in a directory of its own.
QLD=$1
SCRATCH=$2
KIND=${3:-static}
shift 3 2>/dev/null || shift $#
VERSION=${LLVM_VERSION:-23.1.1}
. "$(dirname "$0")/common.sh"

tarball=$(fetch "https://github.com/llvm/llvm-project/releases/download/llvmorg-$VERSION/llvm-project-$VERSION.src.tar.xz")
srcdir="$SCRATCH/src/llvm-project-$VERSION.src"
if [ ! -d "$srcdir" ]; then
  tar -xf "$tarball" -C "$SCRATCH/src"
fi
dir="$SCRATCH/build/llvm-$VERSION-$KIND${LLVM_LTO:+-lto-$LLVM_LTO}"
mkdir -p "$dir"
cd "$dir"
export QLD_LINK_LOG="$dir/links.log"

cc=${LLVM_CC:-clang}
cxx=${LLVM_CXX:-clang++}
if [ "$KIND" = shared ]; then shared=ON; else shared=OFF; fi
jobs=${JOBS:-$(nproc)}

if [ -n "${RELINK:-}" ] && [ -f build.ninja ]; then
  # Delete every linked output so that ninja relinks them (and only them)
  # with the current qld; ninja does not track the linker binary.
  find . -type f \( -perm -u+x -o -name '*.so' -o -name '*.so.*' \) -not -path './CMakeFiles/*' |
    while read -r f; do
      if head -c 4 "$f" | grep -q ELF && readelf -h "$f" 2>/dev/null | grep -Eq 'Type: +(EXEC|DYN)'; then
        rm -f "$f"
      fi
    done
  : > "$QLD_LINK_LOG"
fi

t0=$(now)
cmake -G Ninja "$srcdir/llvm" \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_C_COMPILER="$cc" -DCMAKE_CXX_COMPILER="$cxx" \
  -DLLVM_USE_LINKER="$QLD_BIN/ld" \
  -DLLVM_ENABLE_PROJECTS="clang;lld" \
  -DLLVM_TARGETS_TO_BUILD=X86 \
  -DBUILD_SHARED_LIBS="$shared" \
  -DLLVM_ENABLE_ASSERTIONS=OFF \
  -DLLVM_INCLUDE_BENCHMARKS=OFF \
  ${LLVM_LTO:+-DLLVM_ENABLE_LTO="$LLVM_LTO" -DCMAKE_AR="$(command -v llvm-ar)" -DCMAKE_RANLIB="$(command -v llvm-ranlib)"} \
  > configure.log 2>&1
t1=$(now)
ninja -j"$jobs" > make.log 2>&1
t2=$(now)
check_linked_by_qld bin lib || true
set +e
if [ $# -eq 0 ]; then set -- check-llvm check-clang check-lld; fi
status=0
for target in "$@"; do
  LIT_OPTS="-sv --no-progress-bar" ninja -k 0 -j"$jobs" "$target" > "$target.log" 2>&1 || status=$?
  echo "== $target"
  grep -E '^(Total Discovered Tests|  (Skipped|Unsupported|Passed|Expectedly Failed|Failed|Unexpectedly Passed|Timed Out) *:)' "$target.log"
  grep -E '^  [A-Za-z]+ :: ' "$target.log" | sed -n '/Failed Tests/,$p' | head -50
  grep -E '^(FAIL|TIMEOUT): ' "$target.log" | head -50
done
t3=$(now)
echo "configure $((t1 - t0))s, build $((t2 - t1))s, checks $((t3 - t2))s, check status $status"
exit $status
