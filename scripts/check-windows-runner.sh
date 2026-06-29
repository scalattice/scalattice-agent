#!/usr/bin/env bash
# Verify an online self-hosted Windows runner exists before triggering release CI.
set -euo pipefail

RUNNER_LABEL="${SCALATTICE_WINDOWS_RUNNER_LABEL:-scalattice-release}"

require_windows_runner() {
  local repo count
  repo="$(gh repo view --json nameWithOwner -q .nameWithOwner)"
  count="$(gh api "repos/${repo}/actions/runners" \
    --jq "[.runners[] | select(.status==\"online\") | select([.labels[].name] | index(\"${RUNNER_LABEL}\"))] | length")"

  if [[ "${count:-0}" -ge 1 ]]; then
    echo "==> Windows self-hosted runner online (label: ${RUNNER_LABEL})"
    return 0
  fi

  cat >&2 <<EOF
No online self-hosted Windows runner with label "${RUNNER_LABEL}".

One-time setup on your Windows build machine (Admin PowerShell):

  git clone https://github.com/${repo}.git
  cd Scalattice-Client
  gh auth login
  .\\scripts\\setup-windows-build.ps1
  .\\scripts\\install-windows-runner.ps1

Then re-run ./scripts/release.sh

Emergency fallback (slow GitHub-hosted runner, ~1h cold):
  ./scripts/release.sh --github-hosted-windows ...
EOF
  return 1
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
  require_cmd() { command -v "$1" >/dev/null 2>&1 || { echo "Missing: $1" >&2; exit 1; }; }
  require_cmd gh
  require_windows_runner
fi
