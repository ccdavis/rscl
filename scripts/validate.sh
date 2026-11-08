#!/usr/bin/env bash
# Run every tests/corpus/*.clj (or the given files) under every mode and compare with
# tests/expected/NAME.out. UPDATE_EXPECTED=1 (re)writes the expected files from the
# JIT output when all modes agree.
set -u
cd "$(dirname "$0")/.."
cargo build -q || exit 1
BIN=target/debug/rscl
MODES=(jit aot)
[ -n "${RSCL_JIT_ONLY:-}" ] && MODES=(jit)
files=("$@")
[ ${#files[@]} -eq 0 ] && files=(tests/corpus/*.clj)
fail=0; pass=0
run_mode() { # mode file -> prints stdout, returns status
  local mode=$1 file=$2
  case $mode in
    jit) timeout 20 "$BIN" run "$file" 2>/tmp/rscl_validate_err ;;
    aot) local exe; exe=$(mktemp); timeout 60 "$BIN" build "$file" -o "$exe" 2>/tmp/rscl_validate_err && timeout 20 "$exe" 2>>/tmp/rscl_validate_err; local s=$?; rm -f "$exe"; return $s ;;
  esac
}
for f in "${files[@]}"; do
  name=$(basename "$f" .clj); exp=tests/expected/$name.out
  declare -A outs stats
  for m in "${MODES[@]}"; do outs[$m]=$(run_mode "$m" "$f"); stats[$m]=$?; done
  ok=1; ref=${MODES[0]}
  for m in "${MODES[@]}"; do
    if [[ "${stats[$m]}" != "${stats[$ref]}" ]]; then echo "FAIL $name: $m exit ${stats[$m]} vs $ref exit ${stats[$ref]}"; ok=0; fi
    if [[ $name != *error* && "${outs[$m]}" != "${outs[$ref]}" ]]; then echo "FAIL $name: $m output disagrees with $ref"; diff <(printf '%s\n' "${outs[$ref]}") <(printf '%s\n' "${outs[$m]}") | head -20; ok=0; fi
  done
  if [[ $name == *error* ]]; then
    [[ ${stats[$ref]} -eq 0 ]] && { echo "FAIL $name: expected a failure"; ok=0; }
    actual=$(printf '%s\n%s' "${stats[$ref]}" "${outs[$ref]}")
  else
    [[ ${stats[$ref]} -ne 0 ]] && { echo "FAIL $name: exit ${stats[$ref]}"; sed 's/^/    /' /tmp/rscl_validate_err; ok=0; }
    actual=${outs[$ref]}
  fi
  if [[ $ok -eq 1 && -n "${UPDATE_EXPECTED:-}" ]]; then printf '%s\n' "$actual" > "$exp"; echo "wrote $exp"; fi
  if [[ -f $exp ]]; then
    if [[ "$(printf '%s' "$actual" | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}')" != "$(sed -e :a -e '/^\n*$/{$d;N;ba' -e '}' "$exp")" ]]; then echo "FAIL $name: output differs from $exp"; diff <(printf '%s\n' "$actual") "$exp" | head -20; ok=0; fi
  else
    echo "FAIL $name: missing $exp"; ok=0
  fi
  [[ $ok -eq 1 ]] && { pass=$((pass+1)); echo "ok   $name"; } || fail=$((fail+1))
  unset outs stats
done
echo "passed $pass, failed $fail"
[[ $fail -eq 0 ]]
