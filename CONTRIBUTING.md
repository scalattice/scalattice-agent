# Contributing to Scalattice Agent

Thank you for your interest in **scalattice-agent**, the open-source GPU operator
client for [Scalattice](https://scalattice.com).

This repository is published under the [MIT License](LICENSE). You may read,
study, build, and run the software freely. **Most day-to-day development is done
by [Robottik Ltd](https://robottik.co.uk)** to keep the agent aligned with
Scalattice Cloud, the operator protocol, and supported hardware targets.

We do not routinely accept drive-by pull requests. That is intentional: the
agent must stay compatible with the production hypervisor, provider dashboard,
and release pipeline. Unexpected changes can break live GPU fleets.

## What we welcome

| Type | Where | Notes |
|------|--------|--------|
| **Bug reports** | [GitHub Issues](https://github.com/Robottik-Software/Scalattice-Client/issues) | Repro steps, versions, logs |
| **Security reports** | See [SECURITY.md](SECURITY.md) | Please do not open public issues |
| **Documentation fixes** | Issue first, or small PR | Typos and factual corrections |
| **Feature ideas** | GitHub Issues | Describe the provider/operator use case |
| **Prior agreement to contribute code** | Email **admin@robottik.co.uk** | Required before substantial PRs |

If you are unsure whether a change fits, open an issue before writing code.

## What we generally do not merge

- Changes that point the agent at non-Scalattice endpoints or alter trust boundaries
- New CLI flags or env vars that let operators override routing, region, or models
- Large refactors without a prior maintainer agreement
- Dependencies that complicate cross-platform GPU release builds
- Generated or vendored blobs without a clear license and maintenance plan

## Development setup

Requirements:

- Rust stable (see [release workflow](.github/workflows/release.yml) for CI targets)
- Linux (x86_64 or aarch64) for GPU feature builds
- NVIDIA driver and/or Vulkan stack when testing GPU inference locally

```bash
git clone https://github.com/Robottik-Software/Scalattice-Client.git
cd Scalattice-Client

# x86_64 — CUDA + Vulkan
cargo build --release --features gpu

# aarch64 — build natively on ARM
cargo build --release --no-default-features --features arm-gpu
```

Run locally (requires a provider token from [scalattice.cloud/providers](https://scalattice.cloud/providers)):

```bash
export SCALATTICE_AGENT_TOKEN='slt_provider_…'
cargo run --release --features gpu -- connect --foreground
```

Protocol details: [docs/AGENT_PROTOCOL.md](docs/AGENT_PROTOCOL.md).

## Code guidelines

When contributing with maintainer approval:

- Match existing Rust style in the crate (`rustfmt` defaults, minimal scope)
- Keep the agent **token-only** for configuration; cloud policy belongs on the hypervisor
- Do not log tokens, prompts, or inference payloads at info level
- Prefer focused commits with clear messages (no unrelated changes)
- Update docs when behavior or operator-facing commands change

## Pull request process

1. **Discuss first** for anything beyond a typo or doc fix
2. Fork and branch from `main`
3. Keep the diff small and explain *why* in the PR description
4. Confirm you have built the relevant feature set (`gpu` or `arm-gpu`)
5. Maintainers review on their schedule; silence is not rejection — we may redirect you to an issue

We may close PRs that bypass this process without review.

## Releases

Tagged releases (`v*`) are cut by maintainers. Binaries are published on the
[Releases](https://github.com/Robottik-Software/Scalattice-Client/releases) page
and served via `https://scalattice.cloud/install/agent`.

### Fast path (recommended): build on your machine

CI compiles llama.cpp with CUDA/Vulkan from source — about an hour on a cold cache.
**Do not commit binaries to git.** Upload them to GitHub Releases instead:

```bash
# bump version in Cargo.toml, commit, then:
chmod +x scripts/build-release.sh scripts/publish-release.sh
./scripts/publish-release.sh v1.0.31
```

That builds x86_64 locally, uploads tarballs, tags with `[local]`, and skips the
CI compile job. For aarch64, build on ARM hardware first:

```bash
./scripts/build-release.sh aarch64-unknown-linux-gnu
./scripts/publish-release.sh v1.0.31 origin dist/scalattice-agent-aarch64-unknown-linux-gnu.tar.gz
```

Commit `Cargo.lock` after the first local build — it keeps CI cache hits high when
you do let Actions compile.

### Slow path: CI build on tag push

Push a tag **without** `[local]` in the tag message (see `scripts/release-v1.0.*.sh`)
and wait for GitHub Actions to compile both targets.

The curl installer script is maintained in **scalattice-server**
(`frontend/public/install/agent`), not in this repo. Edit it there and deploy
the server frontend — do not add a duplicate `scripts/install.sh` here.

Do not push tags or publish releases unless you are an authorized maintainer.

## Community standards

Participation in issues and discussions is governed by our
[Code of Conduct](CODE_OF_CONDUCT.md).

## Questions

- Provider setup: [scalattice.com/providers](https://scalattice.com/providers)
- General contact: [admin@robottik.co.uk](mailto:admin@robottik.co.uk)
