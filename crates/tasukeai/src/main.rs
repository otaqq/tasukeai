//! tasukeai: tell which machine should take a piece of work now (process form; see docs/design.md).
//!
//! Collects signals (PromQL, HTTP checks; cached for [metrics] cache_seconds) and answers which machines can take a
//! job. Leases over HTTP come next.

use anyhow::Context;
use axum::{extract::{Query, State}, http::StatusCode, routing::get, Json, Router};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use std::{path::PathBuf, sync::{Arc, Mutex}, time::{Instant, SystemTime, UNIX_EPOCH}};
use tasukeai_core::{assess, collect, Collected, Config};

#[derive(Parser)]
#[command(name = "tasukeai", version, about = "Tell which machine should take a piece of work now")]
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
    /// Which machines can take a job now, with the reasons for the others
    Assess { job: String },
    /// The signals of every machine now, and what could not be collected
    Signals,
    /// Exit 0 when a running server answers /health (for container health checks; needs no configuration)
    Health {
        #[arg(long, default_value = "127.0.0.1:9660")]
        addr: String,
    },
}

/// GET /health over a plain TCP connection: no HTTP client library for one line.
fn health_check(addr: &str) -> anyhow::Result<()> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect_timeout(&addr.parse()?, std::time::Duration::from_secs(3))?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
    s.write_all(format!("GET /health HTTP/1.0\r\nHost: {addr}\r\n\r\n").as_bytes())?;
    let mut buf = [0u8; 64];
    let n = s.read(&mut buf)?;
    let head = String::from_utf8_lossy(&buf[..n]);
    anyhow::ensure!(head.starts_with("HTTP/1.0 200") || head.starts_with("HTTP/1.1 200"), "unhealthy: {}", head.lines().next().unwrap_or(""));
    Ok(())
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

struct App {
    cfg: Config,
    cache: Mutex<Option<(Instant, Collected)>>,
}

impl App {
    /// Signals, collected again when the cached ones are older than [metrics] cache_seconds.
    async fn signals(self: &Arc<Self>) -> Collected {
        if let Some((at, c)) = self.cache.lock().unwrap().as_ref() {
            if at.elapsed().as_secs() < self.cfg.metrics.cache_seconds {
                return c.clone();
            }
        }
        let app = self.clone();
        let fresh = tokio::task::spawn_blocking(move || collect(&app.cfg, now())).await.unwrap_or_default();
        *self.cache.lock().unwrap() = Some((Instant::now(), fresh.clone()));
        fresh
    }
}

#[derive(Deserialize)]
struct JobQuery {
    job: String,
}

async fn health() -> &'static str {
    "ok"
}

async fn assess_job(State(app): State<Arc<App>>, Query(q): Query<JobQuery>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let c = app.signals().await;
    let a = assess(&app.cfg, &q.job, &c.snapshot, &[], now()).map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    Ok(Json(serde_json::to_value(a).unwrap_or_default()))
}

async fn machines(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    Json(serde_json::to_value(app.signals().await).unwrap_or_default())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Cmd::Health { addr } = &cli.cmd {
        return health_check(addr);
    }
    let cfg = Config::from_file(&cli.config).with_context(|| format!("loading {}", cli.config.display()))?;
    match cli.cmd {
        Cmd::Check => {
            for (name, m) in &cfg.machines {
                println!("machine {name}: priority {}, {}, jobs {:?}", m.priority, m.accel, m.jobs);
            }
            for (name, j) in &cfg.jobs {
                println!("job {name}: {:?}", j.mode);
            }
        }
        Cmd::Assess { job } => {
            let c = tokio::task::spawn_blocking({ let cfg = cfg.clone(); move || collect(&cfg, now()) }).await?;
            for e in &c.errors {
                eprintln!("signal: {e}");
            }
            let a = assess(&cfg, &job, &c.snapshot, &[], now())?;
            println!("{}", serde_json::to_string_pretty(&a)?);
        }
        Cmd::Signals => {
            let c = tokio::task::spawn_blocking({ let cfg = cfg.clone(); move || collect(&cfg, now()) }).await?;
            println!("{}", serde_json::to_string_pretty(&c)?);
        }
        Cmd::Health { .. } => unreachable!(),
        Cmd::Serve { listen } => {
            let app = Arc::new(App { cfg, cache: Mutex::new(None) });
            let router = Router::new()
                .route("/health", get(health))
                .route("/machines", get(machines))
                .route("/assess", get(assess_job))
                .with_state(app);
            let listener = tokio::net::TcpListener::bind(&listen).await.with_context(|| format!("listening on {listen}"))?;
            eprintln!("tasukeai listening on {listen}");
            axum::serve(listener, router).with_graceful_shutdown(async { tokio::signal::ctrl_c().await.ok(); }).await?;
        }
    }
    Ok(())
}
