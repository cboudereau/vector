# Contributing

Thank you for your interest in contributing to Sol!

## Getting started

1. Fork the repository and create a feature branch.
2. Make your changes.
3. Run tests: `cargo test -p sol --lib`
4. Submit a pull request.

## Pull requests

- Keep PRs focused and small when possible.
- Follow [conventional commits](https://www.conventionalcommits.org) for the PR title.
- Add tests for new functionality, especially integration tests for external services.
- Ensure `cargo clippy` and `cargo fmt --check` pass.

## Running tests

```bash
# Unit tests
cargo test -p sol --lib

# Integration tests
cargo test -p sol --test integration
```

## License

By contributing to Sol, you agree that your contributions will be licensed
under the [Mozilla Public License 2.0](LICENSE).
