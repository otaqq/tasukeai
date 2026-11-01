# tasukeai

*tasukeai* (助け合い) is Japanese for "helping each other".

tasukeai tells **which machine should take a piece of work now**, among machines that also have other lives: a
desktop someone plays games on, a laptop that leaves the house, a mini PC that runs hot. It reads the metrics the
machines already export (Prometheus-compatible) and applies the owners' rules ("only on AC", "only after 10 minutes
without input", "not above 80 °C"). It does not run the work; it answers, with reasons, to whatever runs it.

Status: early skeleton. The decision core (rules, modes `exclusive` / `spread` / `rank`, reasons) works and is tested;
collecting signals (PromQL, HTTP checks) and leases over HTTP come next. Design: [docs/design.md](docs/design.md).

Two forms from one core: the `tasukeai` binary (HTTP API and CLI, shared leases) and the `tasukeai-core` library.

```sh
cargo test --workspace
cargo run -p tasukeai -- --config examples/tasukeai.toml check
cargo run -p tasukeai -- --config examples/tasukeai.toml assess train
```

## Run with Docker

```sh
cp examples/tasukeai.toml tasukeai.toml    # describe your machines and jobs
docker compose -f docker/compose.yaml up -d
curl -s localhost:9660/health
```

`docker/compose.yaml` builds the image from `docker/Dockerfile` (a static binary on distroless), mounts
`tasukeai.toml` read-only (`TASUKEAI_CONFIG` points elsewhere), publishes `TASUKEAI_PORT` (9660), restarts unless
stopped and checks health with `tasukeai health`. Without Compose:
`docker build -f docker/Dockerfile -t tasukeai . && docker run -v $PWD/tasukeai.toml:/tasukeai.toml:ro -p 9660:9660 tasukeai`.

## License

MIT
