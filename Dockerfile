# Pubky Music Node: one static-ish binary with SQLite, ALSA output enabled.
# Build:  docker build -t pubky-music-node .
# Multi-arch (buildx, cross-compiled on the build host, no QEMU for cargo):
#         docker buildx build --platform linux/amd64,linux/arm64 -t pubky-music-node .
# Run:    docker run --rm -p 7788:7788 --device /dev/snd -v music:/music:ro -v data:/data \
#           -e PM_TOKEN=change-me -e PM_CONTACT=you@example.org pubky-music-node

FROM --platform=$BUILDPLATFORM rust:1-bookworm AS build
ARG TARGETARCH
# Native build for the host arch; for arm64 on an amd64 host cross-compile
# with the Debian multiarch ALSA headers and the GNU cross toolchain.
RUN set -eu; \
    apt-get update; \
    apt-get install -y --no-install-recommends pkg-config libasound2-dev; \
    if [ "$TARGETARCH" = "arm64" ] && [ "$(dpkg --print-architecture)" != "arm64" ]; then \
        dpkg --add-architecture arm64; \
        apt-get update; \
        apt-get install -y --no-install-recommends gcc-aarch64-linux-gnu libc6-dev-arm64-cross libasound2-dev:arm64 cmake; \
        rustup target add aarch64-unknown-linux-gnu; \
    fi; \
    rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
ENV CARGO_INCREMENTAL=0
RUN set -eu; \
    if [ "$TARGETARCH" = "arm64" ] && [ "$(dpkg --print-architecture)" != "arm64" ]; then \
        export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
               CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
               AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar \
               PKG_CONFIG_ALLOW_CROSS=1 \
               PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig; \
        cargo build --release -p pm-node --features alsa --target aarch64-unknown-linux-gnu; \
        install -D target/aarch64-unknown-linux-gnu/release/pm-node /out/pm-node; \
    else \
        cargo build --release -p pm-node --features alsa; \
        install -D target/release/pm-node /out/pm-node; \
    fi

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends libasound2 ca-certificates alsa-utils \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 1000 -g audio -d /data pm
COPY --from=build /out/pm-node /usr/local/bin/pm-node
COPY deploy/docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh && mkdir -p /data /music && chown pm:audio /data
USER pm
VOLUME ["/data", "/music"]
EXPOSE 7788
ENV PM_LISTEN=0.0.0.0:7788 PM_DATA_DIR=/data PM_FOLDERS=/music RUST_LOG=info
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["serve"]
