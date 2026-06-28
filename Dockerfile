# syntax=docker/dockerfile:1.7

FROM rust:1-bookworm AS builder

WORKDIR /src

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        cmake \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY . .

RUN cargo build --workspace --release --all-features \
    && mkdir -p /tmp/runtime-payload/config /tmp/runtime-payload/data/storage /tmp/runtime-payload/static \
    && install -m 0755 target/release/qq-maid-bot /tmp/runtime-payload/qq-maid-bot \
    && install -m 0755 scripts/botctl.sh /tmp/runtime-payload/botctl.sh \
    && install -m 0755 scripts/diagnose-network.sh /tmp/runtime-payload/diagnose-network.sh \
    && install -m 0755 scripts/validate-runtime.sh /tmp/runtime-payload/validate-runtime.sh \
    && install -m 0644 runtime/README.md /tmp/runtime-payload/README.md \
    && install -m 0644 runtime/config/.env.example /tmp/runtime-payload/.env.example \
    && cp -R runtime/config/. /tmp/runtime-payload/config/ \
    && cp -R runtime/static/. /tmp/runtime-payload/static/ \
    && find /tmp/runtime-payload/config -type f ! -name '*.example.*' ! -name '.env.example' -delete \
    && bash scripts/validate-release-runtime.sh /tmp/runtime-payload

FROM debian:bookworm-slim AS runtime

WORKDIR /app/runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        tzdata \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p config data/storage logs run static

COPY --from=builder /tmp/runtime-payload/ /app/runtime/

RUN chmod +x /app/runtime/qq-maid-bot \
    /app/runtime/botctl.sh \
    /app/runtime/diagnose-network.sh \
    /app/runtime/validate-runtime.sh

ENV LLM_SERVER_HOST=0.0.0.0 \
    LLM_SERVER_PORT=8787 \
    APP_DB_FILE=data/storage/app.db \
    RUST_LOG=info,qq_maid_gateway_rs=debug

EXPOSE 8787
VOLUME ["/app/runtime/config", "/app/runtime/data"]

ENTRYPOINT ["/app/runtime/qq-maid-bot"]
