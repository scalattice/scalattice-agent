# Apple Silicon signing (Developer ID)

Metal llama.cpp **cannot be compiled on Linux**. GitHub-hosted `macos-14` / `macos-15` runners are Apple Silicon — use those instead of a slow Mac Mini. Intel Macs are not a release target (`aarch64-apple-darwin` only). You do **not** need a Mac to create the certificate or API key.

Do not paste certificates, `.p12` passwords, or `.p8` keys into chat. Use `./scripts/setup-apple-secrets.sh` so values go straight to GitHub.

## 1. Team ID

1. Open [developer.apple.com/account](https://developer.apple.com/account)
2. Membership details → **Team ID** (10 characters). Example shape: `A1B2C3D4E5`

## 2. Developer ID Application certificate (no Mac required)

This is **Developer ID Application**, not “Apple Development” and not Mac App Store.

On Linux (or any machine with `openssl`):

```bash
mkdir -p ~/scalattice-apple && cd ~/scalattice-apple
openssl req -new -newkey rsa:2048 -nodes \
  -keyout developer-id.key \
  -out developer-id.csr \
  -subj "/CN=Robottik Ltd/C=GB"
```

1. [Certificates, Identifiers & Profiles](https://developer.apple.com/account/resources/certificates/list) → **+**
2. Select **Developer ID Application** → Continue
3. Upload `developer-id.csr`
4. Download the certificate (`developer-id.cer`)

Convert to a `.p12` (pick a strong password; you will type it once into the helper script):

```bash
cd ~/scalattice-apple
openssl x509 -inform DER -in developer-id.cer -out developer-id.pem
openssl pkcs12 -export \
  -inkey developer-id.key \
  -in developer-id.pem \
  -out developer-id.p12
```

Signing identity string (replace name + Team ID to match the cert):

```text
Developer ID Application: Robottik Ltd (TEAMID)
```

Exact name is on the certificate details page. If codesign later says it cannot find the identity, copy the string from Keychain on a Mac or from `openssl pkcs12 -info -in developer-id.p12`.

## 3. App Store Connect API key (notarization)

1. [App Store Connect](https://appstoreconnect.apple.com) → **Users and Access** → **Integrations** → **App Store Connect API**
2. **Team Keys** → **+**
3. Name e.g. `scalattice-agent-notary`, access **Developer** or **Admin**
4. Download `AuthKey_XXXXXXXXXX.p8` (Apple shows it **once**)
5. Note **Key ID** (`XXXXXXXXXX`) and **Issuer ID** (UUID at the top of the API page)

## 4. Put secrets on the public repo

```bash
cd /path/to/scalattice-agent
./scripts/setup-apple-secrets.sh scalattice/scalattice-agent
```

It asks for the `.p12` path, password, identity, Team ID, `.p8` path, Key ID, and Issuer ID, then runs `gh secret set`. It does not print the values.

| Secret | Source |
|--------|--------|
| `APPLE_DEVELOPER_ID_P12_BASE64` | helper (from `.p12`) |
| `APPLE_P12_PASSWORD` | password you set on the `.p12` |
| `APPLE_SIGNING_IDENTITY` | `Developer ID Application: … (TEAMID)` |
| `APPLE_TEAM_ID` | 10-character Team ID |
| `APPLE_API_KEY_P8` | contents of the `.p8` |
| `APPLE_API_KEY_ID` | Key ID |
| `APPLE_API_ISSUER_ID` | Issuer UUID |

If these are missing, CI still ships an **unsigned** arm64 binary (Gatekeeper blocks browser downloads).

Keep `~/scalattice-apple/` off backups you share. Do not commit those files.

## Local sign + notarize (optional Mac)

```bash
export APPLE_SIGNING_IDENTITY='Developer ID Application: Your Name (TEAMID)'
export APPLE_TEAM_ID=TEAMID
export APPLE_API_KEY_ID=XXXXXXXXXX
export APPLE_API_ISSUER_ID=xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
export APPLE_API_KEY_P8_FILE=/path/to/AuthKey_XXXXXXXXXX.p8

./scripts/sign-macos.sh dist/scalattice-agent
```

A **bare Mach-O cannot be stapled**; Gatekeeper fetches the notarization ticket from Apple on first run (needs network once).

## Intel

- CI builds `--target aarch64-apple-darwin` only
- `uname -m` must be `arm64` on the runner or the job fails
- The crate `compile_error!`s on `x86_64-apple-darwin`
- Embedded `Info.plist` sets `LSRequiresNativeExecution` and `LSArchitecturePriority = arm64`
