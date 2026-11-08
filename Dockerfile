# Minimal build image for storage-analyzer (toolchain stays inside the container).
FROM rust:1.99-alpine

ARG UID=1000
ARG GID=1000

RUN apk add --no-cache build-base \
  && rustup component add clippy rustfmt \
  && chmod -R a+rX "${RUSTUP_HOME}" "${CARGO_HOME}"

USER ${UID}:${GID}
WORKDIR /work
