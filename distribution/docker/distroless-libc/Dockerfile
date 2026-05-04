FROM docker.io/debian:trixie-slim AS builder

WORKDIR /vector

COPY sol_*.deb ./
RUN dpkg -i vector_*_"$(dpkg --print-architecture)".deb

RUN mkdir -p /var/lib/sol

# distroless doesn't use static tags
# hadolint ignore=DL3007
FROM gcr.io/distroless/cc-debian12:latest

# https://github.com/opencontainers/image-spec/blob/main/annotations.md
LABEL org.opencontainers.image.url="https://vector.dev"
LABEL org.opencontainers.image.source="https://github.com/vectordotdev/vector"
LABEL org.opencontainers.image.documentation="https://vector.dev/docs"

COPY --from=builder /usr/bin/sol /usr/bin/sol
COPY --from=builder /usr/share/doc/vector /usr/share/doc/vector
COPY --from=builder /usr/share/vector /usr/share/vector
COPY --from=builder /etc/sol /etc/sol
COPY --from=builder /var/lib/sol /var/lib/sol

# Smoke test
RUN ["vector", "--version"]

ENTRYPOINT ["/usr/bin/sol"]
