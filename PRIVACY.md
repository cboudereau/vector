# Privacy Policy

Sol is a data pipeline that processes observability data (logs, metrics, traces)
on infrastructure you control. Sol does not phone home, collect telemetry, or
communicate with any external service that you have not explicitly configured.

## Sensitive data

Sol is designed to transport **observability data only**. You must ensure that
the data flowing through Sol does not contain sensitive or regulated information,
including but not limited to:

- **Personal data** protected under GDPR, CCPA, or equivalent regulations
  (names, email addresses, IP addresses of end users, etc.)
- **Payment card data** (PAN, CVV, expiration dates) as defined by PCI-DSS
- **Authentication secrets** (passwords, tokens, API keys, private keys)
- **Health records** or other data covered by HIPAA or similar frameworks

If sensitive data may be present in your telemetry streams, you are responsible
for redacting or masking it before it reaches Sol, or by using Sol's built-in
transforms (e.g. `remap` with VRL) to strip it in-flight.

Sol provides no encryption-at-rest for its disk buffers. Protect the host
accordingly if buffered data could contain anything sensitive.

## Downloads

Sol release artifacts are hosted on GitHub. Download counts are tracked in
aggregate by GitHub; Sol itself does not collect any additional data.
