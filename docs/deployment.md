# Deployment

tasukeai is one static binary and one configuration file. It needs network access to the metrics store and to the
URLs of its HTTP checks, and nothing on the machines it decides about.

**Run one instance.** Leases are handed out under a lock inside the process. Two instances would each hand out the
same slot, so do not scale it out; if it restarts, `[state] path` brings the leases and holds back.

## Binary

Each [release](https://github.com/otaqq/tasukeai/releases) has static Linux binaries (musl, no dependencies) for
x86_64 and aarch64, with `SHA256SUMS`:

```sh
v=v0.4.1; t=x86_64-unknown-linux-musl          # or aarch64-unknown-linux-musl
curl -fsSLO https://github.com/otaqq/tasukeai/releases/download/$v/tasukeai-$v-$t.tar.gz
curl -fsSL https://github.com/otaqq/tasukeai/releases/download/$v/SHA256SUMS | sha256sum -c --ignore-missing
tar -xzf tasukeai-$v-$t.tar.gz && ./tasukeai-$v-$t/tasukeai --version
```

## Docker

```sh
cp examples/tasukeai.toml tasukeai.toml    # describe your machines and jobs
docker compose -f docker/compose.yaml up -d
curl -s localhost:9660/health
```

`docker/compose.yaml` runs the published image `ghcr.io/otaqq/tasukeai` (linux/amd64 and linux/arm64; a static
binary on distroless, as user 65532) at `TASUKEAI_VERSION` (default `latest`; `docker compose pull` updates it), mounts
`tasukeai.toml` read-only (`TASUKEAI_CONFIG` points elsewhere), publishes `TASUKEAI_PORT` (9660), restarts unless
stopped and checks health with `tasukeai health`. `up -d --build` builds the image from source (`docker/Dockerfile`)
instead. Without Compose:
`docker run -d -v $PWD/tasukeai.toml:/tasukeai.toml:ro -p 9660:9660 ghcr.io/otaqq/tasukeai`.

The image reads `/tasukeai.toml` and runs `serve`. With `[state] path = "/data/state.json"`, mount a writable
volume at `/data` owned by uid 65532.

## Footprint

tasukeai is small enough for edge devices. Measured on one instance in Kubernetes under light load, over 7 days
(cAdvisor metrics, x86_64):

| | now | peak (7 days) |
|---|---|---|
| RSS | 0.45 MiB | 0.52 MiB |
| Working set | 0.75 MiB | 1.24 MiB |
| CPU | about 0.012 cores | |

The binary is static (musl), so it needs no libc or other files on the host. In v0.4.1 it is 6.6 MiB on x86_64
and 6.5 MiB on aarch64 (2.6 MiB as `.tar.gz`). Memory grows with the number of machines, jobs, leases and holds; the
numbers above are for a small setup, not a limit. Requests of `32Mi` memory and `10m` CPU, with a `128Mi` memory
limit, leave plenty of headroom.
