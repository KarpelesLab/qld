#!/bin/sh
# Build musl from source (its libc.so is also its dynamic linker), then link
# shared libraries and executables against it with qld and run them under
# musl's ld.so: a small library/executable pair (with TLS, symbol
# versioning-free exports, weak symbols, dlopen and copy relocations), and
# zlib's shared library with its test programs.
# Usage: tests/projects/musl.sh /path/to/qld /path/to/scratch
QLD=$1
SCRATCH=$2
MUSL_VERSION=${MUSL_VERSION:-1.2.5}
ZLIB_VERSION=${ZLIB_VERSION:-1.3.1}
. "$(dirname "$0")/common.sh"

prefix="$SCRATCH/build/musl-prefix"
t0=$(now)
if [ ! -x "$prefix/bin/musl-gcc" ]; then
  tarball=$(fetch "https://musl.libc.org/releases/musl-$MUSL_VERSION.tar.gz")
  rm -rf "$SCRATCH/build/musl-$MUSL_VERSION"
  tar -xf "$tarball" -C "$SCRATCH/build"
  # musl itself is built with the system linker: this checks qld's outputs
  # under musl's ld.so, not a musl built by qld.
  (cd "$SCRATCH/build/musl-$MUSL_VERSION" &&
    ./configure --prefix="$prefix" --syslibdir="$prefix/lib" > configure.log 2>&1 &&
    make -j"$(nproc)" > make.log 2>&1 && make install > install.log 2>&1)
fi
t1=$(now)

dir="$SCRATCH/build/musl-qld"
rm -rf "$dir"
mkdir -p "$dir"
cd "$dir"
export QLD_LINK_LOG="$dir/links.log"
cc="$prefix/bin/musl-gcc -B$QLD_BIN"

cat > lib.c <<'EOF'
#include <stdio.h>
__thread int tls_counter = 40;
int lib_data = 7;
int lib_weak(void) __attribute__((weak));
int lib_weak(void) { return 1; }
static void init(void) __attribute__((constructor));
static void init(void) { lib_data += 1; }
int lib_add(int x) { tls_counter += x; return tls_counter; }
int lib_call_back(int (*f)(int)) { return f(lib_data); }
EOF
cat > plugin.c <<'EOF'
extern int lib_data;
int plugin_value(void) { return lib_data * 2; }
EOF
cat > main.c <<'EOF'
#include <dlfcn.h>
#include <stdio.h>
#include <string.h>
#include <pthread.h>
extern __thread int tls_counter;
extern int lib_data;
int lib_add(int);
int lib_call_back(int (*)(int));
static int twice(int v) { return 2 * v; }
static void *thread(void *arg) { (void)arg; return (void *)(long)lib_add(1); }
int main(int argc, char **argv) {
  pthread_t t;
  long from_thread;
  int (*plugin_value)(void);
  void *handle;
  (void)argc;
  pthread_create(&t, 0, thread, 0);
  pthread_join(t, (void **)&from_thread);
  handle = dlopen("./libplugin.so", RTLD_NOW);
  if (!handle) { printf("dlopen: %s\n", dlerror()); return 1; }
  plugin_value = dlsym(handle, "plugin_value");
  printf("%d %d %ld %d %d\n", lib_add(2), lib_data, from_thread, lib_call_back(twice), plugin_value());
  return strcmp(argv[0], "") == 0;
}
EOF

status=0
run_case() {
  name=$1
  shift
  if "$@" > "$name.out" 2>&1; then
    echo "PASS: $name: $(cat "$name.out")"
  else
    echo "FAIL: $name: $(cat "$name.out")"
    status=1
  fi
}

$cc -O2 -fPIC -shared -Wl,-soname,libqldtest.so -o libqldtest.so lib.c
$cc -O2 -fPIC -shared -o libplugin.so plugin.c -L. -lqldtest
for kind in pie no-pie; do
  if [ "$kind" = pie ]; then flags="-fPIE -pie"; else flags="-fno-PIE -no-pie"; fi
  $cc -O2 $flags -o "main-$kind" main.c -L. -lqldtest -Wl,-rpath,'$ORIGIN'
  run_case "main-$kind" "./main-$kind"
done
# Lazy binding and -z now, packed relative relocations.
$cc -O2 -fPIE -pie -Wl,-z,now -Wl,-z,pack-relative-relocs -o main-now main.c -L. -lqldtest -Wl,-rpath,'$ORIGIN'
run_case main-now ./main-now
check_linked_by_qld . || status=1

# zlib's shared library and tests against musl.
tarball=$(fetch "https://zlib.net/fossils/zlib-$ZLIB_VERSION.tar.gz")
tar -xf "$tarball"
(cd "zlib-$ZLIB_VERSION" && CC="$cc" ./configure > configure.log 2>&1 &&
  make -j"$(nproc)" > make.log 2>&1 && make test > test.log 2>&1) || status=1
grep -E '\*\*\*' "zlib-$ZLIB_VERSION/test.log"
readelf -d "zlib-$ZLIB_VERSION/examplesh" | grep -E 'NEEDED|INTERP' || true
readelf -l "zlib-$ZLIB_VERSION/examplesh" | grep -o 'interpreter: [^]]*'
check_linked_by_qld "zlib-$ZLIB_VERSION" || status=1
t2=$(now)
echo "musl build $((t1 - t0))s, tests $((t2 - t1))s, status $status"
exit $status
