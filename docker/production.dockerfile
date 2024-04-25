FROM rust:bullseye AS builder

RUN apt-get update \
    && apt-get install -y cmake \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY . .

RUN cargo build --release --bin batch_downloader


FROM debian:bullseye-slim

RUN apt-get update \
    && apt-get install -y openssl ca-certificates curl jq \
    && rm -rf /var/lib/apt/lists/*

RUN update-ca-certificates

COPY ./scripts/*.sh /
RUN chmod +x /*.sh

WORKDIR /app

COPY --from=builder /app/target/release/batch_downloader /usr/local/bin
CMD ["/start.sh"]
