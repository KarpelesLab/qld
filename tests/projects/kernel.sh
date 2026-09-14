#!/bin/sh
# Build the Linux kernel with qld as the linker (an M3 exit criterion),
# compare its layout with GNU ld's, and boot the result in QEMU.
#
# The kernel is the hardest linker script user there is: arch/x86/kernel/
# vmlinux.lds is a fully explicit SECTIONS with ASSERTs, PROVIDEs, sorted
# and KEEPt tables, discards, `--orphan-handling=error` and `--emit-relocs`,
# and the boot stub links with its own script.
#
# Usage: tests/projects/kernel.sh /path/to/qld /path/to/scratch
QLD=$1
SCRATCH=$2
KERNEL_VERSION=${KERNEL_VERSION:-7.2.5}
# Resolve GNU ld before common.sh puts qld wrappers named ld.bfd in PATH's
# reach: the shim below must reach the real GNU ld.
GNU_LD=${GNU_LD:-$(command -v ld.bfd || command -v ld)}
. "$(dirname "$0")/common.sh"

case "$GNU_LD" in
  /*) ;;
  *) echo "kernel.sh: GNU ld not found; set GNU_LD"; exit 2 ;;
esac

major=${KERNEL_VERSION%%.*}
url="https://cdn.kernel.org/pub/linux/kernel/v$major.x/linux-$KERNEL_VERSION.tar.xz"
src="$SCRATCH/build/linux-$KERNEL_VERSION"
build="$SCRATCH/build/linux-$KERNEL_VERSION-qld"
log="$SCRATCH/build/kernel-links.log"
shim="$SCRATCH/bin/ld-kernel"
status=0

# The kernel calls the linker for more than the x86-64 vmlinux link. The
# shim sends to GNU ld what qld cannot do yet, and everything else to qld:
#
#   -m elf_i386     32-bit output (realmode.elf, vdso32, setup.elf): ELF32
#                   is roadmap M4, qld links only elf_x86_64 today;
#   -v/-V/--version the kernel's scripts/ld-version.sh parses GNU ld's
#                   version string to gate features;
#   -r              qld's relocatable output (vmlinux.o) makes objtool fail
#                   with "can't find starting instruction"; that is the
#                   relocatable writer, not script layout. Set KERNEL_QLD_R=1
#                   to send -r links to qld anyway and see it fail.
#
# Everything else is linked by qld: the vmlinux.lds links (.tmp_vmlinux1,
# .tmp_vmlinux2, vmlinux.unstripped), vdso64.so.dbg, and the compressed boot
# stub with its own script. Every call is logged with the linker that ran
# it, and the arguments of the .tmp_vmlinux1 link are kept for step 3.
put_script "$shim" <<EOF
#!/bin/sh
use_gnu=0
out=
next_is_out=0
for arg in "\$@"; do
  if [ "\$next_is_out" = 1 ]; then out=\$arg; next_is_out=0; fi
  case "\$arg" in
    elf_i386|-v|--version|-V) use_gnu=1 ;;
    -r) [ -n "\${KERNEL_QLD_R:-}" ] || use_gnu=1 ;;
    -o) next_is_out=1 ;;
  esac
done
case "\$out" in
  *.tmp_vmlinux1)
    : > "$log.vmlinux1.args"
    for arg in "\$@"; do printf '%s\n' "\$arg" >> "$log.vmlinux1.args"; done
    pwd > "$log.vmlinux1.dir"
    ;;
esac
if [ "\$use_gnu" = 1 ]; then
  printf 'GNU %s\n' "\$*" >> "$log"
  exec "$GNU_LD" "\$@"
fi
printf 'QLD %s\n' "\$*" >> "$log"
"$QLD" "\$@" || { printf 'QLD FAILED %s\n' "\$*" >> "$log"; exit 1; }
EOF

# 1. Source and configuration: plain x86_64 defconfig, out-of-tree build.
t0=$(now)
if [ ! -f "$src/Makefile" ]; then
  tarball=$(fetch "$url")
  rm -rf "$src"
  tar -xf "$tarball" -C "$SCRATCH/build"
fi
rm -f "$log" "$log.vmlinux1.args" "$log.vmlinux1.dir"
mkdir -p "$build"
(cd "$src" && make O="$build" defconfig > "$build/defconfig.log" 2>&1)
t1=$(now)

# 2. Build the bzImage with qld. The link outputs are removed first, so that
#    a rerun with a different qld relinks them instead of reusing what make
#    still considers up to date (make does not track the linker binary).
rm -f "$build/vmlinux" "$build/vmlinux.unstripped" "$build/.tmp_vmlinux"* \
      "$build/arch/x86/boot/bzImage" "$build/arch/x86/boot/voffset.h" \
      "$build/arch/x86/boot/compressed/vmlinux" \
      "$build/arch/x86/entry/vdso/vdso64/vdso64.so.dbg"
if (cd "$src" && make O="$build" -j"$(nproc)" LD="$shim" bzImage \
      > "$build/build.log" 2>&1); then
  echo "PASS: bzImage built with qld"
else
  echo "FAIL: kernel build"
  tail -30 "$build/build.log"
  exit 1
fi
t2=$(now)

qld_links=$(grep -c '^QLD ' "$log" || true)
gnu_links=$(grep -c '^GNU ' "$log" || true)
failed=$(grep -c '^QLD FAILED ' "$log" || true)
echo "links: $qld_links by qld, $gnu_links by GNU ld, $failed qld failures"
grep '^QLD ' "$log" | sed -e 's/.* -o //' -e 's/ .*//' | sort | uniq -c
[ "$failed" -eq 0 ] || status=1
# The outputs qld must have linked. A full build also links vdso32,
# realmode.elf and setup.elf, which are ELF32 and go to GNU ld.
for out in .tmp_vmlinux1 .tmp_vmlinux2 vmlinux.unstripped \
           arch/x86/boot/compressed/vmlinux \
           arch/x86/entry/vdso/vdso64/vdso64.so.dbg; do
  if grep '^QLD ' "$log" | awk -v want="$out" \
       '{ for (i = 2; i < NF; i++) if ($i == "-o" && $(i + 1) == want) found = 1 }
        END { exit !found }'; then
    echo "PASS: $out linked by qld"
  else
    echo "FAIL: $out was not linked by qld"
    status=1
  fi
done

# 3. Layout comparison: relink .tmp_vmlinux1 (the vmlinux.lds link, with
#    --emit-relocs and --orphan-handling=error) from the same inputs with
#    GNU ld and with qld, then compare the allocated sections and every
#    symbol address. -O2 turns on qld's string tail merging, which GNU ld
#    does by default; without it .rodata and everything after it differ.
cmpdir="$SCRATCH/build/kernel-compare"
if [ -f "$log.vmlinux1.args" ]; then
  rm -rf "$cmpdir"
  mkdir -p "$cmpdir"
  if (
    cd "$(cat "$log.vmlinux1.dir")" || exit 1
    # One argument per line; no kernel link argument contains whitespace or
    # a glob character, but disable globbing to be sure.
    set -f
    set -- $(grep -v -e '^-o$' -e '\.tmp_vmlinux1$' "$log.vmlinux1.args")
    "$GNU_LD" "$@" -o "$cmpdir/gnu.elf" > "$cmpdir/gnu.log" 2>&1 || exit 1
    "$QLD" -O2 "$@" -o "$cmpdir/qld.elf" > "$cmpdir/qld.log" 2>&1 || exit 1
  ); then
    for f in gnu qld; do
      # Allocated sections by name, address and size. qld puts the
      # --emit-relocs .rela.* sections after them instead of next to the
      # section they belong to, which readelf's order would show.
      readelf -SW "$cmpdir/$f.elf" | sed -E 's/^ *\[ *[0-9]+\] */ /' |
        awk '$7 ~ /A/ { print $1, $3, $5 }' | sort > "$cmpdir/$f.sections"
      nm -n "$cmpdir/$f.elf" | awk '{ print $1, $3 }' > "$cmpdir/$f.symbols"
      nm -n "$cmpdir/$f.elf" > "$cmpdir/$f.nm"
    done
    if diff -u "$cmpdir/gnu.sections" "$cmpdir/qld.sections" > "$cmpdir/sections.diff"; then
      echo "PASS: $(wc -l < "$cmpdir/gnu.sections") allocated sections have GNU ld's addresses and sizes"
    else
      echo "FAIL: allocated section differences"
      head -40 "$cmpdir/sections.diff"
      status=1
    fi
    if diff -u "$cmpdir/gnu.symbols" "$cmpdir/qld.symbols" > "$cmpdir/symbols.diff"; then
      echo "PASS: all $(wc -l < "$cmpdir/gnu.symbols") symbol addresses match GNU ld's"
    else
      echo "FAIL: symbol address differences"
      head -40 "$cmpdir/symbols.diff"
      status=1
    fi
    # Known, documented symbol table differences (kernel.md): hidden global
    # symbols that qld emits as local. Informational, not a failure.
    diff "$cmpdir/gnu.nm" "$cmpdir/qld.nm" > "$cmpdir/nm.diff" || true
    echo "nm lines differing in binding only: $(grep -c '^<' "$cmpdir/nm.diff" || true)"
  else
    echo "FAIL: relinking .tmp_vmlinux1 for the comparison"
    cat "$cmpdir/gnu.log" "$cmpdir/qld.log" 2>/dev/null || true
    status=1
  fi
else
  echo "FAIL: no .tmp_vmlinux1 link was recorded"
  status=1
fi
t3=$(now)

# 4. Boot the bzImage in QEMU with a one-program initramfs.
bzimage="$build/arch/x86/boot/bzImage"
marker="QLD KERNEL BOOT OK"
boot="$SCRATCH/build/kernel-boot"
if ! command -v qemu-system-x86_64 > /dev/null; then
  echo "SKIP: qemu-system-x86_64 not found, not booting $bzimage"
elif ! command -v cpio > /dev/null; then
  echo "SKIP: cpio not found, not booting $bzimage"
else
  rm -rf "$boot"
  mkdir -p "$boot/root"
  cat > "$boot/init.c" <<EOF
#include <stdio.h>
#include <sys/reboot.h>
#include <sys/utsname.h>
#include <unistd.h>

int main(void)
{
    struct utsname u;
    uname(&u);
    printf("$marker: %s %s\n", u.release, u.version);
    fflush(stdout);
    sync();
    reboot(RB_POWER_OFF);
    return 0;
}
EOF
  if gcc -static -O2 "$boot/init.c" -o "$boot/root/init" > "$boot/init.log" 2>&1; then
    (cd "$boot/root" && echo init | cpio -o -H newc > "$boot/initramfs.cpio" 2>/dev/null)
    timeout 300 qemu-system-x86_64 -m 512M -nographic -no-reboot \
      -kernel "$bzimage" -initrd "$boot/initramfs.cpio" \
      -append "console=ttyS0 panic=-1" > "$boot/console.log" 2>&1 || true
    if grep -a "$marker" "$boot/console.log"; then
      echo "PASS: the kernel linked by qld boots"
    else
      echo "FAIL: no boot marker; console log in $boot/console.log"
      tail -30 "$boot/console.log"
      status=1
    fi
  else
    echo "SKIP: no static libc, not booting (see $boot/init.log)"
  fi
fi
t4=$(now)

echo "kernel $KERNEL_VERSION: configure $((t1 - t0))s, build $((t2 - t1))s, compare $((t3 - t2))s, boot $((t4 - t3))s, status $status"
exit $status
