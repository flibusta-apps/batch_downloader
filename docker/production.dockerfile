FROM rust:bookworm AS chef

RUN apt-get update \
    && apt-get install -y cmake \
    && rm -rf /var/lib/apt/lists/*

RUN cargo install cargo-chef --locked

WORKDIR /app


FROM chef AS planner

COPY . .
RUN cargo chef prepare --recipe-path recipe.json


FROM chef AS builder

COPY --from=planner /app/recipe.json recipe.json
# Build (and cache) dependencies only.
RUN cargo chef cook --release --recipe-path recipe.json

COPY . .
RUN cargo build --release --bin batch_downloader


FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y openssl ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN update-ca-certificates

RUN useradd --system --uid 10001 --no-create-home app \
    && mkdir -p /tmp/batch_downloader \
    && chown app:app /tmp/batch_downloader

COPY ./scripts/start.sh /start.sh
RUN chmod +x /start.sh

WORKDIR /app

COPY --from=builder /app/target/release/batch_downloader /usr/local/bin

USER app

CMD ["/start.sh"]
