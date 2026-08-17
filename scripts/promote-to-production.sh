#!/usr/bin/env bash
# Squash the current tree onto public scalattice-agent production.
# Used locally (gh auth) or by the private-repo promote workflow (SSH deploy key).
set -euo pipefail

cd "$(dirname "$0")/.."

PUBLIC_REPO="${PUBLIC_REPO:-scalattice/scalattice-agent}"
PUBLIC_BRANCH="${PUBLIC_BRANCH:-production}"
SOURCE_SHA="$(git rev-parse --short HEAD)"
MSG="${PROMOTE_MESSAGE:-Promote development ${SOURCE_SHA}}"

WORKDIR="$(mktemp -d /tmp/scalattice-promote.XXXXXX)"
cleanup() { rm -rf "$WORKDIR"; }
trap cleanup EXIT

if [[ -n "${PROMOTE_SSH_KEY:-}" ]]; then
  KEY_FILE="$(mktemp "$WORKDIR/id.XXXXXX")"
  printf '%s\n' "$PROMOTE_SSH_KEY" >"$KEY_FILE"
  chmod 600 "$KEY_FILE"
  export GIT_SSH_COMMAND="ssh -i ${KEY_FILE} -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new"
  git clone --branch "$PUBLIC_BRANCH" "git@github.com:${PUBLIC_REPO}.git" "$WORKDIR/public"
elif command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
  gh repo clone "$PUBLIC_REPO" "$WORKDIR/public" -- --branch "$PUBLIC_BRANCH"
else
  echo "Set PROMOTE_SSH_KEY or run gh auth login." >&2
  exit 1
fi

rsync -a --delete \
  --exclude .git \
  --exclude target \
  --exclude target-gpu-verify \
  --exclude dist \
  ./ "$WORKDIR/public/"

git -C "$WORKDIR/public" add -A
if git -C "$WORKDIR/public" diff --cached --quiet; then
  echo "==> Public ${PUBLIC_BRANCH} already matches this tree"
  exit 0
fi

git -C "$WORKDIR/public" \
  -c user.name="scalattice-agent-dev" \
  -c user.email="41898282+github-actions[bot]@users.noreply.github.com" \
  commit -m "$MSG"

git -C "$WORKDIR/public" push origin "HEAD:${PUBLIC_BRANCH}"
echo "==> Pushed squash to ${PUBLIC_REPO}@${PUBLIC_BRANCH}"
