# Full multi-stage build — for local `docker build --build-arg BIN=<tap-name> .`
FROM rust:1-slim-trixie AS builder
WORKDIR /app
COPY . .
RUN cargo build --workspace --release

FROM debian:trixie-slim AS runtime
ARG BIN

RUN apt-get update && \
    apt-get install -y --no-install-recommends libssl3 ca-certificates ffmpeg wget git git-lfs && \
    rm -rf /var/lib/apt/lists/*

# Microsoft's official ONNX Runtime: SSE4.1 baseline with runtime-dispatched
# AVX/AVX2/AVX-512 kernels. Works on CPUs without AVX2/FMA (unlike pyke's
# prebuilt that ort would otherwise download).
ARG ORT_VERSION=1.22.0
RUN if [ "$BIN" = "supertonic-tap" ]; then \
      wget -q https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/onnxruntime-linux-x64-${ORT_VERSION}.tgz -O /tmp/ort.tgz && \
      mkdir -p /opt/onnxruntime && \
      tar -xzf /tmp/ort.tgz -C /opt/onnxruntime --strip-components=1 && \
      rm /tmp/ort.tgz; \
    fi
ENV ORT_DYLIB_PATH=/opt/onnxruntime/lib/libonnxruntime.so

RUN if [ "$BIN" = "supertonic-tap" ]; then \
      git lfs install --system && \
      git clone --depth=1 https://huggingface.co/Supertone/supertonic-3 /opt/supertonic && \
      git -C /opt/supertonic lfs pull; \
    fi
ENV SUPERTONIC_MODEL_DIR=/opt/supertonic
ENV SUPERTONIC_VOICE_STYLE=/opt/supertonic/voice_styles/M1.json

RUN if [ "$BIN" = "youtube-tap" ]; then \
      printf '#!/bin/sh\nwget -q https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux -O /usr/local/bin/yt-dlp && chmod +x /usr/local/bin/yt-dlp\nexec /usr/local/bin/app "$@"\n' > /entrypoint.sh; \
    else \
      printf '#!/bin/sh\nexec /usr/local/bin/app "$@"\n' > /entrypoint.sh; \
    fi && chmod +x /entrypoint.sh

COPY --from=builder /app/target/release/${BIN} /usr/local/bin/app
ENTRYPOINT ["/entrypoint.sh"]
