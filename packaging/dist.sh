#!/bin/sh
# Makes a release archive from a built qld binary.
#
#   qld-VERSION-TARGET/
#     bin/qld            bin/ld.qld -> qld     bin/ld64.qld -> qld
#     libexec/qld/ld -> ../../bin/qld
#     LICENSE  README.md
#
# as a .tar.gz, or a .zip with copies instead of links for Windows targets.
# The layout is mold's: extract it anywhere (or into /usr/local with
# --strip-components=1) and put bin/ on PATH for `clang -fuse-ld=qld`, or
# pass `-B .../libexec/qld` to gcc. A TARGET.sha256 file in `sha256sum`
# format is written next to the archive.
#
# Usage: packaging/dist.sh BINARY TARGET VERSION OUTDIR
#   e.g. packaging/dist.sh target/x86_64-unknown-linux-musl/release/qld \
#          x86_64-unknown-linux-musl 0.1.0 dist
# Prints the archive's path.

set -eu

[ $# -eq 4 ] || { echo "usage: $0 BINARY TARGET VERSION OUTDIR" >&2; exit 2; }
binary=$1
target=$2
version=$3
outdir=$4
here=$(cd "$(dirname "$0")" && pwd)

name=qld-$version-$target
mkdir -p "$outdir"
outdir=$(cd "$outdir" && pwd)
stage=$(mktemp -d "${TMPDIR:-/tmp}/qld-dist.XXXXXX")
trap 'rm -rf "$stage"' EXIT

sh "$here/install.sh" --prefix "$stage/$name" --docdir "$stage/$name" "$binary"

# Reproducible archives where the tools allow it: sorted names, fixed
# owner and mtime (SOURCE_DATE_EPOCH, else the commit time, else 0).
epoch=${SOURCE_DATE_EPOCH:-$(git -C "$here" log -1 --format=%ct 2>/dev/null || echo 0)}

case "$target" in
  *windows*)
    archive=$outdir/$name.zip
    rm -f "$archive"
    if command -v zip >/dev/null 2>&1; then
      (
        cd "$stage"
        find "$name" -exec touch -d "@$epoch" {} + 2>/dev/null || true
        find "$name" -type f | LC_ALL=C sort | zip -q -X "$archive" -@
      )
    elif command -v 7z >/dev/null 2>&1; then
      (cd "$stage" && 7z a -tzip -bso0 -bsp0 "$archive" "$name")
    else
      echo "$0: neither zip nor 7z found" >&2
      exit 1
    fi
    ;;
  *)
    archive=$outdir/$name.tar.gz
    if tar --version 2>/dev/null | grep -q 'GNU tar'; then
      tar -C "$stage" --sort=name --owner=0 --group=0 --numeric-owner \
        --mtime="@$epoch" -cf - "$name" | gzip -9n > "$archive"
    else
      # bsdtar (macOS)
      tar -C "$stage" --uid 0 --gid 0 -cf - "$name" | gzip -9n > "$archive"
    fi
    ;;
esac

# sha256sum format, with the bare file name so that `sha256sum -c` works
# next to the archive.
if command -v sha256sum >/dev/null 2>&1; then
  sum=$(sha256sum "$archive" | cut -d' ' -f1)
else
  sum=$(shasum -a 256 "$archive" | cut -d' ' -f1)
fi
printf '%s  %s\n' "$sum" "$(basename "$archive")" > "$archive.sha256"
echo "$archive"
