//! tasukeai: tell which machine should take a piece of work now (process form; see docs/design.md).
//!
//! Collects signals (PromQL, HTTP checks; cached for [metrics] cache_seconds), decides, and keeps one lease book for
//! all callers: POST /pick and /spread lease, GET /rank does not; POST /holds keeps a machine out by hand.

use anyhow::Context;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tasukeai_core::{assess, assess_with, collect, rank_with, Collected, Config, Leases, Rule};

/// Rules of the caller for one decision, from the query: min.<signal>=<number>, max.<signal>=<number> (e.g. the free
/// VRAM a model needs: min.vram_free_gb=10). Like configured rules, a missing or stale signal fails them.
fn caller_rules(params: &HashMap<String, String>) -> Result<Vec<Rule>, String> {
    let mut out = Vec::new();
    for (k, v) in params {
        let (signal, is_min) = match (k.strip_prefix("min."), k.strip_prefix("max.")) {
            (Some(s), _) => (s, true),
            (_, Some(s)) => (s, false),
            _ => continue,
        };
        let x: f64 = v.parse().map_err(|_| format!("{k}={v}: not a number"))?;
        out.push(Rule {
            signal: signal.to_string(),
            min: is_min.then_some(x),
            max: (!is_min).then_some(x),
            because: Some("asked by the caller".into()),
            jobs: None,
        });
    }
    out.sort_by(|a, b| a.signal.cmp(&b.signal));
    Ok(out)
}

#[derive(Deserialize)]
struct HoldRequest {
    machine: String,
    #[serde(default)]
    job: Option<String>,
    #[serde(default)]
    minutes: Option<u64>,
    #[serde(default)]
    note: String,
}

/// Exit code when no machine can take the job (EX_TEMPFAIL: try again later)
const NONE_FREE: i32 = 75;

#[derive(Parser)]
#[command(
    name = "tasukeai",
    version,
    about = "Tell which machine should take a piece of work now"
)]
struct Cli {
    /// Configuration file (TOML)
    #[arg(long, short, default_value = "tasukeai.toml", global = true)]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check the configuration and print the machines and jobs
    Check,
    /// Serve the HTTP API
    Serve {
        #[arg(long, default_value = "0.0.0.0:9660")]
        listen: String,
    },
    /// Which machines can take a job now, with the reasons for the others (no lease)
    Assess { job: String },
    /// The signals of every machine now, and what could not be collected
    Signals,
    /// Ask a running server for a machine (exclusive job); prints "machine lease-id", exit 75 when none is free
    Pick {
        job: String,
        #[arg(long, default_value = "http://127.0.0.1:9660")]
        server: String,
    },
    /// Give a lease back to a running server
    Release {
        lease: String,
        #[arg(long, default_value = "http://127.0.0.1:9660")]
        server: String,
    },
    /// Keep a machine out by hand on a running server (every job, or --job); prints the hold id
    Hold {
        machine: String,
        /// Only this job (default: every job)
        #[arg(long)]
        job: Option<String>,
        /// End by itself after this many minutes (default: until unhold)
        #[arg(long)]
        minutes: Option<u64>,
        /// Why, shown in the reasons ("playing a game")
        #[arg(long, default_value = "")]
        note: String,
        #[arg(long, default_value = "http://127.0.0.1:9660")]
        server: String,
    },
    /// End a hold on a running server
    Unhold {
        hold: String,
        #[arg(long, default_value = "http://127.0.0.1:9660")]
        server: String,
    },
    /// List the holds of a running server
    Holds {
        #[arg(long, default_value = "http://127.0.0.1:9660")]
        server: String,
    },
    /// Exit 0 when a running server answers /health (for container health checks; needs no configuration)
    Health {
        #[arg(long, default_value = "127.0.0.1:9660")]
        addr: String,
    },
}

/// GET /health over a plain TCP connection: no HTTP client needed for one line.
fn health_check(addr: &str) -> anyhow::Result<()> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect_timeout(&addr.parse()?, std::time::Duration::from_secs(3))?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
    s.write_all(format!("GET /health HTTP/1.0\r\nHost: {addr}\r\n\r\n").as_bytes())?;
    let mut buf = [0u8; 64];
    let n = s.read(&mut buf)?;
    let head = String::from_utf8_lossy(&buf[..n]);
    anyhow::ensure!(
        head.starts_with("HTTP/1.0 200") || head.starts_with("HTTP/1.1 200"),
        "unhealthy: {}",
        head.lines().next().unwrap_or("")
    );
    Ok(())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct App {
    cfg: Config,
    cache: Mutex<Option<(Instant, Collected)>>,
    leases: Mutex<Leases>,
    /// [state] path: the leases and holds are saved there after every change and loaded on start.
    state: Option<PathBuf>,
}

impl App {
    fn new(cfg: Config) -> anyhow::Result<Arc<Self>> {
        let state = cfg.state.path.as_ref().map(PathBuf::from);
        let book = match &state {
            Some(p) => Leases::load(p, now()).with_context(|| format!("loading the state file {}", p.display()))?,
            None => Leases::new(),
        };
        Ok(Arc::new(App {
            cfg,
            cache: Mutex::new(None),
            leases: Mutex::new(book),
            state,
        }))
    }

    /// Save the book after a change (under its lock, so saves never interleave). A failed save is logged: the change
    /// stands in memory, and the next change tries again.
    fn save(&self, book: &Leases) {
        if let Some(p) = &self.state {
            if let Err(e) = book.save(p) {
                eprintln!("tasukeai: cannot save the state to {}: {e}", p.display());
            }
        }
    }

    /// Signals, collected again when the cached ones are older than [metrics] cache_seconds.
    async fn signals(self: &Arc<Self>) -> Collected {
        if let Some((at, c)) = self.cache.lock().unwrap().as_ref() {
            if at.elapsed().as_secs() < self.cfg.metrics.cache_seconds {
                return c.clone();
            }
        }
        let app = self.clone();
        let fresh = tokio::task::spawn_blocking(move || collect(&app.cfg, now()))
            .await
            .unwrap_or_default();
        *self.cache.lock().unwrap() = Some((Instant::now(), fresh.clone()));
        fresh
    }
}

#[derive(Deserialize)]
struct JobQuery {
    job: String,
    #[serde(default = "one")]
    count: u32,
}

fn one() -> u32 {
    1
}

fn err(status: StatusCode, msg: impl ToString) -> Response {
    (status, Json(json!({"error": msg.to_string()}))).into_response()
}

async fn health() -> &'static str {
    "ok"
}

async fn machines(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    Json(serde_json::to_value(app.signals().await).unwrap_or_default())
}

async fn assess_job(
    State(app): State<Arc<App>>,
    Query(q): Query<JobQuery>,
    Query(all): Query<HashMap<String, String>>,
) -> Response {
    let extra = match caller_rules(&all) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let c = app.signals().await;
    let (leases, holds) = {
        let mut book = app.leases.lock().unwrap();
        (book.active(now()).to_vec(), book.holds(now()).to_vec())
    };
    match assess_with(&app.cfg, &q.job, &c.snapshot, &leases, &holds, &extra, now()) {
        Ok(a) => Json(a).into_response(),
        Err(e) => err(StatusCode::NOT_FOUND, e),
    }
}

/// POST /pick?job=: one machine and its lease; 409 with the reasons when none can take it.
async fn pick_job(State(app): State<Arc<App>>, Query(q): Query<JobQuery>) -> Response {
    let c = app.signals().await;
    let result = {
        let mut book = app.leases.lock().unwrap();
        let r = book.pick(&app.cfg, &q.job, &c.snapshot, now());
        if matches!(r, Ok((Some(_), _))) {
            app.save(&book);
        }
        r
    };
    match result {
        Ok((Some(l), a)) => Json(json!({"machine": l.machine, "lease": l, "assessment": a})).into_response(),
        Ok((None, a)) => (StatusCode::CONFLICT, Json(json!({"machine": null, "assessment": a}))).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

/// POST /spread?job=&count=: up to count leases (a machine once per slot); 409 when none.
async fn spread_job(State(app): State<Arc<App>>, Query(q): Query<JobQuery>) -> Response {
    let c = app.signals().await;
    let result = {
        let mut book = app.leases.lock().unwrap();
        let r = book.spread(&app.cfg, &q.job, q.count, &c.snapshot, now());
        if matches!(&r, Ok((ls, _)) if !ls.is_empty()) {
            app.save(&book);
        }
        r
    };
    match result {
        Ok((ls, a)) => {
            let status = if ls.is_empty() {
                StatusCode::CONFLICT
            } else {
                StatusCode::OK
            };
            (status, Json(json!({"leases": ls, "assessment": a}))).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

/// GET /rank?job=[&min.<signal>=x][&max.<signal>=x]: the usable machines in order, no lease (holds count); with the
/// job's endpoint URLs, in the same order.
async fn rank_job(
    State(app): State<Arc<App>>,
    Query(q): Query<JobQuery>,
    Query(all): Query<HashMap<String, String>>,
) -> Response {
    let extra = match caller_rules(&all) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let c = app.signals().await;
    let holds = app.leases.lock().unwrap().holds(now()).to_vec();
    match rank_with(&app.cfg, &q.job, &c.snapshot, &holds, &extra, now()) {
        Ok((order, a)) => {
            let endpoints: Vec<_> = a.usable.iter().map(|c| c.endpoint.clone()).collect();
            Json(json!({"machines": order, "endpoints": endpoints, "assessment": a})).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn list_leases(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    Json(json!(app.leases.lock().unwrap().active(now())))
}

async fn renew_lease(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let mut book = app.leases.lock().unwrap();
    match book.renew(&app.cfg, &id, now()) {
        Some(l) => {
            app.save(&book);
            Json(l).into_response()
        }
        None => err(StatusCode::NOT_FOUND, format!("no lease {id}")),
    }
}

async fn release_lease(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let mut book = app.leases.lock().unwrap();
    if book.release(&id) {
        app.save(&book);
        StatusCode::NO_CONTENT.into_response()
    } else {
        err(StatusCode::NOT_FOUND, format!("no lease {id}"))
    }
}

/// POST /holds {machine, job?, minutes?, note?}: keep a machine out by hand; 201 with the hold.
async fn add_hold(State(app): State<Arc<App>>, Json(r): Json<HoldRequest>) -> Response {
    let until = r.minutes.map(|m| now() + m * 60);
    let mut book = app.leases.lock().unwrap();
    match book.hold(&app.cfg, &r.machine, r.job.as_deref(), until, &r.note, now()) {
        Ok(h) => {
            app.save(&book);
            (StatusCode::CREATED, Json(h)).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn list_holds(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    Json(json!(app.leases.lock().unwrap().holds(now())))
}

async fn end_hold(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let mut book = app.leases.lock().unwrap();
    if book.unhold(&id) {
        app.save(&book);
        StatusCode::NO_CONTENT.into_response()
    } else {
        err(StatusCode::NOT_FOUND, format!("no hold {id}"))
    }
}

fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/machines", get(machines))
        .route("/assess", get(assess_job))
        .route("/pick", post(pick_job))
        .route("/spread", post(spread_job))
        .route("/rank", get(rank_job))
        .route("/leases", get(list_leases))
        .route("/leases/{id}/renew", post(renew_lease))
        .route("/leases/{id}", delete(release_lease))
        .route("/holds", post(add_hold).get(list_holds))
        .route("/holds/{id}", delete(end_hold))
        .with_state(app)
}

/// tasukeai pick: ask a running server; print "machine lease-id", or the reasons and exit 75.
fn pick_remote(server: &str, job: &str) -> anyhow::Result<()> {
    let url = format!("{}/pick", server.trim_end_matches('/'));
    match ureq::post(&url).query("job", job).call() {
        Ok(r) => {
            let v: serde_json::Value = r.into_json()?;
            println!(
                "{} {}",
                v["machine"].as_str().unwrap_or(""),
                v["lease"]["id"].as_str().unwrap_or("")
            );
            Ok(())
        }
        Err(ureq::Error::Status(409, r)) => {
            let v: serde_json::Value = r.into_json().unwrap_or_default();
            for rej in v["assessment"]["rejected"].as_array().into_iter().flatten() {
                let reasons: Vec<&str> = rej["reasons"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|x| x.as_str())
                    .collect();
                eprintln!("{}: {}", rej["machine"].as_str().unwrap_or(""), reasons.join("; "));
            }
            std::process::exit(NONE_FREE)
        }
        Err(e) => Err(e.into()),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // Commands that talk to a running server need no configuration
    match &cli.cmd {
        Cmd::Health { addr } => return health_check(addr),
        Cmd::Pick { job, server } => {
            let (job, server) = (job.clone(), server.clone());
            return tokio::task::spawn_blocking(move || pick_remote(&server, &job)).await?;
        }
        Cmd::Release { lease, server } => {
            let url = format!("{}/leases/{lease}", server.trim_end_matches('/'));
            tokio::task::spawn_blocking(move || ureq::delete(&url).call()).await??;
            return Ok(());
        }
        Cmd::Hold {
            machine,
            job,
            minutes,
            note,
            server,
        } => {
            let url = format!("{}/holds", server.trim_end_matches('/'));
            let body = json!({"machine": machine, "job": job, "minutes": minutes, "note": note});
            let v: serde_json::Value = tokio::task::spawn_blocking(move || match ureq::post(&url).send_json(body) {
                Ok(r) => r.into_json().map_err(anyhow::Error::from),
                Err(ureq::Error::Status(_, r)) => {
                    let v: serde_json::Value = r.into_json().unwrap_or_default();
                    Err(anyhow::anyhow!("{}", v["error"].as_str().unwrap_or("refused")))
                }
                Err(e) => Err(e.into()),
            })
            .await??;
            println!("{}", v["id"].as_str().unwrap_or(""));
            return Ok(());
        }
        Cmd::Unhold { hold, server } => {
            let url = format!("{}/holds/{hold}", server.trim_end_matches('/'));
            tokio::task::spawn_blocking(move || ureq::delete(&url).call()).await??;
            return Ok(());
        }
        Cmd::Holds { server } => {
            let url = format!("{}/holds", server.trim_end_matches('/'));
            let v: serde_json::Value =
                tokio::task::spawn_blocking(move || ureq::get(&url).call()?.into_json().map_err(anyhow::Error::from))
                    .await??;
            println!("{}", serde_json::to_string_pretty(&v)?);
            return Ok(());
        }
        _ => {}
    }
    let cfg = Config::from_file(&cli.config).with_context(|| format!("loading {}", cli.config.display()))?;
    match cli.cmd {
        Cmd::Check => {
            for (name, m) in &cfg.machines {
                println!(
                    "machine {name}: priority {}, {}, jobs {:?}",
                    m.priority, m.accel, m.jobs
                );
            }
            for (name, j) in &cfg.jobs {
                println!("job {name}: {:?}", j.mode);
            }
        }
        Cmd::Assess { job } => {
            let c = tokio::task::spawn_blocking({
                let cfg = cfg.clone();
                move || collect(&cfg, now())
            })
            .await?;
            for e in &c.errors {
                eprintln!("signal: {e}");
            }
            let a = assess(&cfg, &job, &c.snapshot, &[], &[], now())?;
            println!("{}", serde_json::to_string_pretty(&a)?);
        }
        Cmd::Signals => {
            let c = tokio::task::spawn_blocking({
                let cfg = cfg.clone();
                move || collect(&cfg, now())
            })
            .await?;
            println!("{}", serde_json::to_string_pretty(&c)?);
        }
        Cmd::Serve { listen } => {
            let listener = tokio::net::TcpListener::bind(&listen)
                .await
                .with_context(|| format!("listening on {listen}"))?;
            eprintln!("tasukeai listening on {listen}");
            let app = App::new(cfg)?;
            if let Some(p) = &app.state {
                eprintln!("tasukeai keeps leases and holds in {}", p.display());
            }
            axum::serve(listener, router(app))
                .with_graceful_shutdown(shutdown())
                .await?;
        }
        Cmd::Health { .. }
        | Cmd::Pick { .. }
        | Cmd::Release { .. }
        | Cmd::Hold { .. }
        | Cmd::Unhold { .. }
        | Cmd::Holds { .. } => unreachable!(),
    }
    Ok(())
}

/// Ctrl-C, or SIGTERM from a container runtime (as PID 1 in a container, a signal without a handler is ignored).
async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await.ok();
    eprintln!("tasukeai stopping");
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &str = r#"
[machines.a]
priority = 1
accel = "cuda"
jobs = { big = 1, many = 2, chat = 1 }
endpoints = { llm = "http://a/v1" }
[machines.b]
priority = 2
accel = "cuda"
jobs = { many = 1, chat = 1 }
endpoints = { llm = "http://b/v1" }
[jobs.big]
mode = "exclusive"
[jobs.many]
mode = "spread"
endpoint = "llm"
[jobs.chat]
mode = "rank"
endpoint = "llm"
"#;

    /// The real router on a random port, called over HTTP.
    async fn server() -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(l, router(App::new(Config::from_toml(CFG).unwrap()).unwrap()))
                .await
                .unwrap()
        });
        format!("http://{addr}")
    }

    fn call(method: &str, url: &str) -> (u16, serde_json::Value) {
        match ureq::request(method, url).call() {
            Ok(r) => (r.status(), r.into_json().unwrap_or_default()),
            Err(ureq::Error::Status(s, r)) => (s, r.into_json().unwrap_or_default()),
            Err(e) => panic!("{e}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn leases_over_http() {
        let base = server().await;
        tokio::task::spawn_blocking(move || {
            let (s, v) = call("POST", &format!("{base}/pick?job=big"));
            assert_eq!((s, v["machine"].as_str()), (200, Some("a")));
            let id = v["lease"]["id"].as_str().unwrap().to_string();
            let (s, v) = call("POST", &format!("{base}/pick?job=big"));
            assert_eq!(s, 409);
            assert!(v["assessment"]["rejected"].to_string().contains("leased"));
            // the exclusive lease holds machine a for every job
            let (s, v) = call("POST", &format!("{base}/spread?job=many&count=5"));
            assert_eq!((s, v["leases"].as_array().unwrap().len()), (200, 1));
            assert_eq!(v["leases"][0]["machine"], "b");
            let (_, v) = call("GET", &format!("{base}/rank?job=chat"));
            assert_eq!(v["machines"], json!(["a", "b"]), "rank ignores leases");
            let (s, v) = call("POST", &format!("{base}/leases/{id}/renew"));
            assert_eq!((s, v["id"].as_str()), (200, Some(id.as_str())));
            assert_eq!(call("GET", &format!("{base}/leases")).1.as_array().unwrap().len(), 2);
            assert_eq!(call("DELETE", &format!("{base}/leases/{id}")).0, 204);
            assert_eq!(call("DELETE", &format!("{base}/leases/{id}")).0, 404);
            assert_eq!(call("POST", &format!("{base}/pick?job=big")).1["machine"], "a");
            assert_eq!(
                call("POST", &format!("{base}/pick?job=chat")).0,
                400,
                "chat is a rank job"
            );
        })
        .await
        .unwrap();
    }

    fn send(method: &str, url: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        match ureq::request(method, url).send_json(body) {
            Ok(r) => (r.status(), r.into_json().unwrap_or_default()),
            Err(ureq::Error::Status(s, r)) => (s, r.into_json().unwrap_or_default()),
            Err(e) => panic!("{e}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn state_file_survives_a_restart() {
        let path = std::env::temp_dir().join(format!("tasukeai-http-{}.json", std::process::id()));
        let cfg = format!("[state]\npath = {:?}\n{CFG}", path.display().to_string());
        let start = |cfg: String| async move {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            let app = App::new(Config::from_toml(&cfg).unwrap()).unwrap();
            let task = tokio::spawn(async move { axum::serve(l, router(app)).await.unwrap() });
            (format!("http://{addr}"), task)
        };
        let (base, first) = start(cfg.clone()).await;
        let b = base.clone();
        tokio::task::spawn_blocking(move || {
            assert_eq!(
                send(
                    "POST",
                    &format!("{b}/holds"),
                    json!({"machine": "a", "job": "many", "note": "fan"})
                )
                .0,
                201
            );
            assert_eq!(call("POST", &format!("{b}/pick?job=big")).0, 200);
        })
        .await
        .unwrap();
        first.abort();
        let (base, _second) = start(cfg).await; // as after a restart: everything comes from the file
        tokio::task::spawn_blocking(move || {
            let holds = call("GET", &format!("{base}/holds")).1;
            assert_eq!(holds[0]["note"], "fan");
            assert_eq!(call("GET", &format!("{base}/leases")).1.as_array().unwrap().len(), 1);
            assert_eq!(
                call("POST", &format!("{base}/pick?job=big")).0,
                409,
                "a is still leased"
            );
        })
        .await
        .unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn caller_rules_over_http() {
        let base = server().await;
        tokio::task::spawn_blocking(move || {
            // no machine reports vram_free_gb here: a rule of the caller on it fails closed, with the reason
            let (s, v) = call("GET", &format!("{base}/rank?job=chat&min.vram_free_gb=10"));
            assert_eq!((s, v["machines"].clone()), (200, json!([])));
            assert!(
                v["assessment"]["rejected"]
                    .to_string()
                    .contains("vram_free_gb unknown (asked by the caller)"),
                "{v}"
            );
            assert_eq!(
                call("GET", &format!("{base}/assess?job=chat&max.gpu_temp_celsius=80")).1["usable"],
                json!([])
            );
            assert_eq!(
                call("GET", &format!("{base}/rank?job=chat&min.vram_free_gb=lots")).0,
                400
            );
            assert_eq!(
                call("GET", &format!("{base}/rank?job=chat")).1["machines"],
                json!(["a", "b"]),
                "without them as before"
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn holds_and_endpoints_over_http() {
        let base = server().await;
        tokio::task::spawn_blocking(move || {
            // leases and rank carry the job's endpoint
            let (_, v) = call("POST", &format!("{base}/spread?job=many&count=1"));
            assert_eq!(v["leases"][0]["endpoint"], "http://a/v1");
            assert!(
                call("POST", &format!("{base}/pick?job=big")).1["lease"]
                    .get("endpoint")
                    .is_none(),
                "big names no endpoint"
            );
            let (_, v) = call("GET", &format!("{base}/rank?job=chat"));
            assert_eq!(v["endpoints"], json!(["http://a/v1", "http://b/v1"]));
            // a hold on a keeps it out of rank, with the note in the reason
            let (s, h) = send(
                "POST",
                &format!("{base}/holds"),
                json!({"machine": "a", "minutes": 60, "note": "game"}),
            );
            assert_eq!(s, 201);
            let (_, v) = call("GET", &format!("{base}/rank?job=chat"));
            assert_eq!(v["machines"], json!(["b"]));
            assert!(v["assessment"]["rejected"].to_string().contains("held by hand"), "{v}");
            assert_eq!(
                v["assessment"]["rejected"][0]["endpoint"], "http://a/v1",
                "left-out machines show their endpoint too"
            );
            assert!(v["assessment"]["rejected"].to_string().contains("game"));
            assert_eq!(call("GET", &format!("{base}/holds")).1.as_array().unwrap().len(), 1);
            assert_eq!(
                send("POST", &format!("{base}/holds"), json!({"machine": "nope"})).0,
                400
            );
            let id = h["id"].as_str().unwrap();
            assert_eq!(call("DELETE", &format!("{base}/holds/{id}")).0, 204);
            assert_eq!(call("DELETE", &format!("{base}/holds/{id}")).0, 404);
            assert_eq!(
                call("GET", &format!("{base}/rank?job=chat")).1["machines"],
                json!(["a", "b"])
            );
        })
        .await
        .unwrap();
    }
}
