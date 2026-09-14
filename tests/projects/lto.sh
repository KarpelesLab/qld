#!/bin/sh
# Build a project with LTO, qld as the linker, and run its test suite
# (roadmap M6).
# Usage: tests/projects/lto.sh PROJECT /path/to/qld /path/to/scratch [LTO] [qld|gnu]
#   PROJECT  zlib, lua, curl, openssl, coreutils or python
#   LTO      gcc (gcc -flto=auto, gcc-ar; the default), clang (clang -flto,
#            llvm-ar) or thin (clang -flto=thin)
# With QLD_LINK_COMPARE=DIR, every link over QLD_LINK_COMPARE_MIN bytes is
# also done with GNU ld, which runs the same plugin (see ldcompare.py).
PROJECT=$1
QLD=$2
SCRATCH=$3
LTO=${4:-gcc}
MODE=${5:-qld}
. "$(dirname "$0")/common.sh"

case "$LTO" in
  gcc)
    flto=-flto=auto
    ar=gcc-ar ranlib=gcc-ranlib nm=gcc-nm
    if [ "$MODE" = gnu ]; then cc=gcc cxx=g++; else cc="$QLD_BIN/qcc" cxx="$QLD_BIN/qc++"; fi
    ;;
  clang | thin)
    if [ "$LTO" = thin ]; then flto=-flto=thin; else flto=-flto; fi
    ar=llvm-ar ranlib=llvm-ranlib nm=llvm-nm
    if [ "$MODE" = gnu ]; then
      cc="clang -fuse-ld=bfd" cxx="clang++ -fuse-ld=bfd"
    else
      cc="$QLD_BIN/qclang" cxx="$QLD_BIN/qclang++"
    fi
    ;;
  *) echo "unknown LTO mode $LTO" >&2; exit 2 ;;
esac
jobs=${JOBS:-$(nproc)}

# Counts the objects under the given directories that hold IR: LLVM bitcode,
# or ELF objects with GCC .gnu.lto_ sections (slim or fat). qld refuses IR
# it cannot hand to a plugin, and claims fat objects when a plugin is loaded.
count_ir_objects() {
  ir=0
  total=0
  for f in $(find "$@" -type f -name '*.o' 2>/dev/null); do
    total=$((total + 1))
    if [ "$(head -c 4 "$f" | od -An -tx1 | tr -d ' ')" = 4243c0de ] ||
      grep -q "\.gnu\.lto_" "$f" 2>/dev/null; then
      ir=$((ir + 1))
    fi
  done
  echo "IR check: $ir of $total objects are LTO IR"
}

dir="$SCRATCH/build/$PROJECT-lto-$LTO-$MODE"
rm -rf "$dir"
mkdir -p "$dir"
export QLD_LINK_LOG="$dir/links.log"
t0=$(now)
set +e
case "$PROJECT" in
  zlib)
    version=${ZLIB_VERSION:-1.3.1}
    tar -xf "$(fetch "https://zlib.net/fossils/zlib-$version.tar.gz")" -C "$dir" --strip-components=1
    cd "$dir" || exit 1
    CC="$cc" CFLAGS="-O2 $flto" LDFLAGS="$flto" AR="$ar" RANLIB="$ranlib" \
      ./configure > configure.log 2>&1 || exit 1
    make -j"$jobs" > make.log 2>&1 || { tail -30 make.log; exit 1; }
    t1=$(now)
    make test > check.log 2>&1
    status=$?
    grep -E '\*\*\*' check.log
    outputs=.
    ;;
  lua)
    version=${LUA_VERSION:-5.4.7}
    tar -xf "$(fetch "https://www.lua.org/ftp/lua-$version.tar.gz")" -C "$dir" --strip-components=1
    tar -xf "$(fetch "https://www.lua.org/tests/lua-$version-tests.tar.gz")" -C "$dir"
    cd "$dir" || exit 1
    make -j"$jobs" linux CC="$cc" MYCFLAGS="$flto" MYLDFLAGS="$flto" \
      AR="$ar rc" RANLIB="$ranlib" > make.log 2>&1 || { tail -30 make.log; exit 1; }
    t1=$(now)
    # The portable tests (_U), with the internal C API tests left out.
    (cd "lua-$version-tests" && ../src/lua -e"_U=true" all.lua) > check.log 2>&1
    status=$?
    tail -5 check.log
    outputs=src
    ;;
  curl)
    version=${CURL_VERSION:-8.22.0}
    tar -xf "$(fetch "https://curl.se/download/curl-$version.tar.xz")" -C "$dir" --strip-components=1
    cd "$dir" || exit 1
    ./configure CC="$cc" CFLAGS="-O2 $flto" LDFLAGS="$flto" AR="$ar" RANLIB="$ranlib" NM="$nm" \
      --with-openssl --enable-shared --disable-static > configure.log 2>&1 || exit 1
    make -j"$jobs" > make.log 2>&1 || { tail -30 make.log; exit 1; }
    make -j"$jobs" -C tests > make-tests.log 2>&1 || { tail -30 make-tests.log; exit 1; }
    t1=$(now)
    make test TFLAGS="-j$jobs -a" > check.log 2>&1
    status=$?
    grep -E '^(TESTFAIL|TESTDONE)' check.log
    outputs="lib src tests"
    ;;
  openssl)
    version=${OPENSSL_VERSION:-3.6.4}
    tar -xf "$(fetch "https://github.com/openssl/openssl/releases/download/openssl-$version/openssl-$version.tar.gz")" -C "$dir" --strip-components=1
    cd "$dir" || exit 1
    ./Configure CC="$cc" CFLAGS="-O2 $flto" LDFLAGS="$flto" AR="$ar" RANLIB="$ranlib" \
      > configure.log 2>&1 || exit 1
    make -j"$jobs" > make.log 2>&1 || { tail -30 make.log; exit 1; }
    t1=$(now)
    make test HARNESS_JOBS="$jobs" > check.log 2>&1
    status=$?
    grep -E '^(Files=|Result:)' check.log
    outputs=.
    ;;
  coreutils)
    version=${COREUTILS_VERSION:-9.11}
    tar -xf "$(fetch "https://ftp.gnu.org/gnu/coreutils/coreutils-$version.tar.xz")" -C "$dir" --strip-components=1
    cd "$dir" || exit 1
    ./configure CC="$cc" CFLAGS="-O2 $flto" LDFLAGS="$flto" AR="$ar" RANLIB="$ranlib" \
      > configure.log 2>&1 || exit 1
    make -j"$jobs" > make.log 2>&1 || { tail -30 make.log; exit 1; }
    t1=$(now)
    make -k -j"$jobs" check > check.log 2>&1
    status=$?
    grep -E '^# (TOTAL|PASS|SKIP|XFAIL|FAIL|XPASS|ERROR):' tests/test-suite.log gnulib-tests/test-suite.log
    grep -E '^(FAIL|ERROR):' check.log
    outputs=src
    ;;
  python)
    # Python's own LTO configuration: GCC fat objects (-ffat-lto-objects
    # -flto-partition=none), or clang's full or thin LTO.
    version=${PYTHON_VERSION:-3.14.7}
    tar -xf "$(fetch "https://www.python.org/ftp/python/$version/Python-$version.tar.xz")" -C "$dir" --strip-components=1
    cd "$dir" || exit 1
    case "$LTO" in gcc) with_lto=--with-lto=yes ;; clang) with_lto=--with-lto=full ;; thin) with_lto=--with-lto=thin ;; esac
    ./configure CC="$cc" CXX="$cxx" AR="$ar" RANLIB="$ranlib" --enable-shared "$with_lto" \
      > configure.log 2>&1 || exit 1
    make -j"$jobs" > make.log 2>&1 || { tail -30 make.log; exit 1; }
    sed -n '/^The necessary bits/,$p;/^Following modules built successfully but were removed/,$p;/^Failed to build these modules/,$p' make.log
    t1=$(now)
    LD_LIBRARY_PATH="$dir" ./python -m test -j"$jobs" --timeout 1800 -u all,-network,-urlfetch,-largefile \
      > check.log 2>&1
    status=$?
    sed -n '/^== Tests result/,$p' check.log | head -5
    grep -A30 -E '^[0-9]+ tests? failed' check.log | head -40
    outputs=.
    ;;
  *) echo "unknown project $PROJECT" >&2; exit 2 ;;
esac
t2=$(now)
# shellcheck disable=SC2086
[ "$MODE" = gnu ] || check_linked_by_qld $outputs || true
# shellcheck disable=SC2086
count_ir_objects $outputs
echo "$PROJECT $version, LTO $LTO, $MODE: build $((t1 - t0))s, test $((t2 - t1))s, test exit $status"
exit $status
