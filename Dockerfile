# Build clowarden
FROM rust:1-alpine3.18 as builder
RUN apk --no-cache add musl-dev perl make
WORKDIR /clowarden
COPY src src
COPY templates templates
COPY Cargo.lock Cargo.lock
COPY Cargo.toml Cargo.toml
WORKDIR /clowarden/src
RUN cargo build --release

# Build frontend
FROM node:16-alpine3.17 AS frontend-builder
RUN apk --no-cache add git
WORKDIR /web
COPY web .
ENV NODE_OPTIONS=--max_old_space_size=4096
RUN yarn install --network-concurrency 1
RUN yarn build

# Final stage
FROM alpine:3.18.0
RUN apk --no-cache add ca-certificates && addgroup -S clowarden && adduser -S clowarden -G clowarden
USER clowarden
WORKDIR /home/clowarden
COPY --from=builder /clowarden/target/release/clowarden /usr/local/bin
COPY --from=frontend-builder /web/build ./web/build
