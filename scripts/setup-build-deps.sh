#!/usr/bin/env bash
# Install system dependencies and prepare the build environment for Vector.
#
# Usage:
#   sudo ./scripts/setup-build-deps.sh    # install system packages
#   ./scripts/setup-build-deps.sh --clean  # clean stale build artifacts
set -euo pipefail

if [[ "${1:-}" == "--clean" ]]; then
    echo "Cleaning stale sasl2-sys build cache..."
    rm -rf target/debug/build/sasl2-sys-*
    echo "Done. Re-run: cargo test -p vector --lib"
    exit 0
fi

# Require root for apt-get
if [[ $EUID -ne 0 ]]; then
    echo "Run with sudo for package installation: sudo $0"
    echo "Or use --clean to fix build cache issues without sudo."
    exit 1
fi

apt-get update -qq
apt-get install -y --no-install-recommends \
    build-essential \
    cmake \
    pkg-config \
    libssl-dev \
    libsasl2-dev \
    autoconf \
    automake \
    libtool \
    protobuf-compiler \
    libprotobuf-dev

echo ""
echo "Build dependencies installed."
echo ""
echo "If sasl2-sys still fails to build, run:"
echo "  ./scripts/setup-build-deps.sh --clean"
echo "  cargo test -p vector --lib"
