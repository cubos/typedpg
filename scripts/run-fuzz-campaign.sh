#!/usr/bin/env bash
#
# Long differential-fuzzing campaign: run the fuzzer once per seed, each
# run in its own PG container (via run-pg-sanity.sh), collecting findings
# under target/fuzz-campaign/<timestamp>/seed-<seed>/ and printing a
# cross-seed summary at the end.
#
# The campaign never fails on findings (FUZZ_STRICT stays unset) — triage
# happens on the collected .sql files, where high-*.sql are the
# high-signal single-fault findings and multi-*.sql may be error-ordering
# noise (see CLAUDE.md).
#
# Usage:
#   scripts/run-fuzz-campaign.sh                     # default seeds, 20k iters each
#   scripts/run-fuzz-campaign.sh 7 8 9               # explicit seeds
#   FUZZ_CAMPAIGN_ITERS=50000 scripts/run-fuzz-campaign.sh

set -euo pipefail

ITERS="${FUZZ_CAMPAIGN_ITERS:-20000}"
SEEDS=("$@")
if [[ ${#SEEDS[@]} -eq 0 ]]; then
    SEEDS=(101 102 103 104 105 106 107 108)
fi

script_dir="$(cd "$(dirname "$0")" && pwd)"
# Absolute path: FUZZ_OUT reaches the test process, whose cwd is the crate
# directory (nextest), not the workspace root.
root="$(pwd)/target/fuzz-campaign/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$root"

for seed in "${SEEDS[@]}"; do
    out="$root/seed-$seed"
    # Pre-create the findings dir so the summary's `find` works (under
    # pipefail) even for a seed with zero findings.
    mkdir -p "$out"
    echo
    echo "━━ fuzz campaign: seed=$seed iters=$ITERS → $out ━━"
    FUZZ_ITERS="$ITERS" FUZZ_SEED="$seed" FUZZ_OUT="$out" \
        "$script_dir/run-pg-sanity.sh" \
        --run-ignored all --no-capture -E 'test(fuzz_analyze_against_pg)'
done

echo
echo "━━━━ campaign summary ($root) ━━━━"
total_single=0
total_multi=0
for seed in "${SEEDS[@]}"; do
    out="$root/seed-$seed"
    single=$(find "$out" -name 'high-*.sql' 2>/dev/null | wc -l)
    multi=$(find "$out" -name 'multi-*.sql' 2>/dev/null | wc -l)
    total_single=$((total_single + single))
    total_multi=$((total_multi + multi))
    printf '  seed %-12s single-fault: %-4s multi-fault: %s\n' "$seed" "$single" "$multi"
done
echo "  total: $total_single single-fault (high signal), $total_multi multi-fault (may be ordering)"
if [[ $((total_single + total_multi)) -gt 0 ]]; then
    echo "  findings under $root/seed-*/"
fi
