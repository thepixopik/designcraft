FROM rust:1.95-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

RUN cargo build --locked --release -p designcraft-cli

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    fontconfig \
    fonts-dejavu-core \
    fonts-liberation2 \
    fonts-noto-core \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -u 10001 designcraft \
    && mkdir -p /workspace /usr/local/share/fonts/designcraft \
    && chown -R designcraft:designcraft /workspace

COPY --from=builder /src/target/release/designcraft-cli /usr/local/bin/designcraft-cli
COPY --from=builder /src/assets/fonts/ /usr/local/share/fonts/designcraft/
RUN fc-cache -f

USER designcraft
WORKDIR /workspace

ENV PORT=8789
EXPOSE 8789

ENTRYPOINT ["/bin/sh", "-c", "exec designcraft-cli mcp --http 0.0.0.0:${PORT:-8789} --automation-read-root /workspace --automation-write-root /workspace"]
