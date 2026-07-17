# Contributing to Scalattice Agent

Thank you for your interest in **scalattice-agent**, the open-source GPU agent for
[Scalattice](https://scalattice.com), a product of
[Robottik Ltd](https://robottik.co.uk).

This repository is published under the [MIT License](LICENSE). You may read,
study, build, and run the software freely. Day-to-day development is done by
Robottik Ltd to keep the agent aligned with Scalattice Cloud and supported
hardware targets.

We do not routinely accept drive-by pull requests. That is intentional: the
agent must stay compatible with Scalattice Cloud and supported hardware targets.
Unexpected changes can break providers who rely on released builds.

## What we welcome

| Type | Where | Notes |
|------|--------|--------|
| **Bug reports** | [GitHub Issues](https://github.com/scalattice/scalattice-agent/issues) | Repro steps, versions, logs |
| **Security reports** | See [SECURITY.md](SECURITY.md) | Please do not open public issues |
| **Documentation fixes** | Issue first, or small PR | Typos and factual corrections |
| **Feature ideas** | GitHub Issues | Describe the provider use case |
| **Prior agreement to contribute code** | Email **admin@robottik.co.uk** | Required before substantial PRs |

If you are unsure whether a change fits, open an issue before writing code.

## What we generally do not merge

- Changes that point the agent at non-Scalattice endpoints or alter trust boundaries
- New CLI flags or env vars that override cloud-managed placement or model policy
- Large refactors without a prior maintainer agreement
- Dependencies that complicate cross-platform GPU release builds
- Generated or vendored blobs without a clear license and maintenance plan

## Development setup

Requirements:

- Rust stable (see [release workflow](.github/workflows/release.yml) for CI targets)
- Linux (x86_64 or aarch64) for GPU feature builds
- NVIDIA driver and/or Vulkan stack when testing GPU inference locally

```bash
git clone https://github.com/scalattice/scalattice-agent.git
cd scalattice-agent

# x86_64 — CUDA + Vulkan
cargo build --release --features gpu

# aarch64 — build natively on ARM
cargo build --release --no-default-features --features arm-gpu
```

Run locally (requires a provider token from [scalattice.cloud/providers](https://scalattice.cloud/providers)):

```bash
export SCALATTICE_AGENT_TOKEN='slt_provider_…'
cargo run --release --features gpu -- foreground
```

Protocol details: [docs/AGENT_PROTOCOL.md](docs/AGENT_PROTOCOL.md).

## Code guidelines

When contributing with maintainer approval:

- Match existing Rust style in the crate (`rustfmt` defaults, minimal scope)
- Keep the agent **token-only** for local configuration; policy belongs in Scalattice Cloud
- Do not log tokens, prompts, or inference payloads at info level
- Prefer focused commits with clear messages (no unrelated changes)
- Update docs when behavior or provider-facing commands change

## Pull request process

1. **Discuss first** for anything beyond a typo or doc fix
2. Fork and branch from `main`
3. Keep the diff small and explain *why* in the PR description
4. Confirm you have built the relevant feature set (`gpu` or `arm-gpu`)
5. Maintainers review on their schedule; silence is not rejection — we may redirect you to an issue

We may close PRs that bypass this process without review.

## Releases

Authorized maintainers publish releases with:

```bash
./scripts/release.sh
```

See [scripts/README.md](scripts/README.md). Do not push tags or publish releases
unless you are an authorized maintainer.

## Community standards

Participation in issues and discussions is governed by our
[Code of Conduct](CODE_OF_CONDUCT.md).

## Questions

- Provider setup: [scalattice.cloud/providers](https://scalattice.cloud/providers)
- General contact: [support@scalattice.com](mailto:support@scalattice.com)
