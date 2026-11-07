#!/usr/bin/env bash
set -euo pipefail

report="${MTIDE_GATE_REPORT:-target/mtide-release-gate.json}"
mkdir -p "$(dirname "$report")"

cargo fmt --check
cargo check --workspace --all-targets
cargo test --workspace --all-targets
cargo run --quiet --bin mtide-p0-gate -- \
  --scenarios "${MTIDE_SCENARIOS_DIR:-compat/scenarios}" \
  --output "$report"

echo "release gate passed: $report"
