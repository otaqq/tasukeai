# tasukeai

*tasukeai* (助け合い) is Japanese for "helping each other".

tasukeai tells **which machine should take a piece of work now**, among machines that also have other lives: a
desktop someone plays games on, a laptop that leaves the house, a mini PC that runs hot. It reads the metrics the
machines already export (Prometheus-compatible) and applies the owners' rules ("only on AC", "only after 10 minutes
without input", "not above 80 °C"). It does not run the work; it answers, with reasons, to whatever runs it.

Status: early. The decision core (rules, modes `exclusive` / `spread` / `rank`, reasons) and signal collection
(PromQL, HTTP checks) work and are tested; leases over HTTP come next. Design: [docs/design.md](docs/design.md).

## Signals

Each machine gets named signals from three places (later ones win for the same name):

- **presets**: built-in ones for standard exporter metrics (`node`: node_exporter on Linux, `node_darwin`: on macOS,
  `windows`: windows_exporter) with `up`, `ram_usable_bytes`, `ram_total_bytes`, `cpu_busy_ratio`, ... (see
  [presets.toml](crates/tasukeai-core/src/presets.toml)), and your own under `[presets.<name>]` for other exporters;
- **queries**: PromQL of that machine only;
- **checks**: HTTP GETs at decision time: 1 when the URL answers 2xx in time, else 0; with `json`, the number at that
  JSON pointer.

`{instance}` in an expression becomes the machine's `instance` label (default: its name). A query with no result
leaves the signal unknown, and an unknown signal fails every rule on it, so a machine that stops reporting is not
offered; add a rule on `up` to say so explicitly. The process caches signals for `[metrics] cache_seconds` (10).

Two forms from one core: the `tasukeai` binary (HTTP API and CLI, shared leases) and the `tasukeai-core` library.

```sh
cargo test --workspace
cargo run -p tasukeai -- --config examples/tasukeai.toml check
cargo run -p tasukeai -- --config examples/tasukeai.toml signals        # what each machine reports now
cargo run -p tasukeai -- --config examples/tasukeai.toml assess train   # who can take it, and why not
```

HTTP: `GET /health`, `GET /machines` (signals and collection errors), `GET /assess?job=<kind>`.

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
