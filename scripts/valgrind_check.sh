#!/usr/bin/env bash
# Leak-check a program under both modes with valgrind. Rebuilds release first.
# usage: scripts/valgrind_check.sh FILE.clj [more files...]
set -u
cd "$(dirname "$0")/.."
cargo build --release -q || exit 1
BIN=target/release/rscl
fail=0
for f in "$@"; do
  for mode in jit aot; do
    case $mode in
      jit) cmd=("$BIN" run "$f") ;;
      aot) exe=$(mktemp); "$BIN" build "$f" -o "$exe" || { echo "FAIL build $f"; fail=1; continue; }; cmd=("$exe") ;;
    esac
    log=$(mktemp)
    valgrind --leak-check=full --errors-for-leak-kinds=definite --error-exitcode=0 --suppressions=scripts/rscl.supp "${cmd[@]}" >/dev/null 2>"$log"
    errs=$(grep -o "ERROR SUMMARY: [0-9]*" "$log" | grep -o "[0-9]*$")
    lost=$(grep -o "definitely lost: [0-9,]*" "$log" | grep -o "[0-9,]*$" | tr -d ,)
    if [[ "${errs:-1}" == "0" && "${lost:-1}" == "0" ]]; then echo "ok   $f [$mode]"; else echo "FAIL $f [$mode]: errors=$errs definitely lost=$lost"; grep -A12 "definitely lost" "$log" | head -40; fail=1; fi
    rm -f "$log"; [[ $mode == aot ]] && rm -f "$exe"
  done
done
exit $fail
