#!/usr/bin/env bash
# Time every bench/*.clj (or the given names) under: Python 3 (bench/NAME.py), Clojure
# on the JVM (jars expected in ~/.cache/rscl-bench: clojure, spec.alpha and
# core.specs.alpha from Maven Central), rscl run (JIT), and rscl build (AOT, plain and
# --opt). Prints the median wall time of RUNS runs and checks that every
# implementation prints the same output.
# usage: scripts/bench.sh [--runs N] [--no-jvm] [--no-python] [NAME...]
set -u
cd "$(dirname "$0")/.."
RUNS=3; JVM=1; PY=1; names=()
while [ $# -gt 0 ]; do
  case $1 in
    --runs) RUNS=$2; shift 2 ;;
    --no-jvm) JVM=0; shift ;;
    --no-python) PY=0; shift ;;
    *) names+=("$1"); shift ;;
  esac
done
[ ${#names[@]} -eq 0 ] && for f in bench/*.clj; do names+=("$(basename "$f" .clj)"); done
cargo build --release -q || exit 1
BIN=target/release/rscl
# The interpreter uv manages (this machine's Python policy), run directly so the
# launcher's start-up does not count against Python.
PYTHON=$(uv python find 2>/dev/null || command -v python3)
CACHE=$HOME/.cache/rscl-bench
CP=""
if [ $JVM -eq 1 ]; then
  for f in clojure-1.12.0.jar spec.alpha-0.5.238.jar core.specs.alpha-0.4.74.jar; do
    if [ ! -f "$CACHE/$f" ]; then
      echo "missing $CACHE/$f (download it from Maven Central, or pass --no-jvm)"; exit 1
    fi
    CP="$CP${CP:+:}$CACHE/$f"
  done
fi
median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}'; }
timed() { # cmd... -> median ms in $MS, program output in $OUT
  local times=() start end
  for _ in $(seq 1 "$RUNS"); do
    start=$(date +%s%N); OUT=$("$@" 2>&1); end=$(date +%s%N)
    times+=($(( (end - start) / 1000000 )))
  done
  MS=$(median "${times[@]}")
}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
printf '%-12s %10s %10s %10s %10s %10s\n' benchmark python clojure rscl-jit rscl-aot rscl-opt
for name in "${names[@]}"; do
  clj=bench/$name.clj; py=bench/$name.py
  exe=$TMP/$name; exe_opt=$TMP/${name}_opt
  "$BIN" build "$clj" -o "$exe" >/dev/null 2>&1 || { echo "$name: build failed"; continue; }
  "$BIN" build "$clj" --opt -o "$exe_opt" >/dev/null 2>&1 || { echo "$name: build --opt failed"; continue; }
  ref=""
  t_py="-"; if [ $PY -eq 1 ] && [ -f "$py" ]; then timed "$PYTHON" "$py"; t_py=$MS; ref=$OUT; fi
  t_jvm="-"
  if [ $JVM -eq 1 ]; then
    timed java -cp "$CP" clojure.main "$clj"; t_jvm=$MS
    [ -n "$ref" ] && [ "$OUT" != "$ref" ] && echo "  $name: clojure output differs: $OUT"
    ref=${ref:-$OUT}
  fi
  timed "$BIN" run "$clj"; t_jit=$MS
  [ -n "$ref" ] && [ "$OUT" != "$ref" ] && echo "  $name: rscl run output differs: $OUT"
  ref=${ref:-$OUT}
  timed "$exe"; t_aot=$MS; [ "$OUT" != "$ref" ] && echo "  $name: rscl aot output differs: $OUT"
  timed "$exe_opt"; t_opt=$MS; [ "$OUT" != "$ref" ] && echo "  $name: rscl aot --opt output differs: $OUT"
  printf '%-12s %10s %10s %10s %10s %10s\n' "$name" "$t_py" "$t_jvm" "$t_jit" "$t_aot" "$t_opt"
done
echo "(median of $RUNS runs, milliseconds; the JVM time includes its start-up)"
