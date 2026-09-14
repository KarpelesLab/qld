#!/bin/sh
# Build curl (shared libcurl plus the curl tool) with qld as the linker and
# run its test suite (local test servers only; no network needed).
# Usage: tests/projects/curl.sh /path/to/qld /path/to/scratch [qld|gnu]
QLD=$1
SCRATCH=$2
MODE=${3:-qld}
VERSION=${CURL_VERSION:-8.22.0}
. "$(dirname "$0")/common.sh"

tarball=$(fetch "https://curl.se/download/curl-$VERSION.tar.xz")
dir="$SCRATCH/build/curl-$VERSION-$MODE"
rm -rf "$dir"
mkdir -p "$dir"
tar -xf "$tarball" -C "$dir" --strip-components=1
cd "$dir"
export QLD_LINK_LOG="$dir/links.log"

if [ "$MODE" = gnu ]; then CC=gcc; else CC="$QLD_BIN/qcc"; fi
jobs=${JOBS:-$(nproc)}

t0=$(now)
./configure CC="$CC" --with-openssl --enable-shared --disable-static > configure.log 2>&1
t1=$(now)
make -j"$jobs" > make.log 2>&1
# The unit tests and libtests are built by `make test` itself; build them
# up front so that link failures show up here.
make -j"$jobs" -C tests > make-tests.log 2>&1
t2=$(now)
[ "$MODE" = gnu ] || check_linked_by_qld lib src tests || true
set +e
make test TFLAGS="-j$jobs -a" > check.log 2>&1
status=$?
t3=$(now)
grep -E '^(TESTFAIL|IGNORED|TESTDONE|TESTINFO)' check.log
echo "configure $((t1 - t0))s, make $((t2 - t1))s, test $((t3 - t2))s, test exit $status"
exit $status
