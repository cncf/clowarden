# Build tern
FROM golang:1.24.5-alpine3.22 AS tern
RUN apk --no-cache add git
RUN go install github.com/jackc/tern@latest

# Build final image
FROM alpine:3.22.0
RUN addgroup -S clowarden && adduser -S clowarden -G clowarden
USER clowarden
WORKDIR /home/clowarden
COPY --from=tern /go/bin/tern /usr/local/bin
COPY database/migrations .
