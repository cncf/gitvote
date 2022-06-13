# Build tern
FROM golang:1.17-alpine3.15 AS tern
RUN apk --no-cache add git
RUN go get -u github.com/jackc/tern

# Build final image
FROM alpine:3.15
RUN addgroup -S gitvote && adduser -S gitvote -G gitvote
USER gitvote
WORKDIR /home/gitvote
COPY --from=tern /go/bin/tern /usr/local/bin
COPY database/migrations .
