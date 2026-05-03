# Build stage
FROM rust:1.86-alpine AS builder

RUN apk add --no-cache musl-dev git

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

RUN cargo build --release

# Runtime stage
FROM alpine:3.21

RUN apk add --no-cache git ca-certificates nodejs npm && \
    npm install -g obsidian-headless

COPY --from=builder /build/target/release/mercator /usr/local/bin/mercator
COPY dist/ /app/dist/

WORKDIR /app

EXPOSE 3000

# Default to a localhost-only bind. The container listens on 127.0.0.1
# inside the network namespace, so the host can NOT reach it without an
# explicit override at `docker run` time. This is intentional — exposing
# the API on the LAN unauthenticated would let any reachable peer call
# /api/agent/run (paid LLM) and /api/project/file (read any surveyed
# file). To open it up, set MERCATOR_TOKEN and override:
#
#   docker run -p 3000:3000 \
#     -e MERCATOR_TOKEN=$(openssl rand -hex 32) \
#     -v ~/code:/data/code:ro \
#     mercator mercator serve -b 0.0.0.0 -m /data/map.json
#
CMD ["mercator", "serve", "-p", "3000"]
