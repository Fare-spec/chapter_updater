FROM rust:1.85-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --home-dir /app --shell /usr/sbin/nologin appuser \
    && mkdir -p /app/data \
    && chown -R appuser:appuser /app

WORKDIR /app

COPY --from=builder /app/target/release/chapter_updater /usr/local/bin/chapter_updater

ENV POLL_INTERVAL_SECS=60
ENV STATE_FILE=/app/data/chapter_state.txt

VOLUME ["/app/data"]

USER appuser

ENTRYPOINT ["chapter_updater"]
