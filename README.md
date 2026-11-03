# tasukeai

*tasukeai* (助け合い) is Japanese for "helping each other".

tasukeai tells **which machine should take a piece of work now**, among machines that also have other lives: a
desktop someone plays games on, a laptop that leaves the house, a mini PC that runs hot. It reads the metrics the
machines already export (Prometheus-compatible) and applies the owners' rules ("only on AC", "only after 10 minutes
without input", "not above 80 °C"). It does not run the work; it answers, with reasons, to whatever runs it.

Status: early. The decision core (rules, modes `exclusive` / `spread` / `rank`, reasons), signal collection
(PromQL, HTTP checks) and leases work and are tested. Design: [docs/design.md](docs/design.md).

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

## HTTP

| Call | Answer |
|---|---|
| `GET /health` | `ok` |
| `GET /machines` | every machine's signals and what could not be collected |
| `GET /assess?job=<kind>` | usable machines in order, reasons for the others (no lease); takes rules of the caller like `/rank` |
| `POST /pick?job=<kind>` | `exclusive`: `{machine, lease, assessment}`; 409 with the reasons when none is free |
| `POST /spread?job=<kind>&count=N` | `spread`: up to N leases, a machine once per free slot; 409 when none |
| `GET /rank?job=<kind>[&min.<signal>=x][&max.<signal>=x]` | `rank`: `{machines, endpoints, assessment}`, no lease |
| `GET /leases` | the leases that have not run out |
| `POST /leases/<id>/renew` | extend a lease by its job's `lease_seconds` |
| `DELETE /leases/<id>` | give the machine back |
| `POST /holds` | keep a machine out by hand: `{"machine", "job"?, "minutes"?, "note"?}`; 201 with the hold |
| `GET /holds` | the holds that have not run out |
| `DELETE /holds/<id>` | end a hold |

A lease of an `exclusive` job holds the whole machine; a `spread` lease holds one slot of its own job. Decisions and
leases are taken under one lock, so two callers never get the same slot. Leases and holds live in memory; with
`[state] path = "/data/state.json"` they are also saved to that file after every change and loaded on start, so a
restart keeps them (what ran out meanwhile is dropped).

**Endpoints.** A machine may name addresses of its services (`endpoints = { llm = "http://desk:11434/v1" }`) and a
job the one it uses (`endpoint = "llm"`). Each lease then carries `endpoint` (the URL), and `/rank` answers
`endpoints` in the order of `machines`, so callers need no address table of their own. A job naming an endpoint
that a machine with a slot for it lacks is a configuration error.

**Rules for some jobs.** A rule may name the jobs it is for: `{ signal = "idle_seconds", min = 600, jobs = ["extract",
"train"] }` keeps long batch work off a machine someone is using, while short chat requests still go there.

**Rules of the caller.** `/rank` and `/assess` take extra rules for one decision as query parameters,
`min.<signal>=<number>` and `max.<signal>=<number>` (e.g. `min.vram_free_gb=10` for a model that needs 10 GB). Like
configured rules, a missing or stale signal fails them; the reason ends in `(asked by the caller)`.

**Endpoints of left-out machines.** Rejected machines carry their `endpoint` too, so a caller that routes by address
(an LLM proxy with a list of servers) can tell "known but not usable now" from "not known to tasukeai".

**Holds.** A person keeps a machine out by hand: for every job or one job, until a time or until ended. Held
machines are not offered by `pick`, `spread` or `rank`; the reason reads `held by hand[ for <job>][ until <time>][:
<note>]`. Leases already given stay until they run out or are released.

From scripts: `tasukeai pick <kind> --server http://host:9660` prints `machine lease-id`, or the reasons and exits 75
when none is free; `tasukeai release <lease-id> --server ...` gives it back. `tasukeai hold <machine> [--job J]
[--minutes M] [--note N] --server ...` prints a hold id, `tasukeai unhold <id>` ends it, `tasukeai holds` lists them.

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

## Development

```sh
cargo fmt --all            # rustfmt.toml: max_width 120
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

CI runs the same three checks on every push to `main` and every pull request (`.github/workflows/ci.yml`), builds
the image and runs it once, and on GitHub repeats the tests and the image on native arm64.
`.github/workflows/security.yml` scans on pushes, pull requests and weekly: Trivy on the image and the repository
(vulnerabilities, misconfigurations, secrets; HIGH and CRITICAL with a fix) and `cargo audit` on `Cargo.lock`. On
GitHub the Trivy results go to code scanning; on other CI services one issue titled "Security scan: vulnerabilities
found" holds the report and is closed when a scan comes back clean.

## The name

*tasukeai* (助け合い) means "helping each other". *Tasuke* (助け) is "help"; *ai* (合い, from *au*, "to come
together") makes it mutual, something done together. The pun is intended: the work it was made for is AI work
(LLM chat, extraction, training) on whatever machines can lend a hand right now, and read as English the name
ends in "AI".

## License

MIT
