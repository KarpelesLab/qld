#!/bin/sh
# Captures the W24 benchmark corpus (tests/projects/bench.md) from build
# trees that already exist, without rebuilding them:
#
#   tests/projects/bench-capture.sh SPECS
#
# Environment (defaults in parentheses):
#   LLVM_STATIC  static LLVM 23 tree from llvm.sh (~/.cache/qld-projects/build/llvm-23.1.1-static)
#   LLVM_SHARED  BUILD_SHARED_LIBS tree from llvm.sh (~/.cache/qld-projects/build/llvm-23.1.1-shared)
#   LLVM_DEBUG   a RelWithDebInfo clang tree, see bench.md (~/.cache/qld-bench/build/llvm-23.1.1-debug)
#   KERNEL_TREE  the qld-linked kernel tree from kernel.sh (~/.cache/qld-verify-kernel/build/linux-7.2.5-qld)
#   QLD_SRC      a copy of the qld sources to build in debug mode (~/.cache/qld-bench/build/qld-debug)
#
# A benchmark whose tree is missing is skipped with a message. Links are
# replayed in place later by benches/run.py, so the trees must stay.
set -eu
: "${1:?usage: bench-capture.sh SPECS}"
SPECS=$1
HERE=$(cd "$(dirname "$0")" && pwd)
CAPTURE="$HERE/../../benches/capture.py"
C=$HOME/.cache
LLVM_STATIC=${LLVM_STATIC:-$C/qld-projects/build/llvm-23.1.1-static}
LLVM_SHARED=${LLVM_SHARED:-$C/qld-projects/build/llvm-23.1.1-shared}
LLVM_DEBUG=${LLVM_DEBUG:-$C/qld-bench/build/llvm-23.1.1-debug}
KERNEL_TREE=${KERNEL_TREE:-$C/qld-verify-kernel/build/linux-7.2.5-qld}
QLD_SRC=${QLD_SRC:-$C/qld-bench/build/qld-debug}
CLANG_SMOKE="{out} --version >/dev/null && echo 'int main(void){return 0;}' | {out} -x c -c - -o /dev/null"

if [ -f "$LLVM_STATIC/build.ninja" ]; then
  python3 "$CAPTURE" "$SPECS" clang --smoke "$CLANG_SMOKE" ninja "$LLVM_STATIC" bin/clang-23
  python3 "$CAPTURE" "$SPECS" small-count --smoke "printf 'a\n' | {out} 1" ninja "$LLVM_STATIC" bin/count
else
  echo "skipped clang, small-count: no $LLVM_STATIC"
fi
if [ -f "$LLVM_SHARED/build.ninja" ]; then
  python3 "$CAPTURE" "$SPECS" libclang-cpp \
    --smoke "python3 -c 'import ctypes,os,sys; ctypes.CDLL(sys.argv[1], mode=os.RTLD_NOW)' {out}" \
    --env "LD_LIBRARY_PATH=$LLVM_SHARED/lib" ninja "$LLVM_SHARED" lib/libclang-cpp.so.23.1
else
  echo "skipped libclang-cpp: no $LLVM_SHARED"
fi
if [ -f "$LLVM_DEBUG/build.ninja" ]; then
  python3 "$CAPTURE" "$SPECS" clang-debug --smoke "$CLANG_SMOKE" ninja "$LLVM_DEBUG" bin/clang-23
else
  echo "skipped clang-debug: no $LLVM_DEBUG"
fi
if [ -f "$KERNEL_TREE/vmlinux.o" ]; then
  python3 "$CAPTURE" "$SPECS" vmlinux \
    --smoke "nm {out} | grep -q ' T start_kernel\$' && readelf -lW {out} | grep -q LOAD" \
    argv --cwd "$KERNEL_TREE" -- -m elf_x86_64 --fatal-warnings -z noexecstack \
    --no-warn-rwx-segments -z max-page-size=0x200000 --build-id=sha1 \
    --orphan-handling=error --emit-relocs --discard-none \
    --script=./arch/x86/kernel/vmlinux.lds -o vmlinux.unstripped --whole-archive vmlinux.o \
    .vmlinux.export.o init/version-timestamp.o --no-whole-archive --start-group --end-group \
    .tmp_vmlinux2.kallsyms.o
else
  echo "skipped vmlinux: no $KERNEL_TREE/vmlinux.o"
fi
if [ -f "$QLD_SRC/Cargo.toml" ]; then
  # Relink qld's own debug binary: touch the crate roots so cargo links
  # again; every link also runs GNU ld so the build goes on.
  touch "$QLD_SRC/src/main.rs" "$QLD_SRC/src/lib.rs"
  (cd "$QLD_SRC" && python3 "$CAPTURE" "$SPECS" rust-qld-debug --smoke "{out} --version" cmd -- \
    sh -c 'RUSTFLAGS="-Clink-arg=-B$QLD_BENCH_SHIM -Clinker-features=-lld" cargo build --bin qld --target-dir target')
else
  echo "skipped rust-qld-debug: no $QLD_SRC (a copy of the qld sources)"
fi
