# syntax=docker/dockerfile:1
# Multi-stage: Vite UI → embed-ui release binary → distroless/cc.
# Default CMD: demo into /data if empty, then serve --addr 0.0.0.0:8080.

FROM node:22-bookworm-slim AS ui
WORKDIR /src/ui
COPY ui/package.json ui/package-lock.json ./
RUN npm ci
COPY ui/ ./
RUN npm run build

FROM rust:1.92.0-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY --from=ui /src/ui/dist ./ui/dist
RUN cargo build -p mushroomdb-cli --bin mushroomdb --features embed-ui --release \
    && mkdir -p /data \
    && chown 65532:65532 /data

# debian12 nonroot
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:adcd20c7b4c988b73cbfbddb26d2eee574571e6d7c9ffea29b3821e0690efb77
COPY --from=build /src/target/release/mushroomdb /usr/local/bin/mushroomdb
COPY --from=build --chown=65532:65532 /data /data
USER nonroot
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/mushroomdb"]
CMD ["serve", "/data", "--addr", "0.0.0.0:8080", "--demo-if-empty"]
