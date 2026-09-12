# Scalattice GPU Agent Protocol

WebSocket protocol used by `scalattice-agent` (and compatible agents) to connect to [Scalattice Cloud](https://scalattice.cloud).

## Connect

```
wss://api.scalattice.cloud/v1/operators/agent/ws
Authorization: Bearer <provider_token>
```

Provider tokens are created on the **Providers** dashboard (`slt_provider_…` prefix).

## Message flow

1. **Server → client** `ready`: assigns `nodeId`, sends the model catalog and compute device policy:

```json
{
  "type": "ready",
  "nodeId": "agent-uuid",
  "catalog": [ ... ]
}
```

2. **Client → server** `register`: send machine specs and runtime. Advertise catalog model ids from the `ready` message. Do not send a region — Scalattice Cloud assigns placement.

```json
{
  "type": "register",
  "models": ["example-model"],
  "gpuName": "NVIDIA RTX 4090",
  "vramGb": 24,
  "specs": {
    "gpuName": "NVIDIA RTX 4090",
    "vramGb": 24,
    "vramUsedGb": 2,
    "gpuUtilPct": 8,
    "gpuCount": 1,
    "driverVersion": "550.54.15",
    "cudaVersion": "12.4",
    "hostname": "gpu-box",
    "cpuModel": "AMD Ryzen 9 7950X",
    "ramGb": 128
  },
  "runtime": {
    "ready": false,
    "jobState": "idle",
    "statusLabel": "Connected · waiting for model weights",
    "loadedModels": []
  }
}
```

`gpuName` and `vramGb` are kept for compatibility. Prefer the full `specs` object. Include `runtime` so the Providers dashboard can show readiness and active jobs.

3. **Server → client** `registered`:

```json
{
  "type": "registered",
  "nodeId": "agent-uuid",
  "models": ["example-model"]
}
```

4. **Client ↔ server** `heartbeat` / `pong` every ~25s. The server may refresh compute device policy on `pong`:

```json
{ "type": "pong", "computeDevices": [ { "id": "gpu0", "enabled": true } ] }
```

Heartbeats may refresh live machine specs:

```json
{
  "type": "heartbeat",
  "specs": {
    "gpuName": "NVIDIA RTX 4090",
    "vramGb": 24,
    "vramUsedGb": 3,
    "gpuUtilPct": 12,
    "gpuCount": 1,
    "driverVersion": "550.54.15",
    "cudaVersion": "12.4",
    "hostname": "gpu-box",
    "cpuModel": "AMD Ryzen 9 7950X",
    "ramGb": 128
  },
  "runtime": {
    "ready": true,
    "jobState": "idle",
    "statusLabel": "Idle · ready for inference",
    "loadedModels": ["org/example-runtime"]
  }
}
```

The reference agent sends an extra heartbeat when a job starts or finishes so `jobState: busy` appears on the dashboard promptly. GPU detection uses NVIDIA (`nvidia-smi`), AMD (`rocm-smi`), and PCI graphics devices (`lspci`), plus host CPU/RAM via `/proc`.

5. **Server → client** `invoke`: inference job. Optional `"stream": true` requests token deltas:

```json
{
  "type": "invoke",
  "id": "request-uuid",
  "modelId": "example-model",
  "runtimeModel": "org/example-runtime",
  "messages": [
    { "role": "user", "content": "Hello" },
    {
      "role": "user",
      "content": "What is in this picture?",
      "images": [{ "mime": "image/png", "data": "<base64>" }]
    }
  ],
  "stream": true
}
```

Image catalog models (`jobKind: "image"`) use a second runtime. The invoke is **not** chat: no `stream`, no `messages` required. The catalog HF Diffusers repo is loaded with Python Diffusers (NVIDIA CUDA, AMD ROCm/DirectML, Intel Arc XPU, or Apple Silicon MPS). Generated PNGs are returned on `invoke_result.images` (not `content`, which is capped at 512 KiB). Isolation:

- Image runtime runs only when **both** the catalog row and the invoke have `jobKind: "image"`. A chat/VL probe (missing or `chat` `jobKind`) on an image SKU returns `image_model_chat_unsupported` — it must not generate a picture.
- `jobKind: "image"` on a chat/VL catalog row returns `chat_model_image_unsupported`.
- Split inference (`invoke_split`) is chat-only. Image SKUs return `image_model_chat_unsupported`.
- `inputImages` are edit references, not VL photos. Chat/VL jobs reject them. Vision photos stay on `messages[].images`.
- `invoke_result` with `images` must keep `content` empty and token counts at 0.

```json
{
  "type": "invoke",
  "id": "request-uuid",
  "modelId": "qwen-image",
  "runtimeModel": "Qwen/Qwen-Image",
  "jobKind": "image",
  "prompt": "A red bicycle parked in morning fog",
  "width": 1328,
  "height": 1328,
  "n": 1,
  "seed": 42,
  "inputImages": [{ "mime": "image/png", "data": "<base64>" }]
}
```

```json
{
  "type": "invoke_result",
  "id": "request-uuid",
  "content": "",
  "promptTokens": 0,
  "completionTokens": 0,
  "images": [{ "mime": "image/png", "data": "<base64>" }],
  "imageCount": 1
}
```

NVIDIA CUDA, AMD (ROCm on Linux, DirectML on Windows), Intel Arc (PyTorch XPU), or Apple Silicon (MPS). Enabling an image SKU downloads an isolated CPython (never the host's Python), a Diffusers venv (`~/.cache/scalattice/runtimes/diffusers-*`), and the HF snapshot. Disabling the last image SKU removes that runtime. The agent evicts the llama.cpp worker on that GPU for the job, then respawns it. The catalog row's HF repo is the checkpoint (Qwen-Image sizes/CFG apply only when the repo name contains `qwen-image`). Optional `inputImages` (1–4, `{ mime, data }`) are reference pictures for edit pipelines. Set `SCALATTICE_QWEN_IMAGE_STUB=1` to return a 1×1 PNG without PyTorch.

6. **Client → server** while streaming: zero or more `invoke_delta`, then terminal `invoke_result` or `invoke_error`:

```json
{ "type": "invoke_delta", "id": "request-uuid", "delta": "Hi" }
```

```json
{
  "type": "invoke_result",
  "id": "request-uuid",
  "content": "Hi there",
  "promptTokens": 12,
  "completionTokens": 8,
  "timings": {
    "modelLoadMs": 0,
    "prefillMs": 120,
    "decodeMs": 800,
    "totalMs": 950
  }
}
```

```json
{
  "type": "invoke_error",
  "id": "request-uuid",
  "error": "Model weights not loaded"
}
```

Advertise `runtime.supportsStream: true` on register/heartbeat so Scalattice Cloud can prefer native SSE for this agent.

7. **Server → client** `error` (fatal handshake errors):

```json
{
  "type": "error",
  "error": "invalid_agent_token"
}
```

## Client environment

| Variable | Description |
|----------|-------------|
| `SCALATTICE_AGENT_TOKEN` | Provider token (`slt_provider_…`) |

The WebSocket endpoint is fixed in the agent binary. Placement and schedule are controlled from the Providers dashboard.

## Background agent (Linux + systemd)

`scalattice-agent set-token` saves the machine token and starts (or restarts) a user systemd unit automatically.

```bash
scalattice-agent set-token --token slt_provider_…  # background (default after install)
scalattice-agent foreground                        # follow live logs (Ctrl+C stops watching only)
scalattice-agent status                            # also starts background if stopped
scalattice-agent uninstall --yes                   # remove agent, service, and config
scalattice-agent uninstall --yes --purge           # also delete cached model weights
sudo loginctl enable-linger $USER                  # optional: start at boot without login
```

The curl installer with `--token` writes `agent.env` and starts the background agent automatically.

## Implementation notes

- Load model weights using `runtimeModel` from the catalog. Do not hardcode model names.
- Advertise models from the `ready` catalog; Scalattice Cloud decides which jobs you receive.
- Stay connected and send heartbeats while online; the Providers dashboard shows live status while you are connected.
- Use your provider schedule in Scalattice Cloud to control when your GPU accepts paid work.
- Treat prompts and completions as sensitive: they are visible on the machine that runs the agent.
