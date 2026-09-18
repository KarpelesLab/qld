#!/bin/sh
# Build musl from source (its libc.so is also its dynamic linker), then link
# C programs against it with qld and run them: a small library/executable
# pair under musl's ld.so (with TLS, symbol versioning-free exports, weak
# symbols, dlopen and copy relocations); C programs linked statically
# (non-PIE and static PIE) and dynamically by qld and by GNU ld from the
# same objects, which must behave the same; and zlib's static and shared
# libraries with their test programs.
# Usage: tests/projects/musl.sh /path/to/qld /path/to/scratch
# JOBS sets make's parallelism (default: the number of CPUs).
QLD=$1
SCRATCH=$2
MUSL_VERSION=${MUSL_VERSION:-1.2.5}
ZLIB_VERSION=${ZLIB_VERSION:-1.3.1}
. "$(dirname "$0")/common.sh"
JOBS=${JOBS:-$(nproc)}

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
    make -j"$JOBS" > make.log 2>&1 && make install > install.log 2>&1)
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
# C programs linked with qld and with GNU ld from the same objects, as
# static executables (non-PIE and static PIE) and as dynamic ones (PIE and
# non-PIE, under musl's ld.so): both must run and print the same thing.
# They cover stdio and formatting, malloc, qsort and strings, libm, TLS with
# threads, setjmp/longjmp, signals, constructors/destructors and atexit,
# environment and file I/O, and a weak reference that stays undefined.
cat > basics.c <<'EOF'
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>
#include <setjmp.h>
#include <signal.h>
#include <locale.h>
static int cmp(const void *a, const void *b) { return *(const int *)a - *(const int *)b; }
static jmp_buf env;
static volatile sig_atomic_t got;
static void on_signal(int s) { got = s; }
static void jump(int n) { longjmp(env, n + 1); }
extern int never_defined_qld(void) __attribute__((weak));
int main(int argc, char **argv) {
  int v[] = {5, 3, 9, 1, 7};
  char buf[64], *p;
  qsort(v, 5, sizeof v[0], cmp);
  printf("sorted %d %d %d %d %d\n", v[0], v[1], v[2], v[3], v[4]);
  snprintf(buf, sizeof buf, "%.3f|%e|%x|%-5s|", 3.14159, 12345.678, 255, "ab");
  puts(buf);
  printf("strtod %.2f strtol %ld\n", strtod("2.5e1", 0), strtol("-0x1f", 0, 16));
  printf("libm %.4f %.4f %.4f\n", sin(1.0), pow(2.0, 0.5), log(10.0));
  p = malloc(1 << 20);
  memset(p, 'x', 1 << 20);
  p = realloc(p, 2 << 20);
  printf("malloc %c %zu\n", p[12345], strlen(strcpy(buf, "copied")));
  free(p);
  if (setjmp(env) == 0) jump(41); else printf("longjmp ok\n");
  signal(SIGUSR1, on_signal);
  raise(SIGUSR1);
  printf("signal %d\n", (int)got == SIGUSR1);
  printf("weak %s\n", never_defined_qld ? "defined" : "undefined");
  printf("args %d %s\n", argc, strrchr(argv[0], '/') ? "path" : "name");
  setlocale(LC_ALL, "C");
  return 0;
}
EOF
cat > threads.c <<'EOF'
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
static __thread int tls_counter = 10;
static __thread char tls_buf[32];
static _Thread_local long tls_zero;
static void *worker(void *arg) {
  long n = (long)arg;
  int i;
  for (i = 0; i < 1000; i++) tls_counter++;
  tls_zero += n;
  snprintf(tls_buf, sizeof tls_buf, "t%ld", n);
  return (void *)(long)(tls_counter + tls_zero);
}
static int order;
static void __attribute__((constructor)) ctor(void) { order = order * 10 + 1; }
static void __attribute__((destructor)) dtor(void) { printf("dtor %d\n", order); }
static void at_exit(void) { printf("atexit\n"); }
int main(void) {
  pthread_t t[4];
  long sum = 0, r;
  int i;
  order = order * 10 + 2;
  atexit(at_exit);
  for (i = 0; i < 4; i++) pthread_create(&t[i], 0, worker, (void *)(long)i);
  for (i = 0; i < 4; i++) { pthread_join(t[i], (void **)&r); sum += r; }
  printf("threads %ld main %d %ld order %d\n", sum, tls_counter, tls_zero, order);
  return 0;
}
EOF
cat > files.c <<'EOF'
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <time.h>
int main(void) {
  char name[] = "/tmp/qld-musl-XXXXXX", line[64];
  int fd = mkstemp(name);
  FILE *f = fdopen(fd, "w+");
  struct timespec ts;
  fprintf(f, "line one\nline two\n");
  rewind(f);
  while (fgets(line, sizeof line, f)) printf("read %s", line);
  fclose(f);
  unlink(name);
  errno = 0;
  printf("fopen missing: %s\n", fopen("/nonexistent/qld", "r") ? "yes" : strerror(errno));
  setenv("QLD_MUSL", "value", 1);
  printf("env %s\n", getenv("QLD_MUSL"));
  printf("clock %d\n", clock_gettime(CLOCK_MONOTONIC, &ts) == 0);
  return 3;
}
EOF

gnu_cc="$prefix/bin/musl-gcc -fuse-ld=bfd"
for prog in basics threads files; do
  $cc -O2 -c -o "$prog.o" "$prog.c"
  $cc -O2 -fPIE -c -o "$prog-pie.o" "$prog.c"
  for mode in static static-pie pie no-pie; do
    case $mode in
      static) flags="-static -no-pie"; obj=$prog.o ;;
      static-pie) flags="-static-pie"; obj=$prog-pie.o ;;
      pie) flags="-pie"; obj=$prog-pie.o ;;
      no-pie) flags="-no-pie"; obj=$prog.o ;;
    esac
    libs="-lm"
    [ "$prog" = threads ] && libs="-lpthread"
    $cc $flags -o "qld-$prog-$mode" "$obj" $libs
    $gnu_cc $flags -o "gnu-$prog-$mode" "$obj" $libs
    for linker in qld gnu; do
      set +e
      "./$linker-$prog-$mode" > "$linker-$prog-$mode.out" 2>&1
      echo "exit $?" >> "$linker-$prog-$mode.out"
      set -e
    done
    if cmp -s "qld-$prog-$mode.out" "gnu-$prog-$mode.out"; then
      echo "PASS: $prog $mode: $(tr '\n' ' ' < "qld-$prog-$mode.out")"
    else
      echo "FAIL: $prog $mode differs from GNU ld:"
      diff "gnu-$prog-$mode.out" "qld-$prog-$mode.out" || true
      status=1
    fi
    # The same segments, in the same order, as GNU ld's.
    readelf -lW "gnu-$prog-$mode" | awk '/^  [A-Z]/ { print $1 }' > "gnu-$prog-$mode.segments"
    readelf -lW "qld-$prog-$mode" | awk '/^  [A-Z]/ { print $1 }' > "qld-$prog-$mode.segments"
    if ! cmp -s "gnu-$prog-$mode.segments" "qld-$prog-$mode.segments"; then
      echo "NOTE: $prog $mode program headers differ from GNU ld's:" \
        "$(tr '\n' ' ' < "gnu-$prog-$mode.segments") vs $(tr '\n' ' ' < "qld-$prog-$mode.segments")"
    fi
  done
done
rm -f gnu-*
check_linked_by_qld . || status=1

# zlib's shared library and tests against musl.
tarball=$(fetch "https://zlib.net/fossils/zlib-$ZLIB_VERSION.tar.gz")
tar -xf "$tarball"
(cd "zlib-$ZLIB_VERSION" && CC="$cc" ./configure > configure.log 2>&1 &&
  make -j"$JOBS" > make.log 2>&1 && make test > test.log 2>&1) || status=1
grep -E '\*\*\*' "zlib-$ZLIB_VERSION/test.log"
readelf -d "zlib-$ZLIB_VERSION/examplesh" | grep -E 'NEEDED|INTERP' || true
readelf -l "zlib-$ZLIB_VERSION/examplesh" | grep -o 'interpreter: [^]]*'
check_linked_by_qld "zlib-$ZLIB_VERSION" || status=1
t2=$(now)
echo "musl build $((t1 - t0))s, tests $((t2 - t1))s, status $status"
exit $status
