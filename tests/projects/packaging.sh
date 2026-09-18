#!/bin/sh
# Packaging check (M9): install qld the way the distribution packages and
# release archives do, and check that compiler drivers find it by name.
#
# Two installs are checked, each without touching the system:
#
#   - `packaging/install.sh --prefix /usr --destdir SCRATCH/destdir`, the
#     layout of the Gentoo, Arch, Debian and Homebrew recipes;
#   - the release archive `packaging/dist.sh` makes, extracted into
#     SCRATCH/prefix (or, with --archive, a given release archive).
#
# For each, it checks that
#
#   - bin/ld.qld, bin/ld64.qld and libexec/qld/ld resolve to bin/qld;
#   - ld64.qld takes the ld64 command line and ld.qld the GNU one;
#   - Linux: `gcc -B LIBEXEC/qld` links a hello world with qld, which runs;
#     `gcc -fuse-ld=qld` does too if this gcc accepts it (no gcc release
#     does: gcc knows only bfd, gold, lld, mold and, from 16, wild);
#   - Linux: `clang -fuse-ld=qld` finds ld.qld on PATH, and with -B instead
#     of PATH, and `clang --ld-path=` works; the program is linked by qld
#     (`Linker: qld` in .comment) and runs;
#   - clang for a Darwin target looks for ld64.qld (`-###`, on any host);
#   - macOS: `clang -fuse-ld=qld` and `--ld-path=` link a program that runs.
#
# A missing gcc or clang is reported and skipped, unless
# QLD_REQUIRE_PACKAGING_TOOLS=1, which makes it a failure.
#
# Usage:
#   tests/projects/packaging.sh /path/to/qld SCRATCH
#   tests/projects/packaging.sh --archive qld-VERSION-TARGET.tar.gz SCRATCH
# (CC and CLANG name the compilers; default gcc and clang.)

set -eu

here=$(cd "$(dirname "$0")" && pwd)
top=$(cd "$here/../.." && pwd)
archive=
if [ "${1:-}" = --archive ]; then
  [ $# -eq 3 ] || { echo "usage: $0 --archive ARCHIVE SCRATCH" >&2; exit 2; }
  archive=$2
  shift 2
else
  [ $# -eq 2 ] || { echo "usage: $0 QLD SCRATCH" >&2; exit 2; }
  QLD=$1
  shift
fi
SCRATCH=$1
mkdir -p "$SCRATCH"
SCRATCH=$(cd "$SCRATCH" && pwd)
CC=${CC:-gcc}
CLANG=${CLANG:-clang}
require=${QLD_REQUIRE_PACKAGING_TOOLS:-}
os=$(uname -s)

failures=0
pass() { echo "PASS $*"; }
fail() { echo "FAIL $*"; failures=$((failures + 1)); }
skip() {
  if [ -n "$require" ]; then
    fail "$* (QLD_REQUIRE_PACKAGING_TOOLS is set)"
  else
    echo "SKIP $*"
  fi
}
have() { command -v "$1" >/dev/null 2>&1; }

# linked_by_qld FILE: ELF output carries `Linker: qld VERSION` in .comment.
# (Mach-O has no such note; there the -### lookup is the evidence.)
linked_by_qld() {
  LC_ALL=C grep -a -q 'Linker: qld [0-9]' "$1"
}

# same_file A B: both names resolve to the same file.
same_file() {
  [ -e "$1" ] && [ -e "$2" ] && cmp -s "$1" "$2"
}

# check_run LABEL PROGRAM: PROGRAM prints the greeting (and, on ELF hosts,
# was linked by qld).
check_run() {
  if [ ! -x "$2" ]; then
    fail "$1: no output"
    return
  fi
  if [ "$os" != Darwin ] && ! linked_by_qld "$2"; then
    fail "$1: not linked by qld (the driver used another linker)"
    return
  fi
  out=$("$2" 2>&1) || true
  if [ "$out" = "hello from qld" ]; then
    pass "$1"
  else
    fail "$1: program printed '$out'"
  fi
}

# check_tree LABEL BINDIR LIBEXECDIR
check_tree() {
  label=$1
  bin=$2
  libexec=$3
  work=$SCRATCH/work-$label
  rm -rf "$work"
  mkdir -p "$work"
  printf '#include <stdio.h>\nint main(void) { puts("hello from qld"); return 0; }\n' > "$work/hello.c"

  echo "== $label: $bin"
  if [ -x "$bin/qld" ] && "$bin/qld" --version | grep -q '^qld '; then
    pass "$label: bin/qld runs"
  else
    fail "$label: bin/qld does not run"
    return
  fi
  for name in "$bin/ld.qld" "$bin/ld64.qld" "$libexec/qld/ld"; do
    if same_file "$name" "$bin/qld"; then
      pass "$label: ${name#"$SCRATCH"/} is qld"
    else
      fail "$label: ${name#"$SCRATCH"/} does not resolve to bin/qld"
    fi
  done

  # Flavor by name: -arch is an ld64 option; GNU ld reads it as -a rch.
  if "$bin/ld64.qld" -arch arm64 -v >/dev/null 2>&1; then
    pass "$label: ld64.qld is the ld64 flavor"
  else
    fail "$label: ld64.qld rejects ld64 options"
  fi
  if "$bin/ld.qld" -arch arm64 -v >/dev/null 2>&1; then
    fail "$label: ld.qld accepts ld64 options"
  else
    pass "$label: ld.qld is the GNU flavor"
  fi

  # clang for a Darwin target asks for ld64.<name>, elsewhere for ld.<name>.
  if have "$CLANG"; then
    if PATH=$bin:$PATH "$CLANG" --target=arm64-apple-macos11 -fuse-ld=qld -### \
         "$work/hello.c" -o "$work/hello-darwin" 2>&1 | grep -q "\"$bin/ld64.qld\""; then
      pass "$label: clang -fuse-ld=qld for Darwin runs ld64.qld"
    else
      fail "$label: clang -fuse-ld=qld for Darwin does not run $bin/ld64.qld"
    fi
  fi

  case "$os" in
    Linux) check_linux "$label" "$bin" "$libexec" "$work" ;;
    Darwin) check_darwin "$label" "$bin" "$work" ;;
    *) echo "SKIP $label: no driver checks on $os" ;;
  esac
}

check_linux() {
  label=$1
  bin=$2
  libexec=$3
  work=$4
  if have "$CC"; then
    rm -f "$work/hello-gcc-B"
    "$CC" -B"$libexec/qld" "$work/hello.c" -o "$work/hello-gcc-B" || true
    check_run "$label: $CC -B libexec/qld" "$work/hello-gcc-B"

    rm -f "$work/hello-gcc-fuse"
    if PATH=$bin:$PATH "$CC" -fuse-ld=qld "$work/hello.c" -o "$work/hello-gcc-fuse" \
         2> "$work/gcc-fuse.err"; then
      check_run "$label: $CC -fuse-ld=qld" "$work/hello-gcc-fuse"
    elif grep -q 'fuse-ld=qld' "$work/gcc-fuse.err"; then
      echo "NOTE $label: $("$CC" -dumpfullversion 2>/dev/null || "$CC" -dumpversion) rejects -fuse-ld=qld as expected; use -B$libexec/qld"
    else
      fail "$label: $CC -fuse-ld=qld failed: $(cat "$work/gcc-fuse.err")"
    fi
  else
    skip "$label: $CC not found"
  fi

  if have "$CLANG"; then
    rm -f "$work/hello-clang-path"
    PATH=$bin:$PATH "$CLANG" -fuse-ld=qld "$work/hello.c" -o "$work/hello-clang-path" || true
    check_run "$label: clang -fuse-ld=qld (ld.qld on PATH)" "$work/hello-clang-path"

    rm -f "$work/hello-clang-B"
    "$CLANG" -B"$bin" -fuse-ld=qld "$work/hello.c" -o "$work/hello-clang-B" || true
    check_run "$label: clang -B bin -fuse-ld=qld" "$work/hello-clang-B"

    rm -f "$work/hello-clang-ldpath"
    "$CLANG" --ld-path="$bin/ld.qld" "$work/hello.c" -o "$work/hello-clang-ldpath" || true
    check_run "$label: clang --ld-path=bin/ld.qld" "$work/hello-clang-ldpath"
  else
    skip "$label: $CLANG not found"
  fi
}

check_darwin() {
  label=$1
  bin=$2
  work=$3
  # gcc on macOS is usually clang; libexec/qld/ld is not checked there,
  # since a linker run as `ld` takes the GNU command line.
  if have "$CLANG"; then
    if PATH=$bin:$PATH "$CLANG" -fuse-ld=qld -### "$work/hello.c" -o "$work/hello" 2>&1 |
         grep -q "\"$bin/ld64.qld\""; then
      pass "$label: clang -fuse-ld=qld runs ld64.qld"
    else
      fail "$label: clang -fuse-ld=qld does not run $bin/ld64.qld"
    fi
    rm -f "$work/hello-clang-path"
    PATH=$bin:$PATH "$CLANG" -fuse-ld=qld "$work/hello.c" -o "$work/hello-clang-path" || true
    check_run "$label: clang -fuse-ld=qld (ld64.qld on PATH)" "$work/hello-clang-path"
    rm -f "$work/hello-clang-ldpath"
    "$CLANG" --ld-path="$bin/ld64.qld" "$work/hello.c" -o "$work/hello-clang-ldpath" || true
    check_run "$label: clang --ld-path=bin/ld64.qld" "$work/hello-clang-ldpath"
  else
    skip "$label: $CLANG not found"
  fi
}

extract() {
  rm -rf "$SCRATCH/prefix"
  mkdir -p "$SCRATCH/prefix"
  tar -xzf "$1" -C "$SCRATCH/prefix"
  set -- "$SCRATCH"/prefix/qld-*
  [ -d "$1" ] || { fail "archive: no qld-* directory in it"; return 1; }
  extracted=$1
  for f in LICENSE README.md; do
    if [ -f "$extracted/$f" ]; then pass "archive: has $f"; else fail "archive: no $f"; fi
  done
}

if [ -n "$archive" ]; then
  extract "$archive"
  check_tree archive "$extracted/bin" "$extracted/libexec"
else
  case "$QLD" in /*) ;; *) QLD=$(pwd)/$QLD ;; esac
  rm -rf "$SCRATCH/destdir"
  sh "$top/packaging/install.sh" --prefix /usr --destdir "$SCRATCH/destdir" "$QLD"
  check_tree destdir "$SCRATCH/destdir/usr/bin" "$SCRATCH/destdir/usr/libexec"

  rm -rf "$SCRATCH/dist"
  made=$(sh "$top/packaging/dist.sh" "$QLD" test 0.0.0 "$SCRATCH/dist")
  if (cd "$SCRATCH/dist" && sha256sum -c "$(basename "$made").sha256" >/dev/null 2>&1 ||
      shasum -a 256 -c "$(basename "$made").sha256" >/dev/null 2>&1); then
    pass "archive: checksum file matches"
  else
    fail "archive: checksum file does not match"
  fi
  extract "$made"
  check_tree archive "$extracted/bin" "$extracted/libexec"
fi

echo
if [ "$failures" -ne 0 ]; then
  echo "packaging.sh: $failures check(s) failed"
  exit 1
fi
echo "packaging.sh: all checks passed"
