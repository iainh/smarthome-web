FROM rust:1.88-slim-bookworm AS builder

RUN apt-get update \
    && apt-get install --yes --no-install-recommends build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/*.rs ./src/
COPY templates/*.html ./templates/
RUN cargo build --locked --release

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/tddp-client /usr/local/bin/tddp-client

ENV BIND_ADDR=0.0.0.0:3000
ENV DATABASE_PATH=/data/tddp-client.sqlite3

VOLUME ["/data"]
EXPOSE 3000

ENTRYPOINT ["/usr/local/bin/tddp-client"]
