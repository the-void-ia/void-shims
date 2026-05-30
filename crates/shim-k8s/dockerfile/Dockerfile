# Container image bundling void-box + the entrypoint dispatcher.
# Build with: ./crates/shim-k8s/dockerfile/build.sh /path/to/voidbox
#
# Base must have glibc >= 2.39 to match void-box 0.2.x release builds on
# Fedora 44+ hosts. trixie-slim has glibc 2.40.
FROM debian:trixie-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY voidbox /usr/local/bin/voidbox
COPY voidbox-entrypoint.sh /usr/local/bin/voidbox-entrypoint.sh
RUN chmod +x /usr/local/bin/voidbox /usr/local/bin/voidbox-entrypoint.sh

ENTRYPOINT ["/usr/local/bin/voidbox-entrypoint.sh"]
