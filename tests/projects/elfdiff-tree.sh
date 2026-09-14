#!/bin/sh
# Run elfdiff.py on every executable and shared object present in both of
# two build trees (a GNU ld build and a qld build of the same project).
# Usage: tests/projects/elfdiff-tree.sh GNU_TREE QLD_TREE [elfdiff options]
# Prints a summary line per differing file followed by its differences.
set -u
ref=$1
cand=$2
shift 2
here=$(dirname "$0")
same=0
diff=0
for f in $(cd "$cand" && find . -type f \( -perm -u+x -o -name '*.so' -o -name '*.so.*' \) | sort); do
  [ -f "$ref/$f" ] || continue
  head -c 4 "$cand/$f" 2>/dev/null | grep -q ELF || continue
  readelf -h "$cand/$f" 2>/dev/null | grep -Eq 'Type: +(EXEC|DYN)' || continue
  if out=$(python3 "$here/elfdiff.py" "$ref/$f" "$cand/$f" "$@"); then
    same=$((same + 1))
  else
    diff=$((diff + 1))
    echo "== $f"
    printf '%s\n' "$out"
  fi
done
echo "elfdiff: $same identical, $diff differing"
