<p align="center">
  <img src="https://scalattice.com/resources/images/marketing/earth.png" alt="Scalattice" width="1500" height="500">
</p>

# Scalattice Agent

Open-source GPU agent for the [Scalattice](https://scalattice.com) inference network,
a product of [Robottik Ltd](https://robottik.co.uk).

Install it on a machine with a GPU, connect with a provider token, and Scalattice Cloud can route inference jobs to you.

## Install

### Linux

```bash
curl -fsSL https://scalattice.cloud/install/agent | sh -s -- --token slt_provider_YOUR_TOKEN
source ~/.config/scalattice/agent.env
scalattice-agent status
```

Full agent logs go to `~/.local/share/scalattice/agent.log` (always verbose; rotated at 8 MiB with one backup). `scalattice-agent logs` tails that file.

### Windows

Download and run **[ScalatticeAgentSetup-x86_64.exe](https://github.com/scalattice/scalattice-agent/releases/latest/download/ScalatticeAgentSetup-x86_64.exe)** from [GitHub Releases](https://github.com/scalattice/scalattice-agent/releases) (or use **Download Windows installer** on the Providers dashboard).

1. Run the setup wizard (approve SmartScreen if prompted)
2. Paste your `slt_provider_…` token when asked
3. Finish — PATH is configured and the background agent starts

Setup guide: https://scalattice.cloud/install/agent-setup

The agent registers a **boot** scheduled task (runs as SYSTEM before sign-in) plus logon Startup/Task rails. Use the notification area icon for status, token changes, and live logs. Full detail is written to `%LOCALAPPDATA%\Scalattice\logs\agent.log` (rotated at 8 MiB, one backup). The tray Live log can show a Simplified view; the file itself is always verbose.

### Uninstall

```bash
scalattice-agent uninstall --yes
```

Add `--purge` to also delete cached model weights.

Releases are self-contained binaries (plus bundled runtime libraries where allowed). You can also build from source with Rust/Cargo.

### Hardware support

Release builds include **CUDA** (NVIDIA) and **Vulkan** (AMD/Intel/ARM) on Linux and Windows.

| Hardware | What the host needs |
|----------|---------------------|
| NVIDIA GTX/RTX | NVIDIA driver installed (`nvidia-smi` works) |
| AMD / Intel GPU | Vendor Vulkan ICD (AMD Adrenalin / Intel graphics driver). **ROCm is not required.** Discrete AMD/Intel are detected on Linux (`lspci`) and Windows (Win32 video controllers) and run through Vulkan when enabled in the Providers dashboard. |
| CPU only | Nothing extra — the agent connects; inference uses the CPU backend |

When both NVIDIA and AMD/Intel are enabled, the agent prefers **CUDA**. AMD/Intel-only pools use the same load cascade as NVIDIA (`gpu-full` → offload → CPU). Check agent logs for `Vulkan pool: pinning ggml backend device(s)`.

Do **not** redistribute `vulkan-1.dll` — providers get it from their GPU driver. Build machines need the [Vulkan SDK](https://vulkan.lunarg.com/) (for `glslc` only).

NVIDIA’s `libcuda.so` / `nvcuda.dll` cannot be redistributed. Machines without a GPU driver still run the agent; GPU inference activates when the driver is present.

## Quick start

1. Sign in at [scalattice.cloud](https://scalattice.cloud) → **Providers**
2. Register as a provider and create an **agent token** (`slt_provider_…`)
3. On your GPU machine:

```bash
curl -fsSL https://scalattice.cloud/install/agent | sh -s -- --token slt_provider_…
source ~/.config/scalattice/agent.env
scalattice-agent status
```

The install script saves your token and starts the background agent. To change the token later:

```bash
scalattice-agent set-token --token slt_provider_…
```

## Environment

| Variable | Description |
|----------|-------------|
| `SCALATTICE_AGENT_TOKEN` | Provider agent token (`slt_provider_…`) |

The agent connects to Scalattice Cloud using the built-in endpoint. Region, models, and schedule are managed in the Providers dashboard — not via local flags.

## Protocol

See [docs/AGENT_PROTOCOL.md](docs/AGENT_PROTOCOL.md).

## Build from source

```bash
# x86_64 Linux (NVIDIA + AMD/Intel Vulkan)
cargo build --release --features gpu

# aarch64 Linux (NVIDIA + ARM Vulkan) — build natively on ARM
cargo build --release --no-default-features --features arm-gpu
```

```powershell
# x86_64 Windows (NVIDIA CUDA + AMD/Intel Vulkan) — run in PowerShell
# Requires Vulkan SDK on the build machine (glslc). Providers do not need the SDK.
.\scripts\build-release.ps1
```

CI release binaries: **CUDA + Vulkan** on x86_64/aarch64 Linux and x86_64 Windows.

## Community

| Document | Purpose |
|----------|---------|
| [CONTRIBUTING.md](CONTRIBUTING.md) | How to report issues and when we accept changes |
| [SECURITY.md](SECURITY.md) | Responsible disclosure for security issues |
| [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) | Community standards |

## License

MIT. See [LICENSE](LICENSE).
