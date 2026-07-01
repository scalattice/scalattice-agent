# Security Policy

## Supported versions

Security fixes are applied to **currently supported release tags** of
`scalattice-agent`. Install the latest release from
[GitHub Releases](https://github.com/Robottik-Software/scalattice-agent/releases)
or via:

```bash
curl -fsSL https://scalattice.cloud/install/agent | sh -s -- --token slt_provider_…
```

| Version | Supported |
|---------|-----------|
| Latest tagged release (`v*`) | Yes |
| `main` branch (pre-release) | Best-effort; use latest tag for production GPUs |
| Older tags | No — upgrade to the latest release |

## Reporting a vulnerability

**Please do not report security vulnerabilities through public GitHub issues.**

If you believe you have found a security issue in this repository or in how the
agent interacts with Scalattice Cloud, report it privately to:

**[security@robottik.co.uk](mailto:security@robottik.co.uk)**

If you do not receive a response within five business days, you may follow up at
**[admin@robottik.co.uk](mailto:admin@robottik.co.uk)** with the subject line
`Scalattice Agent security`.

Include as much of the following as you can:

- Description of the issue and potential impact
- Steps to reproduce, or proof-of-concept if available
- Affected version(s) or commit hash
- Your environment (OS, CPU/GPU, agent version)
- Any suggested mitigation (optional)

## What to expect

1. **Acknowledgement** — We aim to confirm receipt within five business days.
2. **Triage** — We assess severity and whether the issue affects the agent,
   install script, release artifacts, or Scalattice Cloud services.
3. **Fix** — Confirmed issues are prioritized; patches may land in `main` and
   ship in a tagged release.
4. **Disclosure** — We coordinate reasonable disclosure timing with you. Please
   allow us time to deploy fixes before public discussion.

We appreciate responsible disclosure and may acknowledge reporters in release
notes when they wish to be credited.

## Scope

**In scope (examples):**

- Remote code execution or privilege escalation in the agent or installer
- Token theft, leakage, or bypass of authentication to Scalattice Cloud
- Integrity issues in release tarballs or install script delivery
- Memory safety bugs in release builds with a plausible exploit path

**Out of scope (examples):**

- Issues requiring physical access or already-compromised provider machines
- Social engineering of provider tokens
- Denial of service against a single operator GPU without broader impact
- Findings in third-party dependencies with no fix available upstream (we still
  want to know — we may track them internally)
- Scalattice Cloud backend or router bugs (report those to the same security
  address; we will route internally)

## Operator security notes

- Treat `slt_provider_…` tokens like passwords. Rotate them from the Providers
  dashboard if exposed.
- The agent connects only to Scalattice Cloud; do not run patched builds that
  change the WebSocket endpoint.
- Inference traffic is TLS-terminated at Scalattice; operators can see job
  content on their own hardware today. See
  [docs/AGENT_PROTOCOL.md](docs/AGENT_PROTOCOL.md) for the current trust model.

## Safe harbor

We do not pursue legal action against researchers who report issues in good faith
and follow this policy, provided they avoid privacy violations, data destruction,
and service disruption to other providers.
