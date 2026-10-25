# --- build ---
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY migrations ./migrations
COPY templates ./templates
COPY static ./static
COPY src ./src
RUN cargo build --release --locked

# --- runtime ---
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --home /data --create-home batchmail
COPY --from=build /src/target/release/batchmail /usr/local/bin/batchmail
USER batchmail
WORKDIR /data
ENV BIND_ADDR=0.0.0.0:8080 \
    DATABASE_URL=sqlite:///data/batchmail.db
EXPOSE 8080
VOLUME ["/data"]
ENTRYPOINT ["batchmail"]
