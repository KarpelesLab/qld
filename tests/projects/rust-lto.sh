#!/bin/sh
# Cross-language LTO with rustc's -C linker-plugin-lto: rustc emits LLVM
# bitcode, clang drives the link with LLVMgold.so and qld as the linker
# (roadmap M6). rustc's LLVM and the plugin's LLVM must be the same major
# version: CLANG defaults to the clang of `rustc -vV`'s LLVM version under
# /usr/lib/llvm/<major>/bin.
# Usage: tests/projects/rust-lto.sh /path/to/qld /path/to/scratch
#
# 1. A generated crate whose Rust code calls a C function compiled to
#    bitcode (-flto=thin) in a static archive: built, run, and its tests run.
# 2. qld's own library unit tests, built the same way (every test binary
#    links std's native code with qld's bitcode through the plugin).
QLD=$1
SCRATCH=$2
. "$(dirname "$0")/common.sh"
REPO=$(cd "$(dirname "$0")/../.." && pwd)

llvm_major=$(rustc -vV | sed -n 's/^LLVM version: \([0-9]*\).*/\1/p')
: "${llvm_major:?cannot find rustc's LLVM version}"
clang_dir=${CLANG_DIR:-/usr/lib/llvm/$llvm_major/bin}
CLANG=${CLANG:-$clang_dir/clang}
LLVM_AR=${LLVM_AR:-$clang_dir/llvm-ar}
echo "rustc: $(rustc -V), LLVM $llvm_major; clang: $("$CLANG" --version | head -1)"

# A clang wrapper for rustc's -C linker: the matching clang, with qld.
put_script "$QLD_BIN/qclang-rust" <<EOF
#!/bin/sh
exec "$CLANG" --ld-path="$QLD_BIN/ld" "\$@"
EOF
# rustc asks clang for its self-contained lld (-fuse-ld=lld), and clang
# passes no -plugin to lld: -Clinker-features=-lld turns that off.
export RUSTFLAGS="-Clinker-plugin-lto -Clinker=$QLD_BIN/qclang-rust -Clinker-features=-lld -Clink-arg=-flto=thin"
export XLTO_CC="$CLANG" XLTO_AR="$LLVM_AR"
jobs=${JOBS:-$(nproc)}

crate="$SCRATCH/build/rust-xlto"
rm -rf "$crate"
mkdir -p "$crate/src" "$crate/c"
export QLD_LINK_LOG="$crate/links.log"
cat > "$crate/Cargo.toml" <<'EOF'
[package]
name = "xlto"
version = "0.1.0"
edition = "2021"
EOF
cat > "$crate/build.rs" <<'EOF'
use std::process::Command;

fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    let obj = format!("{out}/mix.o");
    let lib = format!("{out}/libmix.a");
    let cc = std::env::var("XLTO_CC").unwrap();
    let ar = std::env::var("XLTO_AR").unwrap();
    let ok = Command::new(cc)
        .args(["-O2", "-flto=thin", "-fPIC", "-c", "c/mix.c", "-o", &obj])
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let _ = std::fs::remove_file(&lib);
    assert!(Command::new(ar).args(["rcs", &lib, &obj]).status().unwrap().success());
    println!("cargo:rustc-link-search=native={out}");
    println!("cargo:rustc-link-lib=static=mix");
    println!("cargo:rerun-if-changed=c/mix.c");
}
EOF
cat > "$crate/c/mix.c" <<'EOF'
#include <stdint.h>
uint64_t c_mix_qld(uint64_t a, uint64_t b) { return a * 31 + b; }
uint64_t c_unused_qld(uint64_t a) { return a ^ 0x5a5a; }
EOF
cat > "$crate/src/main.rs" <<'EOF'
extern "C" {
    fn c_mix_qld(a: u64, b: u64) -> u64;
}

fn rust_fold(values: &[u64]) -> u64 {
    values.iter().fold(7, |acc, &v| unsafe { c_mix_qld(acc, v) })
}

fn main() {
    let values: Vec<u64> = (1..=10).collect();
    println!("fold={}", rust_fold(&values));
}

#[cfg(test)]
mod tests {
    #[test]
    fn fold_matches() {
        let expected = (1..=10u64).fold(7u64, |acc, v| acc.wrapping_mul(31).wrapping_add(v));
        assert_eq!(super::rust_fold(&(1..=10).collect::<Vec<_>>()), expected);
    }
}
EOF

t0=$(now)
set +e
(cd "$crate" && cargo build --release && ./target/release/xlto && cargo test --release) > "$crate/build.log" 2>&1
crate_status=$?
grep -E '^fold=|^test result:' "$crate/build.log"
if nm "$crate/target/release/xlto" | grep -q c_unused_qld; then
  echo "c_unused_qld survived LTO"
  crate_status=1
fi
check_linked_by_qld "$crate/target/release" || true
echo "cross-language crate: exit $crate_status"
t1=$(now)

# qld's own unit tests, with the plugin LTO build.
target="$SCRATCH/build/rust-qld-lto-target"
export QLD_LINK_LOG="$SCRATCH/build/rust-qld-lto.links.log"
(cd "$REPO" && CARGO_TARGET_DIR="$target" cargo test --release --lib -j"$jobs") \
  > "$SCRATCH/build/rust-qld-lto.log" 2>&1
qld_status=$?
grep -E '^test result:' "$SCRATCH/build/rust-qld-lto.log"
check_linked_by_qld "$target/release/deps" || true
t2=$(now)
echo "crate $((t1 - t0))s (exit $crate_status), qld unit tests $((t2 - t1))s (exit $qld_status)"
[ $crate_status -eq 0 ] && [ $qld_status -eq 0 ]
