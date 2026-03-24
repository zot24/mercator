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

CMD ["mercator", "serve", "-b", "0.0.0.0", "-p", "3000"]
