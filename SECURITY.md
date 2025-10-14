# Security Policy

## Supported Versions

We take security seriously and provide security updates for the latest version of busd.

## Reporting a Vulnerability

**Please do not report security vulnerabilities through public GitHub issues.**

If you discover a security vulnerability in busd, please report it privately by emailing
**zeenixATgmail**.

Please include the following information in your report:

- A clear description of the vulnerability
- Steps to reproduce the issue
- Potential impact and attack scenarios
- Any suggested fixes or mitigations
- Your contact information for follow-up questions

### What constitutes a security vulnerability?

For busd as a D-Bus broker, security vulnerabilities may include but are not limited to:

- **Authentication bypass**: Circumventing client authentication or connection validation
- **Policy enforcement failures**: Allowing clients to send/receive messages that violate
  configured security policies
- **Privilege escalation**: Enabling unprivileged clients to perform privileged operations or
  impersonate system services
- **Memory safety violations**: Use-after-free, buffer overflows, or other memory corruption
  issues in the broker process
- **Message routing exploits**: Ability to redirect, intercept, or modify messages between
  clients
- **Denial of service**: Maliciously crafted messages or connection patterns causing broker
  crashes or resource exhaustion
- **Information disclosure**: Leaking sensitive information between isolated clients or exposing
  broker internals
- **Name ownership hijacking**: Unauthorized takeover of well-known service names
- **Monitor bypass**: Circumventing monitoring restrictions to eavesdrop on bus traffic
- **Configuration bypass**: Overriding or bypassing XML configuration policies

## Response Timeline

We are committed to responding to security reports promptly:

- **Acknowledgment**: We will acknowledge receipt of your vulnerability report within
  **48 hours**
- **Initial assessment**: We will provide an initial assessment of the report within
  **5 business days**
- **Regular updates**: We will provide progress updates at least every **7 days** until
  resolution
- **Resolution**: We aim to provide a fix or mitigation within **30 days** for critical
  vulnerabilities

Response times may vary based on the complexity of the issue and availability of maintainers.

## Disclosure Policy

We follow a coordinated disclosure process:

1. **Private disclosure**: We will work with you to understand and validate the vulnerability
2. **Fix development**: We will develop and test a fix in a private repository if necessary
3. **Coordinated release**: We will coordinate the public disclosure with the release of a fix
4. **Public disclosure**: After a fix is available, we will publish a security advisory

We request that you:
- Give us reasonable time to address the vulnerability before making it public
- Avoid accessing or modifying data beyond what is necessary to demonstrate the vulnerability
- Act in good faith and avoid privacy violations or destructive behavior

## Security Advisories

Published security advisories will be available through:

- GitHub Security Advisories on the
  [busd repository](https://github.com/z-galaxy/busd/security/advisories)
- [RustSec Advisory Database](https://rustsec.org/)
- Release notes and changelog entries

## Recognition

We appreciate the security research community's efforts to improve the security of busd. With
your permission, we will acknowledge your contribution in:

- Security advisories
- Release notes
- Project documentation

If you prefer to remain anonymous, please let us know in your report.

## Additional Resources

- [Contributing Guidelines](CONTRIBUTING.md)
- [Code of Conduct](https://www.rust-lang.org/policies/code-of-conduct)
- [D-Bus Specification](https://dbus.freedesktop.org/doc/dbus-specification.html)

Thank you for helping to keep busd and the D-Bus ecosystem secure!
