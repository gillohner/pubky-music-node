# CI only (ci.yml, bundle): the node's image, from a pm-node binary built on the runner
# (Ubuntu 24.04, so the same glibc), with the release image's entrypoint and user.
FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends libasound2t64 ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && (userdel -r ubuntu 2>/dev/null || true) && useradd -r -u 1000 -g audio -d /data pm
COPY pm-node /usr/local/bin/pm-node
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh /usr/local/bin/pm-node && mkdir -p /data /music && chown pm:audio /data
USER pm
VOLUME ["/data", "/music"]
ENV PM_LISTEN=0.0.0.0:7788 PM_DATA_DIR=/data PM_FOLDERS=/music RUST_LOG=info
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["serve"]
