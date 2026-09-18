#!/bin/sh
# Builds the release binary for one target, as the release workflow does,
# and prints its path on the last line of stdout.
#
# With --dogfood (Linux targets built on a Linux host of the same
# architecture), qld then links itself: stage 1 is built with the default
# linker, stage 2 with stage 1 as the linker through gcc's -B lookup
# (`ld` -> stage 1). Stage 2 ships only if
#
#   - it builds,
#   - its .comment section says `Linker: qld`, so gcc did not fall back to
#     the system linker,
#   - it runs, and
#   - linked by itself it links a C hello world that runs.
#
# Otherwise the script warns (a `::warning::` line in GitHub Actions) and
# ships stage 1, so a qld bug never blocks a release.
#
# Usage: packaging/release-build.sh [--dogfood] TARGET
# Environment: CARGO (default cargo), CC for the hello-world check (default
# cc); CARGO_TARGET_DIR and CARGO_PROFILE_RELEASE_* as for cargo. Run from
# the source tree.

set -eu

dogfood=
if [ "${1:-}" = --dogfood ]; then
  dogfood=1
  shift
fi
[ $# -eq 1 ] || { echo "usage: $0 [--dogfood] TARGET" >&2; exit 2; }
target=$1
cargo=${CARGO:-cargo}
tdir=${CARGO_TARGET_DIR:-target}

exe=
case "$target" in *windows*) exe=.exe ;; esac

log() { echo "release-build: $*" >&2; }
warn() {
  if [ -n "${GITHUB_ACTIONS:-}" ]; then
    echo "::warning title=qld did not link itself::$*"
  fi
  log "warning: $*"
}

log "stage 1: $target with the default linker"
"$cargo" build --release --locked --target "$target" >&2
stage1=$tdir/$target/release/qld$exe
[ -x "$stage1" ] || { log "$stage1 was not built"; exit 1; }

if [ -z "$dogfood" ]; then
  echo "$stage1"
  exit 0
fi

# The -B directory has a fixed path: it is part of RUSTFLAGS, which cargo
# fingerprints, so a random one would rebuild every dependency each time.
lddir=$tdir/dogfood-ld/$target
rm -rf "$lddir"
mkdir -p "$lddir"
lddir=$(cd "$lddir" && pwd)
cp "$stage1" "$lddir/qld"
ln -s qld "$lddir/ld"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/qld-dogfood.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

# rustc links through `cc`, so gcc's -B finds $lddir/ld. On
# x86_64-unknown-linux-gnu, rustc 1.90 and later link with its bundled
# rust-lld instead (`-fuse-ld=lld` and its own -B), which has to be turned
# off for -B to be used.
flags="-C link-arg=-B$lddir"
case "$target" in
  x86_64-unknown-linux-gnu) flags="$flags -C linker-features=-lld" ;;
esac

stage2=$tdir/dogfood/$target/release/qld$exe
log "stage 2: $target linked by stage 1 ($flags)"
if ! RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }$flags" \
    "$cargo" build --release --locked --target "$target" --target-dir "$tdir/dogfood" >&2; then
  warn "stage 2 of $target failed to build; shipping the stage 1 binary"
  echo "$stage1"
  exit 0
fi

# The .comment check without depending on readelf: `Linker: qld` is a
# NUL-terminated string in it, and no other part of qld contains it
# followed by a version number.
if ! LC_ALL=C grep -a -q 'Linker: qld [0-9]' "$stage2"; then
  warn "stage 2 of $target has no 'Linker: qld' comment (gcc used another linker?); shipping stage 1"
  echo "$stage1"
  exit 0
fi
if ! "$stage2" --version >&2; then
  warn "stage 2 of $target does not run; shipping stage 1"
  echo "$stage1"
  exit 0
fi

# Stage 2 links a program, which must run. Stage 1 may still be running
# (the `--fork` child unmapping its inputs), so replace it rather than
# write over it (ETXTBSY).
rm -f "$lddir/qld"
cp "$stage2" "$lddir/qld"
printf '#include <stdio.h>\nint main(void) { puts("hello from qld"); return 0; }\n' > "$tmp/hello.c"
if ! "${CC:-cc}" -B"$lddir" "$tmp/hello.c" -o "$tmp/hello" >&2 ||
   ! LC_ALL=C grep -a -q 'Linker: qld [0-9]' "$tmp/hello" ||
   [ "$("$tmp/hello")" != "hello from qld" ]; then
  warn "stage 2 of $target cannot link a working hello world; shipping stage 1"
  echo "$stage1"
  exit 0
fi

log "shipping stage 2: $target linked by qld"
echo "$stage2"
