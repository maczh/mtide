#!/usr/bin/env bash
set -u

plugin_root=${1:-${TTT_PLUGINS_DIR:-}}
binary=${MTIDE_BIN:-"$(cd "$(dirname "$0")/.." && pwd)/target/debug/mtide"}

if [[ -z "$plugin_root" || ! -d "$plugin_root" ]]; then
  echo "usage: $0 PATH_TO_TTT_PLUGINS" >&2
  echo "or set TTT_PLUGINS_DIR" >&2
  exit 2
fi
if [[ ! -x "$binary" ]]; then
  echo "mtide binary is not executable: $binary" >&2
  echo "build it with: cargo build" >&2
  exit 2
fi

tmp_home=$(mktemp -d "${TMPDIR:-/tmp}/mtide-plugin-smoke.XXXXXX") || exit 2
trap 'rm -rf -- "$tmp_home"' EXIT

count=0
failures=0
plugin_dirs=()
for manifest in "$plugin_root"/*/plugin.ttt.json; do
  [[ -f "$manifest" ]] || continue
  plugin_dir=${manifest%/*}
  plugin_dirs+=("$plugin_dir")
  plugin_name=${plugin_dir##*/}
  count=$((count + 1))
  output=$(HOME="$tmp_home" \
    XDG_CONFIG_HOME="$tmp_home/config" \
    XDG_DATA_HOME="$tmp_home/data" \
    "$binary" --plugin "$plugin_dir" --welcome \
      --exec '[{"action":"key","key":"ctrl+q"}]' 2>&1)
  result=$?
  if [[ $result -ne 0 ]]; then
    printf 'FAIL %s (exit %s)\n%s\n' "$plugin_name" "$result" "$output" >&2
    failures=$((failures + 1))
  else
    printf 'PASS %s\n' "$plugin_name"
  fi
done

if [[ $count -eq 0 ]]; then
  echo "no plugin.ttt.json manifests found under: $plugin_root" >&2
  exit 2
fi
plugin_args=()
for plugin_dir in "${plugin_dirs[@]}"; do
  plugin_args+=(--plugin "$plugin_dir")
done
output=$(HOME="$tmp_home" \
  XDG_CONFIG_HOME="$tmp_home/config" \
  XDG_DATA_HOME="$tmp_home/data" \
  "$binary" "${plugin_args[@]}" --welcome \
    --exec '[{"action":"key","key":"ctrl+q"}]' 2>&1)
result=$?
if [[ $result -ne 0 ]]; then
  printf 'FAIL combined plugin load (exit %s)\n%s\n' "$result" "$output" >&2
  failures=$((failures + 1))
else
  printf 'PASS all %s plugin packages loaded together\n' "$count"
fi
printf 'Loaded %s plugin packages individually and together; %s startup failures\n' "$count" "$failures"
[[ $failures -eq 0 ]]
