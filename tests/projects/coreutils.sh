#!/bin/sh
# Build GNU coreutils with qld as the linker and run `make check`.
# Usage: tests/projects/coreutils.sh /path/to/qld /path/to/scratch [linker]
# The optional third argument "gnu" builds with the system GNU ld instead,
# for comparing test results.
QLD=$1
SCRATCH=$2
MODE=${3:-qld}
VERSION=${COREUTILS_VERSION:-9.11}
. "$(dirname "$0")/common.sh"

tarball=$(fetch "https://ftp.gnu.org/gnu/coreutils/coreutils-$VERSION.tar.xz")
dir="$SCRATCH/build/coreutils-$VERSION-$MODE"
rm -rf "$dir"
mkdir -p "$dir"
tar -xf "$tarball" -C "$dir" --strip-components=1
cd "$dir"
export QLD_LINK_LOG="$dir/links.log"

if [ "$MODE" = gnu ]; then CC=gcc; else CC="$QLD_BIN/qcc"; fi
jobs=${JOBS:-$(nproc)}

t0=$(now)
./configure CC="$CC" > configure.log 2>&1
t1=$(now)
make -j"$jobs" > make.log 2>&1
t2=$(now)
[ "$MODE" = gnu ] || check_linked_by_qld src
set +e
make -k -j"$jobs" check > check.log 2>&1
status=$?
t3=$(now)
grep -E '^# (TOTAL|PASS|SKIP|XFAIL|FAIL|XPASS|ERROR):' tests/test-suite.log gnulib-tests/test-suite.log 2>/dev/null
grep -E '^(FAIL|ERROR):' check.log
echo "configure $((t1 - t0))s, make $((t2 - t1))s, check $((t3 - t2))s, check exit $status"
exit $status
