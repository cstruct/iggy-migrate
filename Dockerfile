# iggy-migrate on Alpine, with the docker CLI for rehearsals.
#
# `docker build .` builds the binary from source. The release workflow builds
# `--target release` instead, which copies in the static musl binaries it has
# already built and tested, so the image runs the same binary as the release
# archives and needs no emulation to build for other architectures.

# Built natively on musl, so the binary is static.
FROM rust:1.99-alpine3.22 AS build
RUN apk add --no-cache build-base cmake perl
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --locked --release --bin iggy-migrate && \
    cp target/release/iggy-migrate /usr/local/bin/

# Only COPY from here on, so building for another architecture runs nothing.
# Alpine comes with the CA certificates the Postgres ledger's TLS uses.
FROM alpine:3.22 AS base
# Rehearsals, `verify` and `squash` start throwaway Iggy servers with it.
COPY --from=docker:29.8-cli /usr/local/bin/docker /usr/local/bin/docker
LABEL org.opencontainers.image.title="iggy-migrate" \
      org.opencontainers.image.description="Forward-only migrations for Apache Iggy streams, topics and consumer groups" \
      org.opencontainers.image.source="https://github.com/cstruct/iggy-migrate" \
      org.opencontainers.image.licenses="MIT"
WORKDIR /work
ENTRYPOINT ["iggy-migrate"]
CMD ["--help"]

FROM base AS release
# dist/<arch>/iggy-migrate, from the release archives.
ARG TARGETARCH
COPY dist/${TARGETARCH}/iggy-migrate /usr/local/bin/

FROM base AS from-source
COPY --from=build /usr/local/bin/iggy-migrate /usr/local/bin/
