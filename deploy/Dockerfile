# Surface Coordinator server image. Build from the repository root:
#   docker build -f deploy/Dockerfile -t surface-coordinator .

FROM node:24-bookworm-slim AS ui
WORKDIR /src/ui
COPY ui/package.json ui/package-lock.json ./
RUN npm ci
COPY ui/ ./
RUN npm run build

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# The update public key, if the owner has generated one, is compiled in.
COPY release-keys ./release-keys
# The coordinator embeds the web UI at compile time.
COPY --from=ui /src/ui/dist ./ui/dist
RUN test -f ui/dist/index.html && cargo build --release -p coordinator

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data surface-coordinator \
    && mkdir /data && chown surface-coordinator /data
COPY --from=build /src/target/release/coordinator /usr/local/bin/coordinator
USER surface-coordinator
# Clients, the API and the web UI share one port; TLS is terminated by a
# reverse proxy in front, which must pass WebSocket upgrades through.
ENV SC_DATA_DIR=/data \
    SC_BIND=0.0.0.0:8080 \
    SC_SECURE_COOKIES=true
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["coordinator"]
CMD ["serve"]
