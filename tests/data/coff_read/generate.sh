#!/bin/sh
# Regenerates the prebuilt COFF reader fixtures in this directory.
# Needs a MinGW-w64 GCC toolchain, clang and the LLVM binutils.
set -eu
cd "$(dirname "$0")"

x86_64-w64-mingw32-gcc -c -O0 -fcommon mingw.c -o mingw-x86_64.o
x86_64-w64-mingw32-gcc -c -O0 -fcommon -Wa,-mbig-obj mingw.c -o mingw-bigobj.o

for target in x86_64 i686 aarch64; do
    clang --target=$target-pc-windows-msvc -c -O0 -faddrsig -Xclang -cfguard \
        -mno-incremental-linker-compatible msvc.cpp -o msvc-$target.obj
done

llvm-dlltool -m i386:x86-64 -d testlib.def -l short-import-x86_64.lib
llvm-dlltool -m i386 -d testlib.def -l short-import-i386.lib
x86_64-w64-mingw32-dlltool -d testlib.def -l long-import-x86_64.a

x86_64-w64-mingw32-gcc -O1 -shared -nostdlib -s -Wl,-e,DllMain \
    -Wl,--no-insert-timestamp testdll.c testdll.def -o testdll.dll

x86_64-w64-mingw32-windres resource.rc -o resource-windres.o
llvm-windres --target=x86_64-pc-windows-msvc resource.rc -o resource-cvtres.obj
