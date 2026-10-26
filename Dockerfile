# Bump the Rust and Alpine versions together; deno comes from Alpine's packages.
FROM rust:1.99-alpine3.24 AS builder
# `dev` for a faster, unoptimized debug build.
ARG PROFILE=release
WORKDIR /src
RUN apk add --no-cache build-base
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY api ./api
RUN cargo build --profile "$PROFILE" --locked -p togematch-api && \
    cp "target/$([ "$PROFILE" = dev ] && echo debug || echo "$PROFILE")/togematch-api" /togematch-api

FROM alpine:3.24
# Deno is packaged here because its releases are glibc-only; yt-dlp fetches its musl build.
RUN apk add --no-cache ca-certificates deno && \
    adduser -u 1000 -D togematch

WORKDIR /app
COPY --from=builder /togematch-api /app/togematch-api
COPY LICENSE /app/LICENSE
RUN mkdir -p /app/data && chown -R togematch:togematch /app

USER togematch
EXPOSE 7094
VOLUME ["/app/data"]
ENTRYPOINT ["/app/togematch-api"]
CMD ["--db", "/app/data/togematch.db"]
