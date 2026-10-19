# Pubky Music Node: one static-ish binary with SQLite, ALSA output enabled.
# Build:  docker build -t pubky-music-node .
# Run:    docker run --rm -p 7788:7788 --device /dev/snd -v music:/music:ro -v data:/data \
#           -e PM_TOKEN=change-me -e PM_CONTACT=you@example.org pubky-music-node

FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends libasound2-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
ENV CARGO_INCREMENTAL=0
RUN cargo build --release -p pm-node --features alsa

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends libasound2 ca-certificates alsa-utils \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 1000 -g audio -d /data pm
COPY --from=build /src/target/release/pm-node /usr/local/bin/pm-node
COPY deploy/docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh && mkdir -p /data /music && chown pm:audio /data
USER pm
VOLUME ["/data", "/music"]
EXPOSE 7788
ENV PM_LISTEN=0.0.0.0:7788 PM_DATA_DIR=/data PM_FOLDERS=/music RUST_LOG=info
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["serve"]
