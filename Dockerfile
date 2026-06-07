# syntax=docker/dockerfile:1.7

FROM node:26-bookworm AS frontend
WORKDIR /app/frontend
COPY frontend/package.json frontend/pnpm-lock.yaml ./
RUN npm install -g pnpm@10.34.1 && pnpm install --frozen-lockfile
COPY openapi /app/openapi
COPY frontend ./
RUN pnpm generate:client && pnpm build

FROM rust:1.96-bookworm AS rust-build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY migrations ./migrations
RUN cargo build --release --locked --workspace

FROM debian:trixie-slim AS runtime
# apt-get upgrade picks up Debian Security patches for libraries that ship
# pre-installed in the base layer (libc6, libcap2, libsystemd0, libudev1, …)
# but would otherwise stay at whatever version Docker Hub baked into the
# trixie-slim tag. Without it, image scans flag CVEs that are already fixed
# upstream.
# hadolint ignore=DL3008
RUN apt-get update \
  && apt-get -y --no-install-recommends upgrade \
  && apt-get install -y --no-install-recommends ca-certificates curl poppler-utils \
  && rm -rf /var/lib/apt/lists/*
RUN useradd --system --uid 10001 --create-home archivist
WORKDIR /app
COPY --from=rust-build /app/target/release/archivist-api /usr/local/bin/archivist-api
COPY --from=rust-build /app/target/release/archivist-worker /usr/local/bin/archivist-worker
COPY --from=frontend /app/frontend/dist /app/frontend/dist
COPY migrations /app/migrations
ENV ARCHIVIST_STATIC_DIR=/app/frontend/dist
ENV ARCHIVIST_MIGRATIONS_DIR=/app/migrations
USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/archivist-api"]
