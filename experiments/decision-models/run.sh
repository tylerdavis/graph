#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"
graph="${GRAPH_BIN:-../../target/debug/graph}"
plans=(docs_drift_infer docs_drift_decide format_drift_infer format_drift_decide)
limit=0
if [[ "${1:-}" == "--one" ]]; then
  limit=1
fi

now_ms() { perl -MTime::HiRes=time -e 'printf "%d", time * 1000'; }

mkdir -p results
count=0
while read -r pr base head; do
  [[ -z "$pr" ]] && continue
  for plan in "${plans[@]}"; do
    out="results/${pr}-${plan}.json"
    started=$(now_ms)
    set +e
    GRAPH_STORAGE=memory "$graph" plan run "$plan" --input "base=$base" --input "head=$head" --json >"$out.tmp" 2>"results/${pr}-${plan}.log"
    code=$?
    set -e
    ms=$(( $(now_ms) - started ))
    error=$(rg -o 'Error: .*' "results/${pr}-${plan}.log" | tail -1 || true)
    if [[ -s "$out.tmp" ]]; then
      envelope=$(cat "$out.tmp")
    else
      envelope=null
    fi
    jq -nc --arg pr "$pr" --arg plan "$plan" --argjson code "$code" --argjson ms "$ms" \
      --arg error "$error" --argjson envelope "$envelope" \
      '{pr: $pr, plan: $plan, exit_code: $code, wall_ms: $ms, error: (if $error == "" then null else $error end), envelope: $envelope}' >"$out"
    rm -f "$out.tmp"
    jq -c '{pr, plan, exit_code, wall_ms, error}' "$out"
  done
  count=$((count + 1))
  if (( limit > 0 && count >= limit )); then
    break
  fi
done <prs.txt
