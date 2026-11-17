FROM rust:1.99-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/tipper /usr/local/bin/tipper
USER 65534:65534
ENTRYPOINT ["/usr/local/bin/tipper"]
CMD ["--config", "/etc/tipper/config.yaml"]
