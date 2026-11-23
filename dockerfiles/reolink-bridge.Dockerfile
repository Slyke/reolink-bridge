# syntax=docker/dockerfile:1

FROM docker.io/library/rust:1.100.0-bookworm AS build
WORKDIR /workspace

RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock en-US.json errors.json5 ./
COPY src/ ./src/

ARG TARGETPLATFORM
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/target,id=reolink-bridge-$TARGETPLATFORM,sharing=locked \
    cargo build --locked --release --bin reolink-bridge \
    && install -Dm755 target/release/reolink-bridge /out/reolink-bridge

FROM docker.io/library/debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg \
    && rm -rf /var/lib/apt/lists/* \
    && install -d -m 0700 -o 10001 -g 10001 /app/local-only

WORKDIR /app
COPY --from=build /out/reolink-bridge /usr/local/bin/reolink-bridge

ARG BUILD_HASH=unknown
ARG BUILD_VERSION=dev
ARG BUILD_DATE=unknown
ARG GIT_SHA=unknown
ARG GIT_DIRTY=false
ARG CI_RUN_ID=local

# Podman includes active build arguments in RUN cache keys. Refresh the image
# metadata even when this revision produces identical application files.
RUN test -n "$BUILD_HASH" && test -n "$BUILD_VERSION" && test -n "$BUILD_DATE" \
    && test -n "$GIT_SHA" && test -n "$GIT_DIRTY" && test -n "$CI_RUN_ID"

ENV APP_BUILD_HASH="${BUILD_HASH}" \
    APP_VERSION="${BUILD_VERSION}" \
    APP_BUILD_DATE="${BUILD_DATE}" \
    APP_GIT_SHA="${GIT_SHA}" \
    APP_GIT_DIRTY="${GIT_DIRTY}" \
    APP_CI_RUN_ID="${CI_RUN_ID}"

LABEL org.opencontainers.image.source="https://github.com/Slyke/reolink-bridge" \
      org.opencontainers.image.title="reolink-bridge" \
      org.opencontainers.image.revision="${GIT_SHA}" \
      org.opencontainers.image.version="${BUILD_VERSION}" \
      org.opencontainers.image.created="${BUILD_DATE}" \
      build.git.dirty="${GIT_DIRTY}" \
      build.ci.run-id="${CI_RUN_ID}"

# Mount local-only at runtime for the config and any generated HTTPS files.
USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/reolink-bridge"]
CMD ["./local-only/bridge.json5"]
