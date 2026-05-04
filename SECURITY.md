# Security Policy

## Reporting a vulnerability

If you discover a security vulnerability in Sol, please report it responsibly
by opening a [GitHub issue](https://github.com/cboudereau/sol/issues/new?labels=security).

We take all disclosures seriously and will respond as quickly as possible to
verify and address the issue.

When reporting, please include:

- A description of the vulnerability and its potential impact
- Steps to reproduce or a proof of concept
- Any tools or versions used

## Security practices

- **Rust** — Sol is written in Rust, which eliminates many classes of memory
  safety and concurrency vulnerabilities at compile time.
- **No unsafe code** — unsafe code is not allowed except where required for FFI.
- **Dependency auditing** — dependencies are checked with `cargo deny` against
  the [RustSec Advisory Database](https://rustsec.org/).
- **Non-root by default** — Sol is designed to run under non-root privileges.
- **TLS** — all network sinks and sources support TLS for data in transit.
