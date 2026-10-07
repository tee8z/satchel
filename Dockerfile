# syntax=docker/dockerfile:1
#
# Satchel container image. It packages a published release archive and
# compiles nothing. The build context must hold, for each target platform,
# the release archive and its checksum file:
#
#   dist/satchel-<version>-x86_64-linux.tar.gz
#   dist/satchel-<version>-x86_64-linux.tar.gz.sha256
#   dist/satchel-<version>-aarch64-linux.tar.gz
#   dist/satchel-<version>-aarch64-linux.tar.gz.sha256
#
#   docker buildx build --build-arg SATCHEL_VERSION=<version> \
#     --platform linux/amd64,linux/arm64 -t satchel:<version> .
#
# See docs/docker.md. TEST NETWORKS ONLY: Satchel refuses to start on mainnet.

# Unpacks on the build machine's own platform, so a multi-platform build
# needs no emulation.
FROM --platform=$BUILDPLATFORM debian:trixie-slim AS unpack
ARG SATCHEL_VERSION
ARG TARGETARCH
WORKDIR /work
COPY dist/ ./
RUN set -eu; \
    if [ -z "${SATCHEL_VERSION:-}" ]; then \
      echo "Pass --build-arg SATCHEL_VERSION=<version>" >&2; exit 1; \
    fi; \
    case "$TARGETARCH" in \
      amd64) system=x86_64-linux ;; \
      arm64) system=aarch64-linux ;; \
      *) echo "No Satchel release for $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    name="satchel-$SATCHEL_VERSION-$system"; \
    sha256sum --check --strict "$name.tar.gz.sha256"; \
    tar -xzf "$name.tar.gz"; \
    mkdir -p /out/usr/local/bin /out/usr/local/share /out/state/data; \
    install -m 0755 "$name/bin/satchel" /out/usr/local/bin/satchel; \
    cp -R "$name/share/satchel" /out/usr/local/share/satchel

# glibc, CA certificates, and a nonroot user (65532), nothing else.
FROM gcr.io/distroless/cc-debian13:nonroot
ARG SATCHEL_VERSION
LABEL org.opencontainers.image.title="satchel" \
      org.opencontainers.image.description="Multi-account Lightning wallet and Lightning Address server for test networks only" \
      org.opencontainers.image.source="https://github.com/tee8z/satchel" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.version="${SATCHEL_VERSION}"
COPY --from=unpack /out/usr/local/ /usr/local/
COPY --from=unpack --chown=65532:65532 /out/state/ /
# Configuration and credentials are mounted at /etc/satchel; relative
# credential paths in config.toml resolve there. The database lives in /data.
ENV SATCHEL_CONFIG=/etc/satchel/config.toml \
    SATCHEL_SERVER__BIND_ADDRESS=0.0.0.0:8095 \
    SATCHEL_SERVER__DATABASE_PATH=/data/wallet.db \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
USER 65532:65532
VOLUME ["/data"]
EXPOSE 8095
ENTRYPOINT ["/usr/local/bin/satchel"]
