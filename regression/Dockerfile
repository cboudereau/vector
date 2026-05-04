#
# VECTOR BUILDER
#
FROM docker.io/timberio/vector-dev:latest AS builder
WORKDIR /vector
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/vector/target \
    cargo build --bin sol --release && \
    cp target/release/sol .

#
# TARGET
#
FROM docker.io/debian:trixie-slim@sha256:c85a2732e97694ea77237c61304b3bb410e0e961dd6ee945997a06c788c545bb
RUN apt-get update && apt-get dist-upgrade -y && apt-get -y --no-install-recommends install zlib1g ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /vector/vector /usr/bin/sol
RUN mkdir --parents --mode=0777 /var/lib/sol

# Smoke test
RUN ["/usr/bin/sol", "--version"]

ENTRYPOINT ["/usr/bin/sol"]
