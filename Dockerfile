# OculOS — Linux container build
# Note: UI automation requires a running desktop session (AT-SPI2).
# This image is useful for building from source or CI/CD pipelines.
#
# The dashboard is embedded in the binary — no static/ directory is needed.
#
# Security: the container binds to 0.0.0.0, so OculOS requires an API token.
# Without OCULOS_TOKEN a random token is generated and printed in the logs
# (`docker logs <container>`); open the dashboard at
# http://<host>:7878/?token=<token> and send `X-OculOS-Token: <token>` from clients.
# To choose the token yourself:
#   docker run -e OCULOS_TOKEN=$(openssl rand -hex 16) -p 127.0.0.1:7878:7878 oculos

FROM rust:1-bookworm AS builder

RUN apt-get update && apt-get install -y \
    libatspi2.0-dev \
    libdbus-1-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# index.html is compiled into the binary (include_str!)
COPY static/index.html ./static/index.html
RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    libatspi2.0-0 \
    libdbus-1-3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/oculos /usr/local/bin/oculos

EXPOSE 7878

ENTRYPOINT ["oculos"]
CMD ["--bind", "0.0.0.0:7878"]
