FROM docker.io/alpine:3.23 AS builder

WORKDIR /vector

COPY sol-*-unknown-linux-musl*.tar.gz ./
RUN tar -xvf vector-0*-"$(cat /etc/apk/arch)"-unknown-linux-musl*.tar.gz --strip-components=2

RUN mkdir -p /var/lib/sol

# distroless doesn't use static tags
# hadolint ignore=DL3007
FROM gcr.io/distroless/static:latest

# https://github.com/opencontainers/image-spec/blob/main/annotations.md
LABEL org.opencontainers.image.url="https://vector.dev"
LABEL org.opencontainers.image.source="https://github.com/vectordotdev/vector"
LABEL org.opencontainers.image.documentation="https://vector.dev/docs"

COPY --from=builder /vector/bin/* /usr/local/bin/
COPY --from=builder /vector/config/sol.yaml /etc/sol/sol.yaml
COPY --from=builder /var/lib/sol /var/lib/sol

# Smoke test
RUN ["vector", "--version"]

ENTRYPOINT ["/usr/local/bin/sol"]
