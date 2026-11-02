# Telemetry collector

Receives the opt-in usage pings from `src/usage-telemetry.js`. One dependency-free file, Node 20+.

```bash
node collector/telemetry-collector.mjs              # listens on 127.0.0.1:18610
node collector/telemetry-collector.mjs --summarize  # tool-call totals, sessions by version/platform/transport/node
```

It accepts only `POST /api/mcp/telemetry` with a JSON body of at most 16 KiB that matches the README payload exactly (204; otherwise 400, 404, 405, 413, 415, or 429 above 60 requests a minute per client). Each accepted ping is appended to `data/YYYY-MM-DD.jsonl` (UTC) as the payload plus `received_at`. Client IPs are used only for the in-memory rate limit; IPs, headers and user agents are never written.

Settings: `HAPPY_MCP_COLLECTOR_PORT` (default `18610`), `HAPPY_MCP_COLLECTOR_DATA_DIR` (default `data/` next to the script).

Production runs on Build01 as the user unit `happy-mcp-telemetry.service` from `~/happy-mcp-telemetry/`, exposed by Tailscale Funnel on port 10000 for that one path only:

```bash
tailscale funnel --bg --https=10000 --set-path=/api/mcp/telemetry http://127.0.0.1:18610/api/mcp/telemetry
```
