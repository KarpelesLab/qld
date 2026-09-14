#!/bin/sh
# Build OpenSSL 3.x (shared libcrypto/libssl with version scripts, engines
# and providers) with qld as the linker and run `make test`.
# Usage: tests/projects/openssl.sh /path/to/qld /path/to/scratch [qld|gnu]
QLD=$1
SCRATCH=$2
MODE=${3:-qld}
VERSION=${OPENSSL_VERSION:-3.6.4}
. "$(dirname "$0")/common.sh"

tarball=$(fetch "https://github.com/openssl/openssl/releases/download/openssl-$VERSION/openssl-$VERSION.tar.gz")
dir="$SCRATCH/build/openssl-$VERSION-$MODE"
rm -rf "$dir"
mkdir -p "$dir"
tar -xf "$tarball" -C "$dir" --strip-components=1
cd "$dir"
export QLD_LINK_LOG="$dir/links.log"

if [ "$MODE" = gnu ]; then CC=gcc; else CC="$QLD_BIN/qcc"; fi
jobs=${JOBS:-$(nproc)}

t0=$(now)
./Configure CC="$CC" > configure.log 2>&1
t1=$(now)
make -j"$jobs" > make.log 2>&1
t2=$(now)
[ "$MODE" = gnu ] || check_linked_by_qld . || true
set +e
make test HARNESS_JOBS="$jobs" > check.log 2>&1
status=$?
t3=$(now)
grep -E '^(Files=|Result:|Failed|[0-9a-z_/-]+\.t +\(Wstat)' check.log
echo "configure $((t1 - t0))s, make $((t2 - t1))s, test $((t3 - t2))s, test exit $status"
exit $status
