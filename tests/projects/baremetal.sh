#!/bin/sh
# Bare-metal comparison (an M3 exit criterion): link a microcontroller-style
# firmware image with GNU ld and with qld from the same objects and linker
# script, and require the same addresses and the same bytes.
#
# The image is the one in tests/fixtures/script-bare-metal-flash: MEMORY
# regions, `>FLASH`, `>RAM AT> FLASH` load addresses, KEEP, sorted init
# arrays, a fill pattern, data commands, a NOLOAD .bss, LOADADDR, ASSERT and
# /DISCARD/. It compares:
#
#   - section headers and program headers (addresses, sizes, flags, LMAs),
#   - every symbol address (nm),
#   - the flat binary, Intel HEX and S-record images, byte for byte.
#
# The fixtures pin the same expectations inside `cargo test`:
# script-bare-metal-flash checks the ELF addresses, raw-binary and
# raw-ihex-srec check the raw bytes, and tests/script_link.rs compares
# against GNU ld directly when it is installed. This script is the
# standalone, GNU-ld-versus-qld form of all of that, for the milestone.
#
# Usage: tests/projects/baremetal.sh /path/to/qld /path/to/scratch
QLD=$1
SCRATCH=$2
GNU_LD=${GNU_LD:-$(command -v ld.bfd || command -v ld)}
. "$(dirname "$0")/common.sh"

case "$GNU_LD" in
  /*) ;;
  *) echo "baremetal.sh: GNU ld not found; set GNU_LD"; exit 2 ;;
esac

fixtures="$QLD_PROJECTS/../fixtures"
dir="$SCRATCH/build/baremetal"
status=0
rm -rf "$dir"
mkdir -p "$dir"
for f in script-bare-metal-flash/start.s script-bare-metal-flash/main.s \
         script-bare-metal-flash/flash.ld script-static-phdrs/layout.ld; do
  cp "$fixtures/$f" "$dir/" ||
    { echo "baremetal.sh: fixture sources not found under $fixtures"; exit 2; }
done
cp "$fixtures/script-static-phdrs/start.s" "$dir/phdrs.s"
cd "$dir"

as --64 -o start.o start.s
as --64 -o main.o main.s
as --64 -o phdrs.o phdrs.s

# Normalized ELF properties: section headers and program headers without the
# section numbering, and every symbol. The symbol table's own size differs
# (qld does not emit STT_FILE symbols yet), so .symtab, .strtab and
# .shstrtab are left out of the section comparison.
summarize() {
  readelf -SlW "$1" | sed -E 's/^ *\[ *[0-9]+\] */ /' |
    grep -v -e '\.symtab' -e '\.strtab' -e '\.shstrtab' \
            -e 'There are' -e 'Elf file type' -e 'starting at offset' \
            -e 'Key to Flags' -e '^ *[A-Z] (' -e '^$' > "$1.summary"
  nm -n "$1" | awk '{ print $1, $3 }' > "$1.symbols"
}

compare_elf() {
  name=$1
  shift
  "$GNU_LD" "$@" -o "$name.gnu" > "$name.gnu.log" 2>&1 ||
    { echo "FAIL: $name: GNU ld failed"; cat "$name.gnu.log"; status=1; return; }
  "$QLD" "$@" -o "$name.qld" > "$name.qld.log" 2>&1 ||
    { echo "FAIL: $name: qld failed"; cat "$name.qld.log"; status=1; return; }
  summarize "$name.gnu"
  summarize "$name.qld"
  if diff -u "$name.gnu.summary" "$name.qld.summary" > "$name.sections.diff" &&
     diff -u "$name.gnu.symbols" "$name.qld.symbols" > "$name.symbols.diff"; then
    echo "PASS: $name: same sections, segments and $(wc -l < "$name.gnu.symbols") symbol addresses"
  else
    echo "FAIL: $name: layout differs"
    head -40 "$name.sections.diff" "$name.symbols.diff"
    status=1
  fi
}

# Both linkers write to the same output name, because the S-record header
# record (S0) holds it, and then the image is renamed per linker.
compare_raw() {
  name=$1
  shift
  for fmt in binary ihex srec; do
    "$GNU_LD" "$@" --oformat "$fmt" -o "image.$fmt" > "$name.$fmt.log" 2>&1 ||
      { echo "FAIL: $name $fmt: GNU ld failed"; cat "$name.$fmt.log"; status=1; continue; }
    mv "image.$fmt" "$name.gnu.$fmt"
    "$QLD" "$@" --oformat "$fmt" -o "image.$fmt" >> "$name.$fmt.log" 2>&1 ||
      { echo "FAIL: $name $fmt: qld failed"; cat "$name.$fmt.log"; status=1; continue; }
    mv "image.$fmt" "$name.qld.$fmt"
    if cmp -s "$name.gnu.$fmt" "$name.qld.$fmt"; then
      echo "PASS: $name $fmt: identical ($(wc -c < "$name.gnu.$fmt") bytes)"
    else
      echo "FAIL: $name $fmt: bytes differ"
      cmp -l "$name.gnu.$fmt" "$name.qld.$fmt" | head -10
      status=1
    fi
  done
}

# 1. The flash image: MEMORY, AT>, KEEP, SORT_BY_INIT_PRIORITY, fill, data
#    commands, NOLOAD, LOADADDR, ASSERT, /DISCARD/.
flash_args="-T flash.ld -z max-page-size=0x1000 --no-warn-rwx-segments start.o main.o"
# shellcheck disable=SC2086
compare_elf flash $flash_args
# shellcheck disable=SC2086
compare_raw flash $flash_args
# 2. An executable laid out entirely by a script with explicit PHDRS
#    (FILEHDR/PHDRS/FLAGS), SIZEOF_HEADERS, SUBALIGN, SORT and symbol
#    arithmetic; it also has to run.
phdrs_args="-T layout.ld -z max-page-size=0x1000 phdrs.o"
# shellcheck disable=SC2086
compare_elf phdrs $phdrs_args
for f in phdrs.gnu phdrs.qld; do
  if [ -x "./$f" ]; then
    if out=$("./$f" 2>&1); then code=0; else code=$?; fi
    if [ "$out" = "linked by a script" ] && [ "$code" = 37 ]; then
      echo "PASS: $f runs (exit $code)"
    else
      echo "FAIL: $f printed '$out' and exited $code"
      status=1
    fi
  fi
done

echo "bare metal: status $status"
exit $status
