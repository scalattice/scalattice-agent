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

To remove the agent, service, and local config:

```bash
scalattice-agent uninstall --yes
```

Add `--purge` to also delete cached model weights.

Install script downloads a self-contained release from [GitHub](https://github.com/Robottik-Software/Scalattice-Client) (binary + bundled runtime libraries), or builds from source with Rust/Cargo. The install script itself lives in the **scalattice-server** repo (`frontend/public/install/agent`), not here.

### Hardware support

Release builds include **CUDA** (NVIDIA) and **Vulkan** (AMD/Intel/ARM) backends. The installer ships every library we are allowed to redistribute (Vulkan loader, llama.cpp modules, etc.) — no `apt install libvulkan1` or similar.

| Hardware | What the host needs |
|----------|---------------------|
| NVIDIA GTX/RTX | NVIDIA driver installed (provides `libcuda.so`; standard on GPU workstations) |
| AMD / Intel GPU | Vulkan ICD from the vendor (usually already present with GPU drivers) |
| CPU only | Nothing extra — agent connects; inference uses CPU backend |

We cannot bundle NVIDIA's `libcuda.so` (license). Machines without a GPU driver still run the agent; GPU inference activates when the driver is present.

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

The agent always connects to `wss://api.scalattice.cloud/v1/operators/agent/ws`. Region, models, and routing policy are assigned by Scalattice Cloud from your provider profile and the platform catalog — not from local flags or env vars.

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

## Community

| Document | Purpose |
|----------|---------|
| [CONTRIBUTING.md](CONTRIBUTING.md) | How to report issues and when we accept changes |
| [SECURITY.md](SECURITY.md) | Responsible disclosure for security issues |
| [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) | Community standards |

## License

MIT. See [LICENSE](LICENSE).
