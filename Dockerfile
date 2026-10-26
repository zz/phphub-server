# PHPHub runtime image.
#
# libphp (ZTS) and the extensions come from the FrankenPHP image that
# docker-build-phalcon publishes; Phalcon is not compiled here. That image is
# only a build stage. The runtime is plain Alpine: no Caddy, no frankenphp.
ARG PHP_IMAGE=zhuzhu/php:8.5-phalcon-5.23-frankenphp
ARG ALPINE_VERSION=3.24

FROM ${PHP_IMAGE} AS php

FROM php AS build
RUN apk add --no-cache cargo rust build-base
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
COPY csrc csrc
COPY src src
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/phphub /phphub
# Shared libraries that libphp, the extensions and phphub need at run time.
RUN scanelf --needed --nobanner --format '%n#p' --recursive \
        /usr/local/lib/libphp.so /usr/local/lib/php/extensions /phphub \
    | tr ',' '\n' | sort -u \
    | awk 'system("[ -e /usr/local/lib/" $1 " ]") == 0 { next } { print "so:" $1 }' \
    > /rundeps

FROM alpine:${ALPINE_VERSION}
COPY --from=build /rundeps /tmp/rundeps
RUN apk add --no-cache $(cat /tmp/rundeps) && rm /tmp/rundeps
COPY --from=php /usr/local/lib/libphp.so /usr/local/lib/libphp.so
COPY --from=php /usr/local/lib/php/extensions /usr/local/lib/php/extensions
COPY --from=php /usr/local/etc/php /usr/local/etc/php
COPY --from=php /etc/localtime /etc/localtime
COPY --from=php /etc/timezone /etc/timezone
COPY --from=build /phphub /usr/local/bin/phphub

ENV SERVER_ROOT=/var/www/html \
    TZ=America/Los_Angeles

LABEL org.opencontainers.image.title="PHPHub" \
      org.opencontainers.image.description="HTTP/1.1 server with embedded ZTS PHP workers on port 80"

RUN mkdir -p /var/www/html
WORKDIR /var/www/html
EXPOSE 80 9100
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=10s --timeout=3s --start-period=20s --retries=3 \
    CMD ["/usr/local/bin/phphub", "health"]
ENTRYPOINT ["/usr/local/bin/phphub"]
