# Docker Deployment Guide

Deploy ServiceNow MCP Server using Docker for easy, consistent, and portable deployment.

The container runs the HTTP/SSE transport for **one trusted operator**. Every request, health checks included, must carry `Authorization: Bearer <HAPPY_MCP_API_TOKEN>`, and the container refuses to start without a valid token. Shared multi-user hosting (several people or tenants behind one container or token) isn't supported. See [HTTP Transport Security](../README.md#http-transport-security) for the full contract.

## 🔑 Generate the HTTP token

```bash
# 32 random bytes as 64 hex characters. Store it in a secret manager or an
# untracked .env file; never bake it into an image or commit it.
export HAPPY_MCP_API_TOKEN="$(openssl rand -hex 32)"
```

The image sets `HAPPY_MCP_BIND_HOST=0.0.0.0` so the published port is reachable. Because a container sees host-side requests arrive on its own interface, tell it which `Host` values clients use with `HAPPY_MCP_ALLOWED_HOSTS`, for example `localhost:3000,127.0.0.1:3000`. Publish the port on loopback (`-p 127.0.0.1:3000:3000`) and put a TLS reverse proxy in front for remote access.

## 🚀 Quick Start

### Option 1: Docker Hub (Recommended)

```bash
# Pull the latest image
docker pull nczitzer/happy-platform-mcp:latest

# Run with environment variables (single instance)
docker run -d \
  -p 127.0.0.1:3000:3000 \
  -e HAPPY_MCP_API_TOKEN \
  -e HAPPY_MCP_ALLOWED_HOSTS=localhost:3000,127.0.0.1:3000 \
  -e SERVICENOW_INSTANCE_URL=https://dev123456.service-now.com \
  -e SERVICENOW_USERNAME=admin \
  -e SERVICENOW_PASSWORD=your-password \
  --name servicenow-mcp-server \
  nczitzer/happy-platform-mcp:latest
```

`-e HAPPY_MCP_API_TOKEN` without a value copies the token from your shell environment, so it doesn't appear in the command line.

### Option 2: Docker Compose

```bash
# Create an untracked .env file
cat > .env <<EOF
HAPPY_MCP_API_TOKEN=$(openssl rand -hex 32)
SERVICENOW_INSTANCE_URL=https://dev123456.service-now.com
SERVICENOW_USERNAME=admin
SERVICENOW_PASSWORD=your-password
SERVICENOW_AUTH_TYPE=basic
EOF
chmod 600 .env

# Start with docker-compose (fails fast if HAPPY_MCP_API_TOKEN is unset)
docker-compose up -d
```

The bundled `docker-compose.yml` publishes on `127.0.0.1:3000`, defaults `HAPPY_MCP_ALLOWED_HOSTS` to `localhost:3000,127.0.0.1:3000`, and runs an authenticated health check.

### Option 3: Build Locally

```bash
# Build the image
docker build -t servicenow-mcp-server .

# Run the container
docker run -d -p 127.0.0.1:3000:3000 \
  -e HAPPY_MCP_API_TOKEN \
  -e HAPPY_MCP_ALLOWED_HOSTS=localhost:3000,127.0.0.1:3000 \
  -e SERVICENOW_INSTANCE_URL=https://dev123456.service-now.com \
  -e SERVICENOW_USERNAME=admin \
  -e SERVICENOW_PASSWORD=your-password \
  servicenow-mcp-server
```

## 🌐 Multi-Instance Configuration

For multi-instance support, mount your config file:

```bash
# Create config file
cp config/servicenow-instances.json.example config/servicenow-instances.json
# Edit with your instances

# Run with mounted config
docker run -d \
  -p 127.0.0.1:3000:3000 \
  -e HAPPY_MCP_API_TOKEN \
  -e HAPPY_MCP_ALLOWED_HOSTS=localhost:3000,127.0.0.1:3000 \
  -v $(pwd)/config/servicenow-instances.json:/app/config/servicenow-instances.json:ro \
  --name servicenow-mcp-server \
  nczitzer/happy-platform-mcp:latest
```

Or use docker-compose:

```yaml
services:
  servicenow-mcp-server:
    image: nczitzer/happy-platform-mcp:latest
    ports:
      - "127.0.0.1:3000:3000"
    environment:
      - HAPPY_MCP_API_TOKEN=${HAPPY_MCP_API_TOKEN:?Set HAPPY_MCP_API_TOKEN}
      - HAPPY_MCP_ALLOWED_HOSTS=localhost:3000,127.0.0.1:3000
    volumes:
      - ./config/servicenow-instances.json:/app/config/servicenow-instances.json:ro
```

## 🔍 Health Check

`/health` requires the bearer token like every other route. The image's `HEALTHCHECK` (and the one in `docker-compose.yml`) reads `HAPPY_MCP_API_TOKEN` from the container environment at run time and calls `http://127.0.0.1:${PORT:-3000}/health`. Neither the image nor the health check command embeds the token. The running container's environment does hold it, though, so `docker inspect <container>` (its `Config.Env`) and anyone with access to the Docker socket can read it. Restrict Docker socket access, or use your orchestrator's secrets mechanism.

```bash
# Check container health
docker ps

# Test health endpoint (401 without the token)
curl -H "Authorization: Bearer $HAPPY_MCP_API_TOKEN" http://localhost:3000/health

# Check logs
docker logs servicenow-mcp-server
```

## 🛠️ Environment Variables

### HTTP Transport

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `HAPPY_MCP_API_TOKEN` | Yes | - | Bearer token: 64 hex (`openssl rand -hex 32`) or 43 base64url characters |
| `HAPPY_MCP_ALLOWED_HOSTS` | Usually | empty | Comma-separated `host[:port]` values clients send in `Host`, e.g. `localhost:3000,127.0.0.1:3000` or your proxy's public name |
| `HAPPY_MCP_ALLOWED_ORIGINS` | No | empty | Comma-separated exact browser origins; empty rejects any request carrying `Origin` |
| `HAPPY_MCP_BIND_HOST` | No | `0.0.0.0` in the image | Listen address inside the container |
| `PORT` | No | `3000` | Listen port inside the container |
| `SSE_KEEPALIVE_INTERVAL` | No | `15000` | SSE keepalive interval in milliseconds |

### Single Instance Mode

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `SERVICENOW_INSTANCE_URL` | Yes | - | ServiceNow instance URL |
| `SERVICENOW_USERNAME` | Yes | - | ServiceNow username |
| `SERVICENOW_PASSWORD` | Yes | - | ServiceNow password |
| `SERVICENOW_AUTH_TYPE` | No | `basic` | Authentication type |

### Multi-Instance Mode

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `SERVICENOW_INSTANCE` | No | `default` | Instance name from config file |

## 📊 Resource Requirements

**Minimum:**
- CPU: 0.5 cores
- Memory: 256MB
- Disk: 500MB

**Recommended:**
- CPU: 1 core
- Memory: 512MB
- Disk: 1GB

## 🔒 Security Best Practices

### 1. Keep the HTTP token secret

- Generate it with `openssl rand -hex 32`; the server checks its format but can't tell whether it's random.
- Pass it at run time from a secret store or an untracked, `chmod 600` `.env` file, never through a Dockerfile `ENV`, image layer or committed compose file.
- Rotate it by restarting the container with a new value, then update every client.
- One token is one operator. Don't share it between people; run a separate container per operator.

### 2. Use Docker Secrets (Production)

```bash
# Create secrets
echo "your-password" | docker secret create servicenow_password -

# Run with secrets
docker service create \
  --name servicenow-mcp-server \
  --secret servicenow_password \
  -e SERVICENOW_INSTANCE_URL=https://prod.service-now.com \
  -e SERVICENOW_USERNAME=admin \
  nczitzer/happy-platform-mcp:latest
```

### 3. Use Read-Only Config Mount

```bash
docker run -d \
  -e HAPPY_MCP_API_TOKEN \
  -v $(pwd)/config/servicenow-instances.json:/app/config/servicenow-instances.json:ro \
  nczitzer/happy-platform-mcp:latest
```

### 4. Network Isolation

```bash
# Create isolated network
docker network create mcp-network

# Run in isolated network, published on loopback only
docker run -d \
  --network mcp-network \
  -p 127.0.0.1:3000:3000 \
  -e HAPPY_MCP_API_TOKEN \
  -e HAPPY_MCP_ALLOWED_HOSTS=localhost:3000,127.0.0.1:3000 \
  nczitzer/happy-platform-mcp:latest
```

### 5. Reverse proxy with TLS

Terminate TLS at the proxy. If the proxy forwards the public name in `Host`, add that exact authority (for example `HAPPY_MCP_ALLOWED_HOSTS=mcp.example.com`). If it rewrites `Host` to the upstream address, add that instead (for example `servicenow-mcp-server:3000` on a Docker network). Disable response buffering for SSE, raise the read timeout above the keepalive interval, and pass the client's `Authorization` header through unchanged. The server ignores `X-Forwarded-*` and `Forwarded` headers.

Failed-auth throttling keys on the connecting address. Behind a reverse proxy, or behind Docker's port publishing where host-side clients can all arrive from the same gateway address, every client shares one budget. Any unauthenticated client that can reach the endpoint can then keep the operator locked out with ten bad requests a minute. Restrict who can reach the proxy (VPN, IP allow-list or the proxy's own client authentication) and rate-limit at the proxy.

## 🔄 Updates & Maintenance

### Update to Latest Version

```bash
# Pull latest image
docker pull nczitzer/happy-platform-mcp:latest

# Stop and remove old container
docker stop servicenow-mcp-server
docker rm servicenow-mcp-server

# Start new container
docker run -d -p 127.0.0.1:3000:3000 \
  -e HAPPY_MCP_API_TOKEN \
  -e HAPPY_MCP_ALLOWED_HOSTS=localhost:3000,127.0.0.1:3000 \
  -e SERVICENOW_INSTANCE_URL=... \
  --name servicenow-mcp-server \
  nczitzer/happy-platform-mcp:latest
```

### Migrating from unauthenticated releases

Earlier images served `/health`, `/instances` and `/mcp` without authentication on loopback. Before upgrading:

1. Generate `HAPPY_MCP_API_TOKEN` and pass it to the container.
2. Set `HAPPY_MCP_ALLOWED_HOSTS` to the `host:port` your clients use.
3. Add `Authorization: Bearer <token>` to every client: curl scripts, monitoring probes and MCP SSE clients (both the `GET /mcp` stream and message `POST`s).
4. Replace custom health checks with authenticated ones; an unauthenticated probe now gets `401` and marks the container unhealthy.

### View Logs

```bash
# Follow logs
docker logs -f servicenow-mcp-server

# Last 100 lines
docker logs --tail 100 servicenow-mcp-server
```

### Restart Container

```bash
docker restart servicenow-mcp-server
```

## 🚀 Production Deployment

### Kubernetes (k8s)

SSE sessions live in one process, so run a single replica per operator and keep the Service internal (reach it through an authenticated ingress or a port-forward). Kubernetes `httpGet` probes can't read a secret into a header, so probe with the same authenticated `exec` command the image uses.

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: servicenow-mcp-server
spec:
  replicas: 1
  selector:
    matchLabels:
      app: servicenow-mcp-server
  template:
    metadata:
      labels:
        app: servicenow-mcp-server
    spec:
      containers:
      - name: servicenow-mcp-server
        image: nczitzer/happy-platform-mcp:latest
        ports:
        - containerPort: 3000
        env:
        - name: HAPPY_MCP_API_TOKEN
          valueFrom:
            secretKeyRef:
              name: happy-mcp-http
              key: api-token
        - name: HAPPY_MCP_ALLOWED_HOSTS
          value: "servicenow-mcp-server:3000,mcp.example.com"
        - name: SERVICENOW_INSTANCE_URL
          valueFrom:
            secretKeyRef:
              name: servicenow-credentials
              key: instance-url
        - name: SERVICENOW_USERNAME
          valueFrom:
            secretKeyRef:
              name: servicenow-credentials
              key: username
        - name: SERVICENOW_PASSWORD
          valueFrom:
            secretKeyRef:
              name: servicenow-credentials
              key: password
        resources:
          requests:
            memory: "256Mi"
            cpu: "250m"
          limits:
            memory: "512Mi"
            cpu: "500m"
        livenessProbe:
          exec:
            command:
            - node
            - -e
            - "require('http').get({host:'127.0.0.1',port:process.env.PORT||3000,path:'/health',headers:{authorization:'Bearer '+process.env.HAPPY_MCP_API_TOKEN}},(r)=>{r.resume();process.exit(r.statusCode===200?0:1)}).on('error',()=>process.exit(1))"
          initialDelaySeconds: 10
          periodSeconds: 30
---
apiVersion: v1
kind: Service
metadata:
  name: servicenow-mcp-server
spec:
  selector:
    app: servicenow-mcp-server
  ports:
  - port: 3000
    targetPort: 3000
  type: ClusterIP
```

```bash
kubectl create secret generic happy-mcp-http --from-literal=api-token="$(openssl rand -hex 32)"
```

### Docker Swarm

```bash
# Initialize swarm
docker swarm init

# Deploy stack (export HAPPY_MCP_API_TOKEN first; the compose file requires it)
docker stack deploy -c docker-compose.yml servicenow-mcp
```

## 🐛 Troubleshooting

### Container Won't Start

```bash
# Check logs; a missing or malformed token prints
# "HTTP startup refused: HAPPY_MCP_API_TOKEN ..."
docker logs servicenow-mcp-server

# Check which variables are set without printing their values
docker exec servicenow-mcp-server sh -c 'env | cut -d= -f1 | sort'

# Run interactively
docker run -it --rm \
  -e SERVICENOW_INSTANCE_URL=... \
  nczitzer/happy-platform-mcp:latest \
  sh
```

### Health Check Failing

```bash
# Inspect recent health check results
docker inspect --format '{{json .State.Health}}' servicenow-mcp-server

# Run the authenticated check manually inside the container
docker exec servicenow-mcp-server node -e "require('http').get({host:'127.0.0.1',port:process.env.PORT||3000,path:'/health',headers:{authorization:'Bearer '+process.env.HAPPY_MCP_API_TOKEN}},(r)=>console.log(r.statusCode))"
```

### Connection Issues

```bash
# Verify port mapping
docker port servicenow-mcp-server

# Test from host
curl -i -H "Authorization: Bearer $HAPPY_MCP_API_TOKEN" http://localhost:3000/health
```

- `401 Unauthorized`: missing, duplicate or wrong bearer token.
- `403 Host not allowed`: add the `host:port` you're connecting to (as sent in `Host`) to `HAPPY_MCP_ALLOWED_HOSTS`.
- `403 Origin not allowed`: add the exact browser origin to `HAPPY_MCP_ALLOWED_ORIGINS`.
- `429`: too many failed attempts from your address in the last minute, or too many open SSE sessions. Wait for `Retry-After`.

## 📦 Available Tags

- `latest` - Latest stable release
- `2.1.1` - Specific version
- `2.1` - Minor version (auto-updates patch releases)
- `2` - Major version (auto-updates minor/patch releases)

## 🔗 Links

- **Docker Hub:** https://hub.docker.com/r/nczitzer/mcp-servicenow-nodejs
- **GitHub:** https://github.com/Happy-Technologies-LLC/mcp-servicenow-nodejs
- **npm:** https://www.npmjs.com/package/servicenow-mcp-server
- **MCP Registry:** https://registry.modelcontextprotocol.io/servers/io.github.nickzitzer/servicenow-nodejs

## 📄 License

MIT License - Copyright © 2025 Happy Technologies LLC
