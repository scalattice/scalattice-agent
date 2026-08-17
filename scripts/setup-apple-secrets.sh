#!/usr/bin/env bash
# Upload Apple Developer ID + App Store Connect API secrets to GitHub Actions.
# Never prints secret values. See docs/macos-signing.md.
set -euo pipefail

REPO="${1:-scalattice/scalattice-agent}"

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "Missing $1" >&2
    exit 1
  }
}

need gh
need openssl
need base64

if ! gh auth status >/dev/null 2>&1; then
  echo "Run: gh auth login" >&2
  exit 1
fi

echo "Repo: ${REPO}"
echo "This writes GitHub Actions secrets. Values are not echoed."
echo ""

read -r -p "Path to Developer ID .p12: " P12
[[ -f "$P12" ]] || { echo "Not a file: $P12" >&2; exit 1; }
read -r -s -p "Password for that .p12: " P12_PASS
echo
read -r -p "Signing identity (Developer ID Application: Name (TEAMID)): " IDENTITY
[[ -n "$IDENTITY" ]] || { echo "Signing identity required." >&2; exit 1; }
read -r -p "Apple Team ID (10 characters): " TEAM
read -r -p "Path to AuthKey_XXXXXXXXXX.p8: " P8
[[ -f "$P8" ]] || { echo "Not a file: $P8" >&2; exit 1; }
read -r -p "App Store Connect Key ID (XXXXXXXXXX): " KEY_ID
read -r -p "App Store Connect Issuer ID (UUID): " ISSUER

P12_B64="$(openssl base64 -A -in "$P12")"
P8_BODY="$(cat "$P8")"

gh secret set APPLE_DEVELOPER_ID_P12_BASE64 -R "$REPO" --body "$P12_B64"
gh secret set APPLE_P12_PASSWORD -R "$REPO" --body "$P12_PASS"
gh secret set APPLE_SIGNING_IDENTITY -R "$REPO" --body "$IDENTITY"
gh secret set APPLE_TEAM_ID -R "$REPO" --body "$TEAM"
gh secret set APPLE_API_KEY_P8 -R "$REPO" --body "$P8_BODY"
gh secret set APPLE_API_KEY_ID -R "$REPO" --body "$KEY_ID"
gh secret set APPLE_API_ISSUER_ID -R "$REPO" --body "$ISSUER"

echo ""
echo "Wrote 7 secrets on ${REPO}:"
gh secret list -R "$REPO" | grep '^APPLE_' || true
echo ""
echo "Done. Do not commit the .p12 or .p8."
