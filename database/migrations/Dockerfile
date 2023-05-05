# Build tern
FROM golang:1.20.4-alpine3.17 AS tern
RUN apk --no-cache add git
RUN go install github.com/jackc/tern@latest

# Build final image
FROM alpine:3.17.3
RUN addgroup -S gitvote && adduser -S gitvote -G gitvote
USER gitvote
WORKDIR /home/gitvote
COPY --from=tern /go/bin/tern /usr/local/bin
COPY database/migrations .
