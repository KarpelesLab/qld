#!/bin/sh
# Build CPython with qld as the linker and run its test suite.
# Usage: tests/projects/python.sh /path/to/qld /path/to/scratch [qld|gnu] [shared|static]
#   shared: ./configure --enable-shared (libpython3.X.so, the default here)
#   static: ./configure (a static libpython linked into the interpreter)
# Extension modules are shared objects in both cases.
QLD=$1
SCRATCH=$2
MODE=${3:-qld}
KIND=${4:-shared}
VERSION=${PYTHON_VERSION:-3.14.7}
. "$(dirname "$0")/common.sh"

tarball=$(fetch "https://www.python.org/ftp/python/$VERSION/Python-$VERSION.tar.xz")
dir="$SCRATCH/build/python-$VERSION-$MODE-$KIND"
rm -rf "$dir"
mkdir -p "$dir"
tar -xf "$tarball" -C "$dir" --strip-components=1
cd "$dir"
export QLD_LINK_LOG="$dir/links.log"

if [ "$MODE" = gnu ]; then CC=gcc; CXX=g++; else CC="$QLD_BIN/qcc"; CXX="$QLD_BIN/qc++"; fi
if [ "$KIND" = shared ]; then flags=--enable-shared; else flags=; fi
jobs=${JOBS:-$(nproc)}

t0=$(now)
./configure CC="$CC" CXX="$CXX" $flags > configure.log 2>&1
t1=$(now)
make -j"$jobs" > make.log 2>&1
t2=$(now)
# Modules that failed to build or to import are listed at the end of make.
sed -n '/^The necessary bits/,$p;/^Following modules built successfully but were removed/,$p;/^Failed to build these modules/,$p' make.log
[ "$MODE" = gnu ] || check_linked_by_qld . || true
set +e
# -u all minus the network and very slow resources; --timeout guards hangs.
LD_LIBRARY_PATH="$dir" ./python -m test -j"$jobs" --timeout 1800 -u all,-network,-urlfetch,-largefile \
  > check.log 2>&1
status=$?
t3=$(now)
sed -n '/^== Tests result/,$p' check.log | head -5
grep -E '^[0-9]+ tests? (failed|skipped|OK|altered|omitted)|^Total tests|^Result:' check.log
grep -A30 -E '^[0-9]+ tests? failed' check.log | head -40
echo "configure $((t1 - t0))s, make $((t2 - t1))s, test $((t3 - t2))s, test exit $status"
exit $status
