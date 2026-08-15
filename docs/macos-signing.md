# Apple Silicon signing (Developer ID)

Metal llama.cpp **cannot be compiled on Linux**. GitHub-hosted `macos-14` / `macos-15` runners are Apple Silicon — use those instead of a slow Mac Mini. Intel Macs are not a release target (`aarch64-apple-darwin` only).

## What you need from Apple Developer

1. **Developer ID Application** certificate (not Apple Development, not Mac App Store).
2. Export it from Keychain Access as a `.p12` (include private key).
3. App Store Connect **Issuer ID**, **Key ID**, and `.p8` API key with Developer access (for notarization). Team ID is on [developer.apple.com/account](https://developer.apple.com/account).

Do not commit the `.p12` or `.p8`.

## GitHub Actions secrets

| Secret | Value |
|--------|--------|
| `APPLE_DEVELOPER_ID_P12_BASE64` | `base64 -i Certificates.p12 \| tr -d '\n'` |
| `APPLE_P12_PASSWORD` | Password you set on the `.p12` |
| `APPLE_SIGNING_IDENTITY` | Exact name, e.g. `Developer ID Application: Robottik Ltd (TEAMID)` |
| `APPLE_TEAM_ID` | 10-character Team ID |
| `APPLE_API_KEY_P8` | Full contents of `AuthKey_XXXXXXXXXX.p8` |
| `APPLE_API_KEY_ID` | The `XXXXXXXXXX` key id |
| `APPLE_API_ISSUER_ID` | Issuer UUID from App Store Connect → Users and Access → Integrations → App Store Connect API |

If these secrets are missing, CI still builds an **unsigned** arm64 binary (Gatekeeper will block downloads from a browser until you sign).

## Local sign + notarize (on any Mac, including a brief Mini session)

```bash
export APPLE_SIGNING_IDENTITY='Developer ID Application: Your Name (TEAMID)'
export APPLE_TEAM_ID=TEAMID
export APPLE_API_KEY_ID=XXXXXXXXXX
export APPLE_API_ISSUER_ID=xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
export APPLE_API_KEY_P8_FILE=/path/to/AuthKey_XXXXXXXXXX.p8

./scripts/sign-macos.sh dist/scalattice-agent
```

The script codesigns with the hardened runtime, zips the binary, submits it with `notarytool`, and waits. A **bare Mach-O cannot be stapled**; Gatekeeper fetches the notarization ticket from Apple when the file is first run (needs network once).

## Intel

- CI builds `--target aarch64-apple-darwin` only.
- `uname -m` must be `arm64` on the runner or the job fails.
- The crate `compile_error!`s on `x86_64-apple-darwin`.
- Embedded `Info.plist` sets `LSRequiresNativeExecution` and `LSArchitecturePriority = arm64`.
