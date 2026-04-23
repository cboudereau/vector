# Development Environment Setup

This document describes the WSL2 and Rust setup used to build and develop Vector.

## Host

| Component | Version |
|---|---|
| OS | Windows + WSL2 |
| WSL kernel | 6.6.87.2-microsoft-standard-WSL2 |
| Distro | Ubuntu 24.04.3 LTS (Noble Numbat) |
| Architecture | x86_64 |
| RAM | 12 GB (WSL2 allocated) |
| CPU cores | 12 |
| Disk | ~1 TB ext4 (WSL2 virtual disk) |

### WSL configuration

`/etc/wsl.conf`:

```ini
[boot]
systemd=true

[user]
default=clem
```

## Rust toolchain

The project pins its toolchain via `rust-toolchain.toml`:

```toml
[toolchain]
channel = "1.92"
profile = "default"
```

| Component | Version |
|---|---|
| rustc | 1.92.0 (ded5c06cf 2025-12-08) |
| cargo | 1.92.0 (344c4567c 2025-10-21) |
| Target | x86_64-unknown-linux-gnu |
| clippy | installed |
| rustfmt | installed |

## System dependencies

Install them on Ubuntu 24.04 with:

```bash
sudo apt-get update && sudo apt-get install -y \
    autoconf \
    automake \
    build-essential \
    cmake \
    git \
    libclang-dev \
    libsasl2-dev \
    libssl-dev \
    libtool \
    pkg-config \
    protobuf-compiler
```

> **Why autoconf / automake / libtool?** The `rdkafka` crate enables
> `gssapi-vendored` on Linux GNU targets, which pulls in `sasl2-sys` with the
> `vendored` feature. That feature builds `libsasl2` from source using GNU
> Autotools. Without these packages the build fails with
> `configure failed: No such file or directory`.

Installed versions on this machine:

| Package | Version |
|---|---|
| autoconf | 2.71 |
| automake | 1.16.5 |
| libtool | 2.4.7 |
| gcc / g++ | 13.3.0 |
| cmake | 3.28.3 |
| protoc (protobuf-compiler) | 3.20.2 |
| libssl-dev | 3.0.13 |
| libsasl2-dev | 2.1.28 |
| libclang-dev | 18.0 |
| pkg-config | 1.8.1 |
| make | 4.3 |
| perl | 5.38.2 |

## Repository

```
git@github.com-cboudereau/vector.git   (fork)
```

Workspace root: `/home/clem/gh/vector`

## Build commands

```bash
# Type-check (fastest feedback loop, ~30-60s incremental)
cargo check

# Run all lib unit tests (~45s after build)
cargo test --lib

# Run a specific test
cargo test --lib -- "sinks::elasticsearch::tests::encode_valid"

# Clippy lint
cargo clippy --workspace --all-targets -- -D warnings

# Format check
cargo fmt --all -- --check

# Test vector
cargo test -p vector --all-features

# To log output:
RUSTFLAGS="--cfg tokio_unstable" cargo test -p vector --all-features 2>&1 | tee tests.log
```

### Cargo aliases

Defined in `.cargo/config.toml`:

```bash
# Run the vdev CLI helper
cargo vdev <args>
```

### Notable build flags

From `.cargo/config.toml`:

- **jemalloc**: `JEMALLOC_SYS_WITH_LG_PAGE=16` (large page support for CentOS/RHEL compatibility)
- **Linux GNU target**: `-C link-args=-rdynamic` (export symbols for plugin support)
- **Clippy denies**: `print_stdout`, `print_stderr`, `dbg_macro`

## Performance tips for WSL2

- **Keep the repo on the Linux filesystem** (`/home/...`), not on `/mnt/c/...`. Cross-filesystem I/O through the 9P mount is 10-50x slower.
- **Increase WSL memory** if builds OOM. Create or edit `%USERPROFILE%\.wslconfig`:

  ```ini
  [wsl2]
  memory=16GB
  processors=12
  ```

  Then restart WSL: `wsl --shutdown` from PowerShell.

- **Use `cargo check`** for fast iteration. A full `cargo test --lib` build from scratch takes ~5 minutes; incremental `cargo check` takes ~30s.
- **Avoid running Windows antivirus on the WSL2 vhdx**. Add the vhdx path to Windows Defender exclusions.
