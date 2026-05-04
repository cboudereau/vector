# Support

If you need help with Sol, here are some options:

- **Bug reports**: [GitHub Issues](https://github.com/cboudereau/sol/issues)
- **Feature requests**: [GitHub Issues](https://github.com/cboudereau/sol/issues)
- **Questions**: [GitHub Discussions](https://github.com/cboudereau/sol/discussions)

## How to ask a question

### Before asking

Check the existing issues and discussions first — someone may have already asked the same question.

### Provide details

- Sol version (`sol --version`)
- Operating system and architecture
- Configuration file (redact sensitive values)
- Relevant log output (`SOL_LOG=sol=debug sol -vvv`)
- How Sol is deployed (standalone, Kubernetes, Docker, etc.)

### Formatting

Use fenced code blocks for configuration and log snippets:

```yaml
sources:
  demo:
    type: demo_logs
    format: json
```
