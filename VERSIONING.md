# Versioning

Sol adheres to [Semantic Versioning 2.0](https://semver.org/).

Versions follow the `MAJOR.MINOR.PATCH` format (e.g. `1.2.3`).

## Public API

The following are considered part of Sol's public API and covered by semver guarantees:

- Configuration file format (YAML, TOML, JSON)
- CLI flags and subcommands
- Environment variables (`SOL_*`)
- Source, transform, and sink component interfaces

The following are **not** covered:

- Internal metrics names and labels (may change between minor versions)
- Log output format and messages
- Rust crate APIs (Sol is not published as a library)
