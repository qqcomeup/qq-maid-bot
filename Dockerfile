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

RUN cargo build --workspace --release --all-features

FROM debian:bookworm-slim AS runtime

WORKDIR /app/runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        tzdata \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p config data/storage logs run static

COPY --from=builder /src/target/release/qq-maid-bot /app/runtime/qq-maid-bot
COPY --from=builder /src/scripts/botctl.sh /app/runtime/botctl.sh
COPY --from=builder /src/scripts/diagnose-network.sh /app/runtime/diagnose-network.sh
COPY --from=builder /src/scripts/validate-runtime.sh /app/runtime/validate-runtime.sh
COPY --from=builder /src/runtime/README.md /app/runtime/README.md
COPY --from=builder /src/runtime/config/.env.example /app/runtime/.env.example
COPY --from=builder /src/runtime/config /app/runtime/config
COPY --from=builder /src/runtime/static /app/runtime/static

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
