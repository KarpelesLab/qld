#!/bin/sh
# Regenerates the prebuilt PE link fixtures in this directory, for the
# tests that must also run where no cross toolchain is installed (the
# Windows ARM64 CI runner links them with nothing but qld).
#
# Needs clang and the LLVM binutils with the AArch64 and X86 targets.
# `LLVM` names their directory when they are not on PATH.
set -eu
cd "$(dirname "$0")"
llvm=${LLVM:+$LLVM/}

# i386 SafeSEH: the handler registered, and a decoy registered instead.
${llvm}llvm-mc -triple i686-windows-gnu -filetype=obj safeseh-i386.s -o safeseh-i386.o
${llvm}llvm-mc -triple i686-windows-gnu -filetype=obj --defsym UNREGISTERED=1 \
    safeseh-i386.s -o safeseh-i386-unregistered.o

# ARM64: a freestanding program, its branch targets, TLS and kernel32.
flags="--target=aarch64-w64-mingw32 -O2 -ffreestanding -fno-builtin -funwind-tables"
${llvm}clang $flags -c arm64-main.c -o arm64-main.o
${llvm}clang $flags -c arm64-tls.c -o arm64-tls.o
${llvm}llvm-mc -triple aarch64-windows-gnu -filetype=obj arm64-near.s -o arm64-near.o
${llvm}llvm-mc -triple aarch64-windows-gnu -filetype=obj arm64-far.s -o arm64-far.o
${llvm}llvm-dlltool -m arm64 -d kernel32-arm64.def -l libkernel32-arm64.a

# i386 native TLS (SECREL through `__tls_index`), linked with GCC's runtime.
${llvm}clang --target=i686-w64-mingw32 -O2 -c tls-native.c -o tls-native-i386.o
