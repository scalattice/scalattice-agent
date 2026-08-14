#!/usr/bin/env bash
# Drop stale rustc hashes of *this* binary in target/**/deps.
# Does not delete llama.cpp .rlib / build trees, so the next compile stays warm.
set -euo pipefail

cd "$(dirname "$0")/.."

# Cursor sandboxes and CI often set CARGO_TARGET_DIR away from ./target.
# Prune both so leftover 1 GB rustc hashes do not pile up in either tree.
roots=()
if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
  roots+=("$CARGO_TARGET_DIR")
fi
roots+=("target")

declare -A seen=()
uniq_roots=()
for r in "${roots[@]}"; do
  [[ -d "$r" ]] || continue
  key=$(readlink -f "$r" 2>/dev/null || echo "$r")
  if [[ -n "${seen[$key]:-}" ]]; then
    continue
  fi
  seen[$key]=1
  uniq_roots+=("$r")
done

if [[ ${#uniq_roots[@]} -eq 0 ]]; then
  exit 0
fi

files=()
for root in "${uniq_roots[@]}"; do
  while IFS= read -r -d '' f; do
    files+=("$f")
  done < <(find "$root" -type f -path '*/deps/*' -name 'scalattice_agent-*' -print0 2>/dev/null || true)
done

if [[ ${#files[@]} -eq 0 ]]; then
  echo "==> cargo target: no leftover scalattice_agent-* hashes in deps/"
  exit 0
fi

bytes=0
for f in "${files[@]}"; do
  if [[ -f "$f" ]]; then
    sz=$(stat -c%s "$f" 2>/dev/null || stat -f%z "$f" 2>/dev/null || echo 0)
    bytes=$((bytes + sz))
    rm -f "$f"
  fi
done

gib=$(awk -v b="$bytes" 'BEGIN { printf "%.1f", b / 1024 / 1024 / 1024 }')
echo "==> pruned ${#files[@]} leftover agent hash(es) from deps/ (${gib} GB)"
echo "    llama.cpp artifacts kept; next cargo build will not recompile ggml"
