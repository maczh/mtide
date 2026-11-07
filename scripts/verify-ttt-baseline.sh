#!/usr/bin/env bash
set -euo pipefail

expected="afa48edc0b35539423062bafef747dca610fbd5b"
repo="${TTT_ORACLE_REPO:-${1:-}}"
binary="${TTT_ORACLE_BIN:-ttt}"

if [[ -z "$repo" || ! -d "$repo/.git" ]]; then
  echo "ttt oracle checkout is required; set TTT_ORACLE_REPO" >&2
  exit 2
fi
actual=$(git -C "$repo" rev-parse HEAD)
if [[ "$actual" != "$expected" ]]; then
  echo "ttt oracle commit mismatch: expected $expected, got $actual" >&2
  exit 1
fi
if [[ -n "$(git -C "$repo" status --porcelain)" ]]; then
  echo "ttt oracle checkout is dirty: $repo" >&2
  exit 1
fi
if ! command -v "$binary" >/dev/null 2>&1; then
  echo "ttt oracle executable not found: $binary" >&2
  exit 2
fi
echo "ttt oracle baseline verified: $expected"
