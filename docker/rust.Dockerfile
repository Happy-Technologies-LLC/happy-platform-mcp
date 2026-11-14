# Happy Platform MCP 7.x (Rust) - OCI image
# Copyright 2025 Happy Technologies LLC
# Licensed under Apache License 2.0
#
# Packages prebuilt glibc release binaries; nothing is compiled here.
# Build context layout (filled by .github/workflows/publish-image.yml from the
# release run's archives):
#   x86_64-unknown-linux-gnu/{happy-platform-mcp,servicenow-mcp-server,happy-platform-mcp-http}
#   aarch64-unknown-linux-gnu/{...same three...}
#
#   docker buildx build -f docker/rust.Dockerfile --platform linux/amd64,linux/arm64 <context>

# The binaries link libdbus-1.so.3 (Secret Service keyring store), which pulls
# in libsystemd0 and its deps. distroless/cc ships only libc/libgcc/libstdc++,
# so the shared libraries are fetched for TARGETARCH on the build platform
# (apt-get download + dpkg-deb -x: no emulation, nothing executed for the target).
FROM --platform=$BUILDPLATFORM debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc AS libs
ARG TARGETARCH
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-linux-gnu ;; \
      arm64) triple=aarch64-linux-gnu ;; \
      *) echo "unsupported TARGETARCH $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    dpkg --add-architecture "$TARGETARCH"; \
    apt-get update -qq; \
    mkdir -p /pkgs /extract "/out/usr/lib/$triple" /out/usr/local/bin /scripts; \
    cd /pkgs; \
    apt-get download \
      "libdbus-1-3:$TARGETARCH" "libsystemd0:$TARGETARCH" "libcap2:$TARGETARCH" \
      "libgcrypt20:$TARGETARCH" "liblzma5:$TARGETARCH" "libzstd1:$TARGETARCH" \
      "liblz4-1:$TARGETARCH" "libgpg-error0:$TARGETARCH"; \
    for deb in /pkgs/*.deb; do dpkg-deb -x "$deb" /extract; done; \
    find /extract -name '*.so.*' \( -type f -o -type l \) -exec cp -a {} "/out/usr/lib/$triple/" \; ; \
    for bin in happy-platform-mcp servicenow-mcp-server happy-platform-mcp-http; do \
      ln -s "/app/bin/$bin" "/out/usr/local/bin/$bin"; \
    done; \
    rm -rf /var/lib/apt/lists/*

FROM scratch AS bin-amd64
COPY x86_64-unknown-linux-gnu/ /

FROM scratch AS bin-arm64
COPY aarch64-unknown-linux-gnu/ /

FROM bin-${TARGETARCH} AS bin

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f

LABEL org.opencontainers.image.title="happy-platform-mcp" \
      org.opencontainers.image.description="Model Context Protocol server for the ServiceNow platform (Rust)" \
      org.opencontainers.image.licenses="Apache-2.0" \
      io.modelcontextprotocol.server.name="io.github.nickzitzer/happy-platform-mcp"

COPY --from=libs /out/ /
# Binaries live in /app/bin so the 6.x legacy registry path still resolves:
# <exe>/../../config/servicenow-instances.json = /app/config/servicenow-instances.json
# (the docker-compose.yml mount). /usr/local/bin holds symlinks on PATH.
COPY --from=bin --chmod=0755 / /app/bin/

WORKDIR /app
# Generated fix scripts are written to <cwd>/scripts; the nonroot user owns it.
COPY --from=libs --chown=65532:65532 /scripts /app/scripts

EXPOSE 3000

# Listen on every container interface so the published port is reachable.
# Startup still fails unless HAPPY_MCP_API_TOKEN is supplied at run time.
ENV HAPPY_MCP_BIND_HOST=0.0.0.0

USER nonroot:nonroot

# Same default as the 6.x image (node src/server.js): the HTTP transport.
# Stdio: docker run -i --rm <image> servicenow-mcp-server
CMD ["happy-platform-mcp-http"]
