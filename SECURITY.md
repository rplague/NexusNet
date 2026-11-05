# Security policy

## Supported versions

Security fixes target the current `0.6.x` line. Older releases may not receive
patches; upgrade to the latest tag before reporting unless the issue prevents
that.

## Disclosure policy

NexusNet prefers **immediate public disclosure**. If you find a security issue,
you are encouraged to open a public issue right away so every operator can learn
about it at the same time. There is no private embargo period and maintainers
will not ask you to delay publication.

If you prefer not to use the public tracker, you can email
**wcx0717@outlook.com** instead. Public reports are still the default and the
fastest path.

This policy deliberately favors fast awareness over a private patch window. In
practice that means:

- Watch the repository for security issues and upgrade promptly.
- Assume that a public report may be exploitable before a fix is released.
- If you need time to deploy a mitigation, say so in the report; maintainers can
  document workarounds while a fix is prepared, but publication is not held back.

## What to include

A useful report contains:

- NexusNet version or tag (for example `0.6.2-11020`).
- Operating system and how the node is deployed (systemd, Windows service, embedded).
- The relevant configuration, with keys, keypair paths and private addresses redacted.
- Logs from `journalctl -u nexusnet` or the configured log file.
- A minimal reproduction or a clear description of the attack path.
- The impact you believe the issue has.

Never include `keypair.bin`, `keypair.pq.bin`, authority private keys or other
secret material in a report.

## Scope

In scope:

- The node, its libp2p configuration and transport behavior.
- The sidecar protocol, service registry, authorization and post-quantum layers.
- The Debian and Windows packaging and service definitions.

Out of scope:

- Vulnerabilities in third-party dependencies that are already public and have an
  upstream fix; report those upstream and mention the affected version here.
- Misconfiguration that is documented as unsupported (for example exposing a
  backend port directly to the internet).
- Denial of service that requires control of a configured bootstrap or relay node
  unless it crosses a documented trust boundary.

## Response

Maintainers will confirm reports, ask for missing details when needed, and
publish fixes and advisories in the normal release stream. There is no bug bounty
program.
