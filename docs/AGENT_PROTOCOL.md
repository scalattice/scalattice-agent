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
  "models": ["mistral-large"],
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
  "models": ["mistral-large"]
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
    "loadedModels": ["mistralai/Mistral-Large-Instruct-2407"]
  }
}
```

The reference agent sends an extra heartbeat when a job starts or finishes so `jobState: busy` appears on the dashboard promptly. GPU detection uses NVIDIA (`nvidia-smi`), AMD (`rocm-smi`), and PCI graphics devices (`lspci`), plus host CPU/RAM via `/proc`.

5. **Server → client** `invoke`: inference job:

```json
{
  "type": "invoke",
  "id": "request-uuid",
  "modelId": "mistral-large",
  "runtimeModel": "mistralai/Mistral-Large-Instruct-2407",
  "messages": [{ "role": "user", "content": "Hello" }]
}
```

6. **Client → server** `invoke_result` or `invoke_error`:

```json
{
  "type": "invoke_result",
  "id": "request-uuid",
  "content": "Hi there",
  "promptTokens": 12,
  "completionTokens": 8
}
```

```json
{
  "type": "invoke_error",
  "id": "request-uuid",
  "error": "Model weights not loaded"
}
```

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
