#!/usr/bin/env bash
# Helper for the lumen-py end-to-end corpus in crates/lumen-py/tests/py.
#   scripts/py-corpus.sh regen   regenerate .out/.err from python3 (overwrites files)
#   scripts/py-corpus.sh run     build lumen-py and print per-directory pass/total
# Entry scripts are *.py files whose path below tests/py has no component starting with '_'.
# Expected files: X.out = stdout; X.err = exit code (line 1) + last stderr line (line 2),
# present only when the exit code is nonzero (no X.err means "exit 0").
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CORPUS="$ROOT/crates/lumen-py/tests/py"
PYTHON="${PYTHON:-/opt/homebrew/bin/python3}"
SCRATCH="${TMPDIR:-/tmp}/py-corpus-scratch"
mkdir -p "$SCRATCH"
cd "$ROOT" || exit 1

with_timeout() { perl -e 'alarm shift; exec @ARGV or die' "$@"; }

entries() {
  find "$CORPUS" -name '*.py' -not -path '*/_*' | sort
}

case "${1:-run}" in
  regen)
    n=0
    for f in $(entries); do
      dir="$(dirname "$f")"; base="$(basename "$f" .py)"
      ( cd "$dir" && with_timeout 20 "$PYTHON" "$base.py" >"$base.out" 2>"$SCRATCH/err" )
      code=$?
      if [ "$code" -ne 0 ]; then
        printf '%s\n%s\n' "$code" "$(tail -n 1 "$SCRATCH/err")" >"$dir/$base.err"
      elif [ -e "$dir/$base.err" ]; then
        echo "now exits 0, delete ${dir#"$ROOT"/}/$base.err"
      fi
      n=$((n + 1))
    done
    echo "regenerated $n entries"
    ;;
  run)
    cargo build -p lumen-py >&2 || exit 1
    BIN="$ROOT/target/debug/lumen-py"
    : >"$SCRATCH/results"
    for f in $(entries); do
      dir="$(dirname "$f")"; base="$(basename "$f" .py)"
      cat_="${dir#"$CORPUS"/}"
      ( cd "$dir" && with_timeout 10 "$BIN" "$base.py" >"$SCRATCH/out" 2>"$SCRATCH/err" )
      code=$?
      ok=1
      cmp -s "$SCRATCH/out" "$dir/$base.out" || ok=0
      if [ -s "$dir/$base.err" ]; then
        [ "$code" = "$(sed -n 1p "$dir/$base.err")" ] || ok=0
        [ "$(tail -n 1 "$SCRATCH/err")" = "$(sed -n 2p "$dir/$base.err")" ] || ok=0
      else
        [ "$code" -eq 0 ] || ok=0
      fi
      echo "$cat_ $ok" >>"$SCRATCH/results"
      [ "$ok" -eq 1 ] || echo "FAIL $cat_/$base.py" >&2
    done
    awk '{t[$1]++; p[$1]+=$2; T++; P+=$2}
         END {for (k in t) printf "%-20s %d/%d\n", k, p[k], t[k] | "sort"; close("sort");
              printf "overall: %d/%d (%.1f%%)\n", P, T, (T ? 100*P/T : 0)}' "$SCRATCH/results"
    ;;
  *)
    echo "usage: $0 regen|run" >&2; exit 2 ;;
esac
