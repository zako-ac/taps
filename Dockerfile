# Full multi-stage build — for local `docker build --build-arg BIN=<tap-name> .`
FROM rust:1-slim AS builder
WORKDIR /app
COPY . .
RUN cargo build --workspace --release

FROM debian:bookworm-slim AS runtime
ARG BIN

RUN apt-get update && \
    apt-get install -y --no-install-recommends libssl3 ca-certificates ffmpeg wget && \
    rm -rf /var/lib/apt/lists/*

RUN if [ "$BIN" = "youtube-tap" ]; then \
      printf '#!/bin/sh\nwget -q https://github.com/yt-dlp/yt-dlp/releases/download/2026.03.17/yt-dlp_linux -O /usr/local/bin/yt-dlp && chmod +x /usr/local/bin/yt-dlp\nexec /usr/local/bin/app "$@"\n' > /entrypoint.sh; \
    else \
      printf '#!/bin/sh\nexec /usr/local/bin/app "$@"\n' > /entrypoint.sh; \
    fi && chmod +x /entrypoint.sh

COPY --from=builder /app/target/release/${BIN} /usr/local/bin/app
ENTRYPOINT ["/entrypoint.sh"]
