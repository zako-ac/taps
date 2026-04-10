# Full multi-stage build — for local `docker build --build-arg BIN=<tap-name> .`
FROM rust:1-slim AS builder
WORKDIR /app
COPY . .
RUN cargo build --workspace --release

FROM debian:bookworm-slim AS runtime
ARG BIN

RUN apt-get update && \
    apt-get install -y --no-install-recommends libssl3 ca-certificates && \
    if [ "$BIN" = "youtube-tap" ]; then \
      apt-get install -y --no-install-recommends python3 yt-dlp; \
    fi && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/${BIN} /usr/local/bin/app
ENTRYPOINT ["/usr/local/bin/app"]
