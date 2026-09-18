#!/bin/sh
# M8 exit criterion: real C and C++ projects built on macOS by Apple clang
# with `-fuse-ld=<qld>`, then tested, on arm64 and (under Rosetta) x86_64.
# The C, C++ and Objective-C programs of tests/data/macho_link/suite are
# built and run by `cargo test --test macho_link fuse_ld_suite`; this
# script adds downloaded projects:
#
#   zlib    static and shared libz, `make test`
#   lua     the interpreter and the Lua 5.4 test suite, with its C modules
#           built as bundles (`-bundle -undefined dynamic_lookup`)
#   sqlite  the amalgamation's shell, static and against libsqlite3.dylib,
#           running macos-suite.sql; the output must match the same objects
#           linked by Apple's linker
#   fmt     {fmt} as a dylib and its GoogleTest suite (CMake, ctest)
#
# Usage: tests/projects/macos-suite.sh QLD SCRATCH [PROJECT...]
#   QLD      the qld binary
#   SCRATCH  a scratch directory (downloads, builds, logs)
#   PROJECT  zlib, lua, sqlite, fmt (default: all of them)
# Environment:
#   QLD_SUITE_ARCHS  architectures to build (default: "arm64 x86_64")
#
# Every Mach-O executable, dylib and bundle the builds leave behind must
# have been linked by qld: the `ld64.qld` wrapper logs each link's output.
# Exits non-zero when anything fails, after a summary.

set -eu

if [ $# -lt 2 ]; then
  echo "usage: $0 QLD SCRATCH [zlib|lua|sqlite|fmt]..." >&2
  exit 2
fi
QLD=$1
SCRATCH=$2
shift 2
case "$QLD" in /*) ;; *) QLD="$(pwd)/$QLD" ;; esac
case "$SCRATCH" in /*) ;; *) SCRATCH="$(pwd)/$SCRATCH" ;; esac
HERE=$(cd "$(dirname "$0")" && pwd)
PROJECTS=${*:-zlib lua sqlite fmt}
ARCHS=${QLD_SUITE_ARCHS:-arm64 x86_64}
JOBS=$(sysctl -n hw.ncpu 2>/dev/null || echo 4)

ZLIB_VERSION=1.3.1
LUA_VERSION=5.4.7
SQLITE_VERSION=3460100
SQLITE_YEAR=2024
# 12.x: 11.0.2 specializes std::is_floating_point in a test, which the
# libc++ of Xcode 26 rejects.
FMT_VERSION=12.2.0

mkdir -p "$SCRATCH/bin" "$SCRATCH/src" "$SCRATCH/build"
LINKER="$SCRATCH/bin/ld64.qld"
LINK_LOG="$SCRATCH/links.log"
: > "$LINK_LOG"

# The linker the compilers are pointed at: records the absolute path of
# each output, then runs qld with the ld64 flavor.
cat > "$LINKER.tmp" <<EOF
#!/bin/sh
out=a.out
prev=
for a in "\$@"; do
  if [ "\$prev" = "-o" ]; then out=\$a; fi
  prev=\$a
done
dir=\$(dirname "\$out")
if [ -d "\$dir" ]; then
  echo "\$(cd "\$dir" && pwd -P)/\$(basename "\$out")" >> "$LINK_LOG"
fi
exec "$QLD" -flavor darwin "\$@"
EOF
chmod +x "$LINKER.tmp"
mv -f "$LINKER.tmp" "$LINKER"

# Downloads $1 into $SCRATCH/src unless it is already there; prints the path.
fetch() {
  f="$SCRATCH/src/$(basename "$1")"
  if [ ! -f "$f" ]; then
    curl -fsSL --retry 3 -o "$f.part" "$1"
    mv "$f.part" "$f"
  fi
  printf '%s\n' "$f"
}

# Lists the Mach-O images under $1 that qld did not link; fails if any.
check_linked_by_qld() {
  bad=0
  total=0
  for f in $(find "$1" -type f \( -perm -u+x -o -name '*.dylib' -o -name '*.so' -o -name '*.bundle' \) \
    -not -path '*/reference/*' -not -path '*/CMakeFiles/*' 2>/dev/null); do
    case "$(file -b "$f")" in
      Mach-O*executable* | Mach-O*shared\ library* | Mach-O*bundle*) ;;
      *) continue ;;
    esac
    total=$((total + 1))
    real="$(cd "$(dirname "$f")" && pwd -P)/$(basename "$f")"
    if ! grep -qxF "$real" "$LINK_LOG"; then
      echo "not linked by qld: $f"
      bad=$((bad + 1))
    fi
  done
  echo "linked-by-qld check: $total Mach-O images, $bad not linked by qld"
  [ "$total" -gt 0 ] && [ "$bad" -eq 0 ]
}

build_zlib() {
  tar -xf "$(fetch "https://zlib.net/fossils/zlib-$ZLIB_VERSION.tar.gz")" --strip-components=1
  CC="$CC" CFLAGS="-O2" ./configure > configure.log 2>&1 || { tail -30 configure.log; return 1; }
  make -j"$JOBS" > make.log 2>&1 || { tail -30 make.log; return 1; }
  make test > check.log 2>&1 || { tail -30 check.log; return 1; }
  grep '\*\*\*' check.log || true
  # static and shared, each with example and minigzip
  [ "$(grep -c 'test OK' check.log)" -ge 2 ] || return 1
  otool -L libz.*.dylib examplesh | grep -q libz || return 1
  check_linked_by_qld .
}

build_lua() {
  tar -xf "$(fetch "https://www.lua.org/ftp/lua-$LUA_VERSION.tar.gz")" --strip-components=1
  tar -xf "$(fetch "https://www.lua.org/tests/lua-$LUA_VERSION-tests.tar.gz")"
  make -C src -j"$JOBS" macosx CC="$CC" > make.log 2>&1 || { tail -30 make.log; return 1; }
  # The test suite's C modules, loaded with package.loadlib and require.
  make -C "lua-$LUA_VERSION-tests/libs" CC="$CC" LUA_DIR="$(pwd)/src" \
    CFLAGS="-Wall -std=gnu99 -O2 -I$(pwd)/src -bundle -undefined dynamic_lookup" \
    > make-libs.log 2>&1 || { tail -30 make-libs.log; return 1; }
  # The portable tests (_U), without the internal C API tests.
  (cd "lua-$LUA_VERSION-tests" && ../src/lua -e"_U=true" all.lua) > check.log 2>&1 ||
    { tail -30 check.log; return 1; }
  tail -3 check.log
  grep -q 'final OK' check.log || return 1
  if grep -q 'cannot load dynamic library' check.log; then
    echo "the C modules did not load"
    return 1
  fi
  check_linked_by_qld .
}

build_sqlite() {
  name="sqlite-amalgamation-$SQLITE_VERSION"
  unzip -qo "$(fetch "https://www.sqlite.org/$SQLITE_YEAR/$name.zip")"
  flags="-O2 -DSQLITE_ENABLE_FTS5 -DSQLITE_ENABLE_RTREE -DSQLITE_ENABLE_MATH_FUNCTIONS"
  # Compiled once, linked by qld and (in reference/) by Apple's linker.
  set -x
  $CC $flags -c "$name/sqlite3.c" -o sqlite3.o
  $CC $flags -c "$name/shell.c" -o shell.o
  $CC sqlite3.o shell.o -o sqlite3
  $CC -Wl,-dead_strip sqlite3.o shell.o -o sqlite3-dead-strip
  $CC -dynamiclib sqlite3.o -install_name @rpath/libsqlite3.dylib -o libsqlite3.dylib
  $CC shell.o -L. -lsqlite3 -Wl,-rpath,@executable_path -o sqlite3-shared
  mkdir -p reference
  $APPLE_CC sqlite3.o shell.o -o reference/sqlite3
  set +x
  for shell in reference/sqlite3 sqlite3 sqlite3-dead-strip sqlite3-shared; do
    out="$(echo "$shell" | tr / -).out"
    rm -f test.db test.db-wal test.db-shm
    "./$shell" test.db < "$HERE/macos-suite.sql" > "$out" 2>&1 ||
      { echo "$shell failed:"; tail -20 "$out"; return 1; }
  done
  grep -qx ok reference-sqlite3.out || { cat reference-sqlite3.out; return 1; }
  status=0
  for out in sqlite3.out sqlite3-dead-strip.out sqlite3-shared.out; do
    if ! diff -u reference-sqlite3.out "$out"; then
      echo "$out differs from the output of the shell Apple's linker linked"
      status=1
    fi
  done
  [ "$status" -eq 0 ] || return 1
  echo "4 shells, same output"
  check_linked_by_qld .
}

build_fmt() {
  unzip -qo "$(fetch "https://github.com/fmtlib/fmt/releases/download/$FMT_VERSION/fmt-$FMT_VERSION.zip")"
  cmake -S "fmt-$FMT_VERSION" -B build -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_C_COMPILER=clang -DCMAKE_CXX_COMPILER=clang++ \
    -DCMAKE_OSX_ARCHITECTURES="$ARCH" -DBUILD_SHARED_LIBS=ON -DFMT_TEST=ON \
    -DCMAKE_EXE_LINKER_FLAGS="-fuse-ld=$LINKER" \
    -DCMAKE_SHARED_LINKER_FLAGS="-fuse-ld=$LINKER" \
    -DCMAKE_MODULE_LINKER_FLAGS="-fuse-ld=$LINKER" > configure.log 2>&1 ||
    { tail -30 configure.log; return 1; }
  cmake --build build -j"$JOBS" > make.log 2>&1 || { tail -40 make.log; return 1; }
  (cd build && ctest -j"$JOBS" --output-on-failure) > check.log 2>&1 ||
    { tail -40 check.log; return 1; }
  grep 'tests passed' check.log || true
  check_linked_by_qld build
}

results=
failed=0
for arch in $ARCHS; do
  if [ "$arch" = x86_64 ] && [ "$(uname -m)" = arm64 ] && ! arch -x86_64 /usr/bin/true 2>/dev/null; then
    echo "x86_64 needs Rosetta: softwareupdate --install-rosetta --agree-to-license" >&2
    exit 1
  fi
  ARCH=$arch
  CC="clang -arch $arch -fuse-ld=$LINKER"
  APPLE_CC="clang -arch $arch"
  export CC
  for project in $PROJECTS; do
    dir="$SCRATCH/build/$project-$arch"
    rm -rf "$dir"
    mkdir -p "$dir"
    echo "=== $project ($arch) in $dir"
    start=$(date +%s)
    set +e
    (cd "$dir" && set -e && "build_$project")
    status=$?
    set -e
    seconds=$(($(date +%s) - start))
    if [ "$status" -eq 0 ]; then
      result=PASS
    else
      result=FAIL
      failed=$((failed + 1))
    fi
    echo "=== $project ($arch): $result in ${seconds}s"
    results="$results$(printf '%-8s %-7s %s %5ss' "$project" "$arch" "$result" "$seconds")
"
  done
done

echo
echo "Summary (qld: $("$QLD" --version | head -1))"
printf '%s' "$results"
[ "$failed" -eq 0 ]
