# Build clowarden
FROM rust:1-alpine3.17 as builder
RUN apk --no-cache add musl-dev perl make
WORKDIR /clowarden
COPY src src
COPY templates templates
COPY Cargo.lock Cargo.lock
COPY Cargo.toml Cargo.toml
WORKDIR /clowarden/src
RUN cargo build --release

# Final stage
FROM alpine:3.17.3
RUN apk --no-cache add ca-certificates && addgroup -S clowarden && adduser -S clowarden -G clowarden
USER clowarden
WORKDIR /home/clowarden
COPY --from=builder /clowarden/target/release/clowarden /usr/local/bin
