# Build clowarden
FROM rust:1-alpine3.22 AS builder
RUN apk --no-cache add musl-dev perl make
WORKDIR /clowarden
COPY Cargo.* ./
COPY clowarden-cli/Cargo.* clowarden-cli
COPY clowarden-core clowarden-core
COPY clowarden-server clowarden-server
WORKDIR /clowarden/clowarden-server
RUN cargo build --release

# Build frontend
FROM node:24-alpine3.22 AS frontend-builder
RUN apk --no-cache add git
WORKDIR /web
COPY web .
ENV NODE_OPTIONS=--max_old_space_size=4096
RUN yarn config set network-timeout 1000000
RUN yarn install --network-concurrency 1
RUN yarn build

# Final stage
FROM alpine:3.22.2
RUN apk --no-cache add ca-certificates && addgroup -S clowarden && adduser -S clowarden -G clowarden
USER clowarden
WORKDIR /home/clowarden
COPY --from=builder /clowarden/target/release/clowarden-server /usr/local/bin
COPY --from=frontend-builder /web/build ./web/build
