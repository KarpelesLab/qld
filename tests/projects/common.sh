# Shared setup for the real-project build scripts in tests/projects/.
#
# Source it from a project script after setting:
#   QLD      absolute path of the qld binary to test
#   SCRATCH  absolute path of a scratch directory (sources, builds, logs)
#
# It creates $SCRATCH/bin with `ld`, `ld.qld`, `ld.bfd`, `ld.gold`, `ld.lld`
# and `ld.mold` wrappers that all run qld, so that gcc's `-B` and clang's
# `-fuse-ld=` lookups cannot fall back to another linker, plus `qcc`,
# `qc++`, `qclang` and `qclang++` compiler wrappers that point the drivers
# at them. When QLD_LINK_LOG is set, every link is appended to it (working
# directory and argv), which is what reproducing a failing link needs.
#
# These scripts are for manual and nightly runs; `cargo test` never runs them.

set -eu

: "${QLD:?set QLD to the qld binary}"
: "${SCRATCH:?set SCRATCH to a scratch directory}"

case "$QLD" in /*) ;; *) QLD="$(pwd)/$QLD" ;; esac
case "$SCRATCH" in /*) ;; *) SCRATCH="$(pwd)/$SCRATCH" ;; esac
mkdir -p "$SCRATCH/bin" "$SCRATCH/src" "$SCRATCH/build"
QLD_BIN="$SCRATCH/bin"

# Wrappers are written to a temporary file and renamed into place, so that
# concurrent builds sharing $SCRATCH never see a missing or partial `ld`
# (gcc would silently fall back to the system linker), and so that an
# existing symlink is replaced rather than written through.
put_script() {
  cat > "$1.tmp.$$"
  chmod +x "$1.tmp.$$"
  mv -f "$1.tmp.$$" "$1"
}

# libtool decides that a GNU linker cannot build shared libraries unless
# `ld --help` prints a "supported targets: ... elf" line, which GNU ld, gold,
# lld and mold all print. Until qld's own --help does, the wrapper appends
# one (set QLD_NO_HELP_WORKAROUND=1 to see what libtool does without it).
put_script "$QLD_BIN/ld" <<EOF
#!/bin/sh
if [ -n "\${QLD_LINK_LOG:-}" ]; then
  { printf 'cd %s;' "\$(pwd)"; for a in "\$@"; do printf " '%s'" "\$a"; done; printf '\n'; } >> "\$QLD_LINK_LOG"
fi
case " \$* " in *" --help "*) help=1 ;; *) help= ;; esac
if [ -n "\$help" ] && [ -z "\${QLD_NO_HELP_WORKAROUND:-}" ]; then
  "$QLD" "\$@" || exit
  if ! "$QLD" "\$@" | grep -q 'supported targets:'; then
    echo "qld: supported targets: elf64-x86-64 elf32-i386 elf32-x86-64 elf64-little elf64-big elf32-little elf32-big"
    echo "qld: supported emulations: elf_x86_64 elf_i386 elf32_x86_64"
  fi
  exit 0
fi
exec "$QLD" "\$@"
EOF
for n in ld.qld ld.bfd ld.gold ld.lld ld.mold; do
  [ -L "$QLD_BIN/$n" ] || ln -s ld "$QLD_BIN/$n"
done

# gcc finds `ld` through -B; clang through --ld-path. Wrapper scripts make
# the choice survive build systems that drop CFLAGS/LDFLAGS for some links.
put_script "$QLD_BIN/qcc" <<EOF
#!/bin/sh
exec gcc -B"$QLD_BIN" "\$@"
EOF
put_script "$QLD_BIN/qc++" <<EOF
#!/bin/sh
exec g++ -B"$QLD_BIN" "\$@"
EOF
put_script "$QLD_BIN/qclang" <<EOF
#!/bin/sh
exec clang --ld-path="$QLD_BIN/ld" "\$@"
EOF
put_script "$QLD_BIN/qclang++" <<EOF
#!/bin/sh
exec clang++ --ld-path="$QLD_BIN/ld" "\$@"
EOF

# Download $1 into $SCRATCH/src unless it is already there; print the path.
fetch() {
  f="$SCRATCH/src/$(basename "$1")"
  if [ ! -f "$f" ]; then
    curl -fL --retry 3 -o "$f.part" "$1"
    mv "$f.part" "$f"
  fi
  printf '%s\n' "$f"
}

# Lists the executables and shared objects under the given directories that
# were not linked by qld (no "Linker: qld" in .comment); returns non-zero
# when there are any.
check_linked_by_qld() {
  bad=0
  total=0
  for f in $(find "$@" -type f \( -perm -u+x -o -name '*.so' -o -name '*.so.*' \) 2>/dev/null); do
    head -c 4 "$f" 2>/dev/null | grep -q 'ELF' || continue
    readelf -h "$f" 2>/dev/null | grep -Eq 'Type: +(EXEC|DYN)' || continue
    total=$((total + 1))
    if ! readelf -p .comment "$f" 2>/dev/null | grep -q 'Linker: qld'; then
      echo "not linked by qld: $f"
      bad=$((bad + 1))
    fi
  done
  echo "linked-by-qld check: $total ELF outputs, $bad not linked by qld"
  [ "$bad" -eq 0 ]
}

now() { date +%s; }
