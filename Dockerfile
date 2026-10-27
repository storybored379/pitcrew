FROM rust:1.99.1-bookworm AS build
WORKDIR /build
COPY . .
RUN cargo build --locked --release -p pitcrew

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 pitcrew \
    && useradd --uid 10001 --gid 10001 --no-create-home pitcrew \
    && install -d -o pitcrew -g pitcrew -m 0700 /var/lib/pitcrew
COPY --from=build /build/target/release/pitcrew /usr/local/bin/pitcrew
USER 10001:10001
WORKDIR /var/lib/pitcrew
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl --noproxy '*' --fail --silent --max-time 4 http://127.0.0.1:7400/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/pitcrew"]
CMD ["--config", "/config/pitcrew.toml"]
