# Build fortis-index + fortis-edge into one small image.
#   docker build -f deploy/Dockerfile -t fortis-backend .

FROM rust:1-slim-bookworm AS build
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p fortis-index -p fortis-edge

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 10001 fortis
COPY --from=build /src/target/release/fortis-index /usr/local/bin/
COPY --from=build /src/target/release/fortis-edge  /usr/local/bin/
USER fortis
# fortis-index: 8094  ·  fortis-edge: 8098
EXPOSE 8094 8098
ENTRYPOINT ["/usr/local/bin/fortis-edge"]
