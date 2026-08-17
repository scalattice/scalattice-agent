# Apple Silicon signing (Developer ID)

Metal llama.cpp is built on GitHub-hosted `macos-14` (Apple Silicon). Intel Macs are not supported. CI reads these repository secrets; if they are missing the macOS app/dmg is still uploaded **unsigned**.

CI produces `ScalatticeAgentSetup-aarch64.dmg` (drag the app to Applications) and the `aarch64-apple-darwin` tarball.

Put them on the repo: **Settings → Secrets and variables → Actions → New repository secret**.

| Secret | Where it comes from |
|--------|---------------------|
| `APPLE_TEAM_ID` | [developer.apple.com/account](https://developer.apple.com/account) → Membership → Team ID |
| `APPLE_SIGNING_IDENTITY` | Exact cert name, e.g. `Developer ID Application: Robottik LTD (TEAMID)` |
| `APPLE_DEVELOPER_ID_P12_BASE64` | Base64 of the Developer ID Application `.p12`. Create it with **`-legacy`** on Linux OpenSSL 3, then `base64 -w0 developer-id.p12 > developer-id.p12.b64` and paste that file (not a terminal dump). |
| `APPLE_P12_PASSWORD` | Exact export password for that `.p12` (no trailing newline) |
| `APPLE_API_KEY_P8` | Full text of `AuthKey_XXXXXXXXXX.p8` |
| `APPLE_API_KEY_ID` | Key ID shown when you create the App Store Connect API key |
| `APPLE_API_ISSUER_ID` | Issuer UUID on the App Store Connect API page |

Do not commit the `.p12` or `.p8`.

Linux OpenSSL 3 defaults to a SHA-256 PKCS#12 that macOS `security import` rejects as `MAC verification failed (wrong password?)`. Export like this:

```bash
openssl pkcs12 -export -legacy \
  -inkey developer-id.key \
  -in developer-id.pem \
  -out developer-id.p12
base64 -w0 developer-id.p12 > developer-id.p12.b64
```

Then replace `APPLE_DEVELOPER_ID_P12_BASE64` with the contents of `developer-id.p12.b64`.
