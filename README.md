# Scalattice Agent

Open-source GPU operator agent for the [Scalattice](https://scalattice.com) inference network.

Connects to Scalattice Cloud over WebSocket, registers your GPU, and accepts inference jobs routed by the hypervisor.

## Install

### Linux

From any machine with `curl`:

```bash
curl -fsSL https://scalattice.cloud/install/agent | sh -s -- --token slt_provider_YOUR_TOKEN
source ~/.config/scalattice/agent.env
scalattice-agent status
```

### Windows

Download and run **[ScalatticeAgentSetup-x86_64.exe](https://github.com/scalattice/scalattice-agent/releases/latest/download/ScalatticeAgentSetup-x86_64.exe)** from GitHub Releases (or use **Download Windows installer** on the Providers dashboard).

1. Run the setup wizard (approve SmartScreen if prompted)
2. Paste your `slt_provider_…` token when asked
3. Finish. The installer adds Scalattice to your PATH and starts the background agent.

Setup guide: https://scalattice.cloud/install/agent-setup

The agent runs as a logon scheduled task. Use the notification area icon for status, token changes, and live logs. Log file: `%LOCALAPPDATA%\Scalattice\logs\agent.log`.

To remove the agent, background service, and local config:

```bash
scalattice-agent uninstall --yes          # Linux
scalattice-agent uninstall --yes          # Windows (same command)
```

Add `--purge` to also delete cached model weights.

Install scripts download self-contained releases from [GitHub](https://github.com/scalattice/scalattice-agent) (binary + bundled runtime libraries), or build from source with Rust/Cargo. The Linux install script lives in **scalattice-server** (`frontend/public/install/agent`); Windows uses `ScalatticeAgentSetup-x86_64.exe` from GitHub Releases. After install, use the notification area icon for status, token changes, and live logs.

### Hardware support

Release builds include **CUDA** (NVIDIA) and **Vulkan** (AMD/Intel/ARM) backends on Linux. Windows releases use **CUDA only** (NVIDIA). The installer ships redistributable runtime libraries where allowed — no `apt install libvulkan1` or similar on Linux.

| Hardware | What the host needs |
|----------|---------------------|
| NVIDIA GTX/RTX | NVIDIA driver installed (provides `libcuda.so` / CUDA on Windows; standard on GPU workstations) |
| AMD / Intel GPU | Vulkan ICD from the vendor (Linux releases only; usually already present with GPU drivers) |
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
```

The install script saves your token and starts the background agent automatically. To change the token later:

```bash
scalattice-agent set-token --token slt_provider_…
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

```powershell
# x86_64 Windows (NVIDIA CUDA) — run in PowerShell
.\scripts\build-release.ps1
```

CI release binaries: **CUDA + Vulkan** on x86_64 and aarch64 Linux; **CUDA only** on x86_64 Windows.

## Community

| Document | Purpose |
|----------|---------|
| [CONTRIBUTING.md](CONTRIBUTING.md) | How to report issues and when we accept changes |
| [SECURITY.md](SECURITY.md) | Responsible disclosure for security issues |
| [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) | Community standards |

## License

MIT. See [LICENSE](LICENSE).
