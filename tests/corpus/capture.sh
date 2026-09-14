#!/bin/sh
# Captures the exact argv that compiler drivers pass to the linker on this
# machine, using a logging `ld` shim found through -B / --ld-path.
# Usage: capture.sh [build-dir]; then normalize.py build-dir tests/corpus
set -e
D=$(cd "$(dirname "$0")" && pwd)
B="${1:-$D/build}"
rm -rf "$B"
mkdir -p "$B/raw" "$B/shim"
cd "$B"

RUST_LLD=$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld

for name in ld ld.lld; do
  if [ "$name" = ld ]; then real='/usr/bin/ld.bfd "$@"'; else real="\"$RUST_LLD\" -flavor gnu \"\$@\""; fi
  cat > "shim/$name" <<EOF
#!/bin/sh
out=a.out
prev=
for a in "\$@"; do
  if [ "\$prev" = "-o" ]; then out=\$a; fi
  prev=\$a
done
printf '%s\n' "\$@" > "$B/raw/\$(basename "\$out").args"
exec $real
EOF
  chmod +x "shim/$name"
done
S="$B/shim"

cat > hello.c <<'EOF'
#include <stdio.h>
int main(void) { puts("hi"); return 0; }
EOF
cat > lib.cpp <<'EOF'
#include <string>
std::string greet(const std::string &n) { return "hello " + n; }
EOF
cat > main.rs <<'EOF'
fn main() { println!("hi"); }
EOF

gcc -B"$S/" -O2 -no-pie -o gcc-dynamic-c hello.c
gcc -B"$S/" -O2 -static -o gcc-static-c hello.c
gcc -B"$S/" -O2 -fPIE -pie -o gcc-pie-c hello.c
gcc -B"$S/" -O2 -static-pie -o gcc-static-pie-c hello.c
gcc -B"$S/" -O2 -flto -Wl,--gc-sections -Wl,-O1 -Wl,--as-needed -o gcc-lto-gc-sections-c hello.c
g++ -B"$S/" -O2 -fPIC -shared -Wl,-soname,libgreet.so.1 -Wl,-z,defs -o gxx-shared-lib lib.cpp
clang --ld-path="$S/ld" -O2 -o clang-pie-c hello.c
clang --ld-path="$S/ld" -O2 -static -o clang-static-c hello.c
clang++ --ld-path="$S/ld" -O2 -fPIC -shared -Wl,-soname,libgreet.so -o clangxx-shared-lib lib.cpp

# rustc's default (rust-lld through gcc -fuse-ld=lld) and GNU ld.
printf '#!/bin/sh\nexec cc -B"%s/" "$@"\n' "$S" > cc-shim-first
chmod +x cc-shim-first
rustc -O -C linker="$B/cc-shim-first" -o rustc-bin-lld main.rs
rustc -O -C linker-features=-lld -C link-arg=-B"$S/" -o rustc-bin-bfd main.rs
rustc -O -C linker-features=-lld -C link-arg=-B"$S/" --crate-type cdylib -o rustc-cdylib.so main.rs 2>/dev/null || true

# CMake: shared library with SONAME/versions, executable with an rpath.
mkdir -p cm/src cm/build
cp hello.c cm/src/main.c
cp lib.cpp cm/src/lib.cpp
cat > cm/src/CMakeLists.txt <<'EOF'
cmake_minimum_required(VERSION 3.16)
project(corpus C CXX)
add_library(greet SHARED lib.cpp)
set_target_properties(greet PROPERTIES VERSION 1.2.3 SOVERSION 1)
add_executable(app main.c)
target_link_libraries(app PRIVATE greet)
EOF
(cd cm/build && cmake -G Ninja -DCMAKE_BUILD_TYPE=Release "-DCMAKE_EXE_LINKER_FLAGS=-B$S/" "-DCMAKE_SHARED_LINKER_FLAGS=-B$S/" ../src >/dev/null && ninja >/dev/null)
mv raw/libgreet.so.1.2.3.args raw/cmake-shared-lib.args
mv raw/app.args raw/cmake-exe-rpath.args

# Meson: its default link arguments for a shared library and an executable.
mkdir -p ms/src
cp lib.cpp ms/src/lib.cpp
cp hello.c ms/src/main.c
cat > ms/src/meson.build <<EOF
project('corpus', 'c', 'cpp', default_options: ['buildtype=release'])
greet = shared_library('greet', 'lib.cpp', version: '1.0.0', link_args: ['-B$S/'])
executable('app', 'main.c', link_with: greet, link_args: ['-B$S/'])
EOF
(cd ms && meson setup build src >/dev/null && ninja -C build >/dev/null)
mv raw/libgreet.so.1.0.0.args raw/meson-shared-lib.args
mv raw/app.args raw/meson-exe.args

ls -la raw
