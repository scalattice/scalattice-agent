# Scalattice Agent

Open-source GPU operator client for the [Scalattice](https://scalattice.com) inference network.

Connects to Scalattice Cloud over WebSocket, registers your GPU, and accepts inference jobs routed by the hypervisor.

## Install

From any machine with `curl`:

```bash
curl -fsSL https://scalattice.cloud/install/agent | sh -s -- --token slt_provider_YOUR_TOKEN
source ~/.config/scalattice/agent.env
scalattice-agent status
scalattice-agent connect   # background service (installed automatically with --token)
```

Use `scalattice-agent connect --foreground` for debugging in the terminal.

Install script downloads a release binary from [GitHub](https://github.com/Robottik-Software/Scalattice-Client) when available, or builds from source with Rust/Cargo.

## Quick start

1. Sign in at [scalattice.cloud](https://scalattice.cloud) → **Providers**
2. Register as a provider and create an **agent token** (`slt_provider_…`)
3. On your GPU machine:

```bash
curl -fsSL https://scalattice.cloud/install/agent | sh -s -- --token slt_provider_…
source ~/.config/scalattice/agent.env
scalattice-agent status
scalattice-agent connect
```

## Environment

| Variable | Description |
|----------|-------------|
| `SCALATTICE_AGENT_TOKEN` | Provider agent token (`slt_provider_…`) |
| `SCALATTICE_AGENT_WS` | WebSocket URL (default `wss://api.scalattice.cloud/v1/operators/agent/ws`) |
| `SCALATTICE_AGENT_REGION` | Region: `auto`, `us`, `eu`, or `ap` |
| `SCALATTICE_AGENT_MODELS` | Comma-separated catalog model ids to advertise |

Demo mode is configured per GPU in the Scalattice Cloud **Providers** dashboard (off by default).

## Protocol

See [docs/AGENT_PROTOCOL.md](docs/AGENT_PROTOCOL.md) in this repository.

## Build from source

```bash
# x86_64 Linux (NVIDIA + AMD/Intel Vulkan)
cargo build --release --features gpu

# aarch64 Linux (NVIDIA + ARM Vulkan) — build natively on ARM
cargo build --release --no-default-features --features arm-gpu
```

CI release binaries include **CUDA + Vulkan** on both x86_64 and aarch64.

## License

MIT. See [LICENSE](LICENSE).
