# Scalattice GPU Agent Protocol

Public WebSocket protocol for `scalattice-agent` and other operator clients connecting to [Scalattice Cloud](https://scalattice.cloud).

## Connect

```
wss://api.scalattice.cloud/v1/operators/agent/ws
Authorization: Bearer <provider_setup_code>
```

Setup codes are created on the **Providers** dashboard (`slt_provider_…` prefix).

## Message flow

1. **Server → client** `ready`: assigns `nodeId`, sends the model catalog:

```json
{
  "type": "ready",
  "nodeId": "agent-uuid",
  "catalog": [
    {
      "modelId": "mistral-large",
      "displayName": "Mistral Large",
      "runtimeModel": "mistralai/Mistral-Large-Instruct-2407",
      "maxContextTokens": 32768,
      "regions": ["us", "eu"]
    }
  ]
}
```

2. **Client → server** `register`: declare region, models, and machine specs:

```json
{
  "type": "register",
  "region": "us",
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
    "demoMode": false,
    "ready": false,
    "jobState": "idle",
    "statusLabel": "Connected · no model runtime loaded",
    "loadedModels": []
  }
}
```

`gpuName` and `vramGb` are kept for compatibility. Prefer sending the full `specs` object. Include `runtime` so the Providers dashboard can show demo mode, readiness, and active jobs.

3. **Server → client** `registered`: the machine is in the live operator pool.

```json
{
  "type": "registered",
  "nodeId": "agent-uuid",
  "models": ["mistral-large"]
}
```

4. **Client ↔ server** `heartbeat` / `pong` every ~25s. Heartbeats may refresh live machine specs:

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
    "demoMode": true,
    "ready": true,
    "jobState": "idle",
    "statusLabel": "Idle · demo mode (echo only)",
    "loadedModels": []
  }
}
```

The reference agent sends an extra heartbeat when a job starts or finishes so `jobState: busy` appears on the dashboard immediately. GPU detection uses NVIDIA (`nvidia-smi`), AMD (`rocm-smi`), and PCI graphics devices (`lspci`), plus host CPU/RAM via `/proc`.

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
| `SCALATTICE_AGENT_TOKEN` | Provider setup code (`slt_provider_…`) |
| `SCALATTICE_AGENT_WS` | WebSocket URL (default `wss://api.scalattice.cloud/v1/operators/agent/ws`) |
| `SCALATTICE_AGENT_REGION` | Region: `auto`, `us`, `eu`, or `ap` |
| `SCALATTICE_AGENT_MODELS` | Comma-separated catalog model ids to advertise |
| `SCALATTICE_AGENT_DEMO` | Set to `1` for echo-only connectivity testing |

## Background service (Linux + systemd)

```bash
scalattice-agent service install    # user unit, auto-restart on disconnect
scalattice-agent service status
sudo loginctl enable-linger $USER   # optional: start at boot without login
```

Foreground `connect` (v1.0.3+) also reconnects automatically after network drops.

## Implementation notes

- Load model weights using `runtimeModel` from the catalog. Do not hardcode model names.
- Advertise only models from the catalog your hardware can run.
- Stay connected and send heartbeats while online; the Providers dashboard shows live specs while you are connected.
- Use your provider schedule in Scalattice Cloud to control when your GPU accepts paid work.
