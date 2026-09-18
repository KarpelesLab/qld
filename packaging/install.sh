#!/bin/sh
# Installs a built qld binary with the names compiler drivers look for.
#
#   BINDIR/qld                  the linker
#   BINDIR/ld.qld               -> qld   clang -fuse-ld=qld (ELF, PE)
#   BINDIR/ld64.qld             -> qld   clang -fuse-ld=qld (Mach-O targets)
#   LIBEXECDIR/qld/ld           -> qld   gcc -B LIBEXECDIR/qld (gcc has no
#                                        -fuse-ld=qld; see packaging/README.md)
#
# qld picks its command-line flavor from the name it is run as: `ld64` and
# `ld64.*` select the Apple ld64 flavor, every other name the GNU one.
#
# On Unix the extra names are relative symbolic links, so that they resolve
# inside DESTDIR and an installed tree can be moved. On Windows (a binary named *.exe, or
# --copy) they are copies: MinGW gcc and clang look for ld.exe and
# ld.qld.exe, and symbolic links need privileges there.
#
# Nothing but POSIX sh and coreutils is needed, so distribution recipes
# (packaging/arch/PKGBUILD, packaging/debian/rules), the release workflow
# (packaging/dist.sh) and tests/projects/packaging.sh all share this file.
#
# Usage: packaging/install.sh [options] path/to/qld[.exe]
#   --prefix DIR       default /usr/local
#   --destdir DIR      staging root prepended to every path (default $DESTDIR)
#   --bindir DIR       default PREFIX/bin
#   --libexecdir DIR   default PREFIX/libexec (Arch: /usr/lib)
#   --no-libexec       do not install LIBEXECDIR/qld/ld
#   --docdir DIR       also install LICENSE and README.md there
#   --copy             copy instead of symlinking (default for *.exe)

set -eu

prog=packaging/install.sh
prefix=/usr/local
destdir=${DESTDIR:-}
bindir=
libexecdir=
libexec=1
docdir=
copy=
binary=

die() {
  echo "$prog: $*" >&2
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) [ $# -ge 2 ] || die "$1 needs a value"; prefix=$2; shift 2 ;;
    --prefix=*) prefix=${1#*=}; shift ;;
    --destdir) [ $# -ge 2 ] || die "$1 needs a value"; destdir=$2; shift 2 ;;
    --destdir=*) destdir=${1#*=}; shift ;;
    --bindir) [ $# -ge 2 ] || die "$1 needs a value"; bindir=$2; shift 2 ;;
    --bindir=*) bindir=${1#*=}; shift ;;
    --libexecdir) [ $# -ge 2 ] || die "$1 needs a value"; libexecdir=$2; shift 2 ;;
    --libexecdir=*) libexecdir=${1#*=}; shift ;;
    --no-libexec) libexec=; shift ;;
    --docdir) [ $# -ge 2 ] || die "$1 needs a value"; docdir=$2; shift 2 ;;
    --docdir=*) docdir=${1#*=}; shift ;;
    --copy) copy=1; shift ;;
    -h | --help) sed -n '2,/^$/s/^# \{0,1\}//p' "$0"; exit 0 ;;
    -*) die "unknown option: $1" ;;
    *) [ -z "$binary" ] || die "more than one binary given"; binary=$1; shift ;;
  esac
done

[ -n "$binary" ] || die "no qld binary given (try --help)"
[ -f "$binary" ] || die "$binary: not a file"

bindir=${bindir:-$prefix/bin}
libexecdir=${libexecdir:-$prefix/libexec}
# Trailing slashes would confuse relpath.
while [ "${bindir%/}" != "$bindir" ] && [ "$bindir" != / ]; do bindir=${bindir%/}; done
while [ "${libexecdir%/}" != "$libexecdir" ] && [ "$libexecdir" != / ]; do libexecdir=${libexecdir%/}; done

# relpath DIR PATH: PATH relative to the directory DIR. Both are absolute
# (or both relative to the same directory), without `.` or `..`.
relpath() {
  from=$1
  up=
  while :; do
    case "$2/" in "$from"/*) echo "$up${2#"$from"/}"; return ;; esac
    case "$from" in
      */*) from=${from%/*} ;;
      *) echo "../$up$2"; return ;;
    esac
    up=../$up
  done
}

exe=
case "$binary" in
  *.exe | *.EXE) exe=.exe; copy=1 ;;
esac

# `install -D` is not POSIX; mkdir -p and cp -f then chmod are.
mkdir -p "$destdir$bindir"
cp -f "$binary" "$destdir$bindir/qld$exe"
chmod 755 "$destdir$bindir/qld$exe"

# link TARGET_FOR_SYMLINK NAME: NAME becomes qld, as a copy or a symlink.
link() {
  rm -f "$2"
  if [ -n "$copy" ]; then
    cp -f "$destdir$bindir/qld$exe" "$2"
    chmod 755 "$2"
  else
    ln -s "$1" "$2"
  fi
}

link "qld$exe" "$destdir$bindir/ld.qld$exe"
link "qld$exe" "$destdir$bindir/ld64.qld$exe"

if [ -n "$libexec" ]; then
  mkdir -p "$destdir$libexecdir/qld"
  link "$(relpath "$libexecdir/qld" "$bindir/qld$exe")" "$destdir$libexecdir/qld/ld$exe"
fi

if [ -n "$docdir" ]; then
  top=$(cd "$(dirname "$0")/.." && pwd)
  mkdir -p "$destdir$docdir"
  for f in LICENSE README.md; do
    cp -f "$top/$f" "$destdir$docdir/$f"
    chmod 644 "$destdir$docdir/$f"
  done
fi
