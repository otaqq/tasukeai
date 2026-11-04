# Security

## Reporting a vulnerability

Report it privately through GitHub: the repository's **Security** tab → **Report a vulnerability**. Do not open a
public issue for it. Fixes go into the latest release only.

## No authentication of its own

The HTTP API has **no authentication**. Who may reach it is decided by your network: a private subnet, a firewall, or
an Ingress restricted to networks you trust. Anyone who can reach port 9660 can:

- lease machines (`POST /pick`, `/spread`) and give back any lease, also other callers' (`DELETE /leases/<id>`);
- keep machines out by hand and end any hold (`POST /holds`, `DELETE /holds/<id>`);
- read every machine's signals (`GET /machines`: idle time, temperatures, free memory) and the endpoints of the
  machines it hands out.

So keep it off untrusted networks. The Helm chart has the Ingress disabled by default; enable it only for networks you
trust.

## What it fetches

tasukeai only queries the URLs in its configuration file: the metrics store (`[metrics] url`) and the HTTP checks of
each machine. Callers never pass a URL, so the API cannot be used to make it fetch anything else. Each query has a
timeout, and signals are collected in the background, so a machine that does not answer never holds up a request.

A signal it cannot read, or one older than `stale_after_seconds`, counts as unknown, and an unknown signal fails its
rule: when in doubt, a machine is left out rather than handed out.

## State file

With `[state] path`, leases and holds are written to that JSON file after every change. It holds machine names, job
names, endpoints and the notes of holds, no credentials. Only tasukeai needs to read or write it.

## Running it

- **Container image**: distroless (no shell, no package manager), as the non-root user 65532.
- **Helm chart**: `runAsNonRoot`, read-only root filesystem, all capabilities dropped, no privilege escalation,
  the `RuntimeDefault` seccomp profile.
- **systemd** (`dist/systemd/tasukeai.service`): `DynamicUser`, no capabilities, `ProtectSystem=strict` and a sandbox,
  `MemoryMax=128M`; see [Deployment](deployment.md).

## Scans

The `security` workflow scans the image and the repository (vulnerabilities, misconfigurations, secrets) with Trivy
and reports the findings to GitHub code scanning (the Security tab), not to the public job log. It also checks the
dependencies in `Cargo.lock` with `cargo audit`, which fails the job when an advisory matches.
