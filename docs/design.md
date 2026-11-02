# tasukeai — design notes (draft)

*tasukeai* (助け合い) is Japanese for "helping each other". tasukeai decides **which machine should take a piece of
work now**, among machines that also have other lives: a desktop someone plays games on, a laptop that leaves the
house, a mini PC that runs hot. It does not run the work. It answers the question, with reasons, to whatever runs
it: a workflow engine, a batch script, an LLM proxy.

Status: early. The decision core, signal collection and leases (HTTP and CLI) are built; see the README.

## Why

Small fleets of mixed machines (workstations, laptops, a few VMs with GPUs) have spare capacity most of the day, but
using it safely needs answers that general schedulers do not give out of the box:

- Is someone using this machine right now (input in the last minutes)?
- Is it on battery, too hot, away from home, asleep?
- How much GPU memory and RAM is *actually usable* now, not installed?
- Which machine is the strongest that is free, and do not give it to two jobs at once.

Existing tools cover parts of this. HTCondor schedules work onto idle desktops, but brings its own agents, queues
and execution. Kubernetes scheduler plugins read GPU metrics from Prometheus, but only for pods. LLM load balancers
route to healthy inference servers, but know nothing about the machine's owner. tasukeai sits in between:

- **Inputs it does not own**: metrics the machines already export to a Prometheus-compatible store (node_exporter,
  windows_exporter, GPU exporters, textfile collectors), plus optional quick HTTP checks at decision time.
- **No agent on the machines**, no queue, no execution. One binary, one config file.
- **Owner policies as rules**: "only on AC", "only after 10 minutes without input", "not above 80 °C".

## Concepts

| Concept | Meaning |
|---|---|
| machine | A computer that may take work. Static facts in config: accelerator (cuda, rocm, metal, cpu), total VRAM / RAM, priority (lower = stronger), labels, which jobs are installed there, slots per job. |
| signal | A value about a machine now, read by a PromQL expression or an HTTP check: `up`, `ram_usable_bytes`, `vram_free_bytes`, `gpu_busy_ratio`, `gpu_temp_celsius`, `thermal_state`, `on_ac`, `idle_seconds`, ... Expressions are config, so tasukeai hard-codes no metric names; presets ship for common exporters. |
| job kind | What a piece of work needs: accelerators, VRAM, RAM, policy overrides, the mode below. |
| policy | Rules a machine must pass to take work: per machine (owner rules) and per job (needs). Each failed rule becomes a human-readable reason. |
| lease | A short reservation tasukeai keeps after handing out a machine, so that the next decision (before metrics catch up, or while the job loads its model) does not hand it out again. Released by the caller or by timeout. |

## Modes

| Mode | For | Behaviour |
|---|---|---|
| `exclusive` | One heavy job that occupies a machine (training, a long render) | Machines that pass all rules, in priority order; the first gets a lease. Leased machines are not offered again. |
| `spread` | N copies of the same work in parallel (extraction workers, batch inference) | Fill slots in priority order: each machine has a number of slots for the job kind; returns up to N assignments, each with a lease. Asking again later rebalances to what is free then. |
| `rank` | Short requests, one at a time (LLM chat completions) | No lease. Returns the usable machines in order. Metrics may be up to a scrape interval old, so `rank` can add quick HTTP checks (e.g. "does the inference server answer within 2 s", "is the GPU busy right now") with a short cache. |

## Interfaces (planned)

HTTP (JSON):

- `GET /machines` — every machine's signals and what could not be collected.
- `GET /assess?job=<kind>` — usable machines in order and the reasons for the others (no lease).
- `POST /pick?job=<kind>` — `exclusive`: the chosen machine and its lease; 409 with the reasons when none is free.
- `POST /spread?job=<kind>&count=N` — `spread`: up to N leases (a machine once per slot); 409 when none.
- `GET /rank?job=<kind>` — `rank`: usable machines in order, no lease.
- `GET /leases`, `POST /leases/<id>/renew` (long jobs extend their lease), `DELETE /leases/<id>` (the job ended or did
  not start).
- Planned: `GET /metrics` — tasukeai's own state for Prometheus (decisions, leases, reasons).

CLI (for scripts and workflow engines): `tasukeai pick <kind> --server URL` prints the machine name and the lease id
(exit code 75 when none can take it, with the reasons on stderr), `tasukeai release <lease> --server URL`;
`tasukeai signals` and `tasukeai assess <kind>` decide locally without leases.

Integrations stay outside the core: a LiteLLM callback that filters deployments with `/rank`, a workflow-engine task
that calls `pick` and passes the machine on.

## Process or library

tasukeai comes in two forms built from one core:

| Form | What | Good for |
|---|---|---|
| **process** | The `tasukeai` binary: the HTTP API above, a CLI, its own `/metrics` | Several callers sharing one view and **one set of leases** (`exclusive`, `spread`): a workflow engine, scripts on different machines, an LLM proxy |
| **library** | The `tasukeai-core` crate (Rust), later bindings for other languages (Python first, for LLM proxies and workflow engines written in it) | Deciding inside the caller, without a network hop: `rank` in an LLM proxy callback, a single program that places its own workers |

- The core holds everything that decides: configuration, signal collection (PromQL and HTTP checks), rules,
  modes and reasons. The binary only adds the HTTP and CLI faces and a lease store shared by its callers.
- Leases live where decisions are made. A library instance keeps its own leases, which is enough for one caller.
  Independent callers that must not get the same machine should share a process (or, later, a shared lease store).
- The same configuration file works for both forms.

Planned layout: a Cargo workspace with `crates/tasukeai-core` (library) and `crates/tasukeai` (binary); language
bindings as separate crates/packages on top of the core.

## Freshness and failure

- A signal older than a limit (default 2 minutes) counts as unknown, and unknown fails the rules that need it: a
  machine that stopped reporting (off, asleep, away) is not offered.
- If the metrics store does not answer, `exclusive` and `spread` give nothing (fail closed); `rank` may fall back to
  its HTTP checks only.
- The work itself still checks what it needs when it starts (for example free VRAM after stopping other services) and
  gives the machine back with a retry code when it does not fit; the caller asks again and gets the next machine.

## Configuration (sketch)

```toml
[metrics]
url = "http://prometheus:9090"
stale_after = "2m"

[presets.node]   # PromQL per signal; {instance} is the machine's instance label
ram_usable_bytes = 'node_memory_MemAvailable_bytes{instance="{instance}"}'

[machines.desk]
priority = 1
accel = "cuda"
vram_gb = 12
signals = ["node", "nvidia"]
rules = ["idle_seconds >= 600", "gpu_temp_celsius < 80"]
jobs = { lora-train = 1, extract = 2 }   # slots

[jobs.lora-train]
mode = "exclusive"
accel = ["cuda"]
vram_gb = 11
lease = "30m"
```

## Open questions

- Rules: a tiny expression language, or fixed rule kinds with parameters.
- Leases across restarts: memory only, or a small state file.
- Whether `rank` should also learn which machine already has a model loaded (fewer reloads).
- Power and energy as a rule input (prefer the machine that uses less for the same work).
