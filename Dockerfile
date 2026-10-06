# Happy MCP Server - Docker Image
# Model Context Protocol Server for the ServiceNow Platform
# Copyright 2025 Happy Technologies LLC
# Licensed under Apache License 2.0

FROM node:24-alpine AS production

# Set working directory
WORKDIR /app

# Security: Update Alpine packages to patch vulnerabilities
RUN apk update && \
    apk upgrade --no-cache && \
    rm -rf /var/cache/apk/*

# Install the exact production dependency graph from the committed lockfile
COPY package.json package-lock.json ./

RUN npm ci --omit=dev

# Runtime doesn't need package managers; remove them to reduce attack surface
RUN rm -rf /usr/local/lib/node_modules/npm && \
    rm -f /usr/local/bin/npm /usr/local/bin/npx

# Copy application source
COPY src/ ./src/
COPY config/ ./config/
COPY docs/ ./docs/
COPY LICENSE ./
COPY NOTICE ./
COPY README.md ./

# Create directory for config
RUN mkdir -p /app/config

# Expose HTTP server port (for SSE transport)
EXPOSE 3000

# Health check
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
  CMD node -e "require('http').get('http://localhost:3000/health', (r) => {process.exit(r.statusCode === 200 ? 0 : 1)})"

# Default to HTTP server (SSE transport)
# Use stdio-server.js for Claude Desktop integration
CMD ["node", "src/server.js"]
