# syntax=docker/dockerfile:1.7

FROM node:26-bookworm AS frontend
WORKDIR /app/frontend
COPY frontend/package.json frontend/pnpm-lock.yaml ./
RUN npm install -g pnpm@10.33.4 && pnpm install --frozen-lockfile
COPY openapi /app/openapi
COPY frontend ./
RUN pnpm generate:client && pnpm build

FROM rust:1.95-bookworm AS rust-build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY migrations ./migrations
RUN cargo build --release --locked --workspace

FROM debian:bookworm-slim AS runtime
# hadolint ignore=DL3008
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates poppler-utils \
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
