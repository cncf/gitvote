# Build gitvote
FROM rust:1-alpine3.19 as builder
RUN apk --no-cache add musl-dev perl make
WORKDIR /gitvote
COPY src src
COPY templates templates
COPY Cargo.lock Cargo.lock
COPY Cargo.toml Cargo.toml
WORKDIR /gitvote/src
RUN cargo build --release

# Final stage
FROM alpine:3.19.1
RUN apk --no-cache add ca-certificates && addgroup -S gitvote && adduser -S gitvote -G gitvote
USER gitvote
WORKDIR /home/gitvote
COPY --from=builder /gitvote/target/release/gitvote /usr/local/bin
