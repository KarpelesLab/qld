#!/bin/sh
# Regenerates the prebuilt Mach-O reader fixtures in this directory.
# Needs clang, llvm-lipo, llvm-ar and yaml2obj; no macOS SDK.
set -eu
cd "$(dirname "$0")"

clang --target=arm64-apple-macos13 -O1 -fcommon -c atoms.c -o atoms-arm64.o
clang --target=x86_64-apple-macos13 -O1 -fcommon -c atoms.c -o atoms-x86_64.o
clang --target=arm64-apple-macos13 -c alt_entry_arm64.s -o alt-entry-arm64.o
clang++ --target=arm64-apple-macos13 -O1 -c eh.cpp -o eh-arm64.o
clang++ --target=x86_64-apple-macos13 -O1 -femit-dwarf-unwind=always -c eh.cpp -o eh-dwarf-x86_64.o
clang --target=i386-apple-macos10.14 -O1 -c i386.c -o i386.o

llvm-lipo -create atoms-arm64.o atoms-x86_64.o -output atoms-fat.o
rm -f libatoms-arm64.a libatoms-x86_64.a
llvm-ar --format=darwin rcs libatoms-arm64.a atoms-arm64.o alt-entry-arm64.o
llvm-ar --format=darwin rcs libatoms-x86_64.a atoms-x86_64.o
llvm-lipo -create libatoms-arm64.a libatoms-x86_64.a -output libatoms-fat.a
rm -f libatoms-arm64.a libatoms-x86_64.a

yaml2obj libqld.yaml -o libqld-arm64.dylib
yaml2obj libchained.yaml -o libchained-x86_64.dylib
