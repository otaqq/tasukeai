//! Signal collection: PromQL instant queries against a Prometheus-compatible API and quick HTTP checks, one thread per
//! machine. Blocking (no async runtime needed), so the library can be used from any program.
//!
//! A query that returns nothing leaves its signal out (unknown): a machine that stopped reporting has no series. A
//! failed request is reported in `errors` and its signal is unknown too. An HTTP check that fails gives 0, since "does
//! not answer" is itself the information a check is for.

use crate::config::{Check, Config};
use crate::decide::{Signals, Snapshot};
use serde::Serialize;
use std::time::Duration;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Collected {
    pub snapshot: Snapshot,
    /// "machine signal: what went wrong", for /machines and logs.
    pub errors: Vec<String>,
}

/// The value of an instant query: the first sample of a vector, or a scalar. None when there is no result.
pub fn parse_query_response(body: &serde_json::Value) -> Result<Option<f64>, String> {
    if body.get("status").and_then(|s| s.as_str()) != Some("success") {
        return Err(format!(
            "query failed: {}",
            body.get("error").and_then(|e| e.as_str()).unwrap_or("no status")
        ));
    }
    let data = body.get("data").ok_or("no data")?;
    let value = match data.get("resultType").and_then(|t| t.as_str()) {
        Some("vector") => data["result"].as_array().and_then(|r| r.first()).map(|s| &s["value"]),
        Some("scalar") => Some(&data["result"]),
        Some(other) => return Err(format!("unexpected result type {other}")),
        None => return Err("no result type".into()),
    };
    let Some(v) = value else { return Ok(None) };
    let text = v.get(1).and_then(|x| x.as_str()).ok_or("sample without a value")?;
    let f: f64 = text.parse().map_err(|_| format!("not a number: {text}"))?;
    Ok(if f.is_nan() { None } else { Some(f) })
}

fn agent(timeout_ms: u64) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
}

fn query(agent: &ureq::Agent, base: &str, expr: &str) -> Result<Option<f64>, String> {
    let url = format!("{}/api/v1/query", base.trim_end_matches('/'));
    let resp = agent.get(&url).query("query", expr).call().map_err(|e| e.to_string())?;
    let body: serde_json::Value = resp.into_json().map_err(|e| e.to_string())?;
    parse_query_response(&body)
}

/// The value of one HTTP check (see `Check`). Err only when the JSON pointer finds no number.
pub fn run_check(c: &Check, default_timeout_ms: u64) -> Result<f64, String> {
    let resp = match agent(c.timeout_ms.unwrap_or(default_timeout_ms)).get(&c.url).call() {
        Ok(r) => r,
        Err(_) => return Ok(0.0), // refused, timed out, 4xx/5xx: does not answer as wanted
    };
    let Some(pointer) = &c.json else { return Ok(1.0) };
    let body: serde_json::Value = resp.into_json().map_err(|e| format!("not JSON: {e}"))?;
    match body.pointer(pointer) {
        Some(serde_json::Value::Number(n)) => n.as_f64().ok_or_else(|| "number out of range".into()),
        Some(serde_json::Value::Bool(b)) => Ok(if *b { 1.0 } else { 0.0 }),
        Some(other) => Err(format!("{pointer} is not a number: {other}")),
        None => Err(format!("{pointer} not found")),
    }
}

fn collect_machine(cfg: &Config, name: &str, now: u64) -> (Signals, Vec<String>) {
    let (mut signals, mut errors) = (Signals::new(), Vec::new());
    let queries = cfg.queries_of(name);
    if !queries.is_empty() {
        match &cfg.metrics.url {
            None => errors.push(format!("{name}: signals need [metrics] url")),
            Some(base) => {
                let a = agent(cfg.metrics.timeout_ms);
                for (signal, expr) in queries {
                    match query(&a, base, &expr) {
                        Ok(Some(v)) => {
                            signals.insert(signal, (v, now));
                        }
                        Ok(None) => {}
                        Err(e) => errors.push(format!("{name} {signal}: {e}")),
                    }
                }
            }
        }
    }
    for c in &cfg.machines[name].checks {
        match run_check(c, cfg.metrics.timeout_ms) {
            Ok(v) => {
                signals.insert(c.signal.clone(), (v, now));
            }
            Err(e) => errors.push(format!("{name} {}: {e}", c.signal)),
        }
    }
    (signals, errors)
}

/// Every machine's signals now, collected in parallel (one thread per machine).
pub fn collect(cfg: &Config, now: u64) -> Collected {
    let mut out = Collected::default();
    std::thread::scope(|s| {
        let handles: Vec<_> = cfg
            .machines
            .keys()
            .map(|n| (n.clone(), s.spawn(move || collect_machine(cfg, n, now))))
            .collect();
        for (name, h) in handles {
            match h.join() {
                Ok((signals, errors)) => {
                    out.snapshot.insert(name, signals);
                    out.errors.extend(errors);
                }
                Err(_) => out.errors.push(format!("{name}: collector panicked")),
            }
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn parses_vector_scalar_and_empty() {
        let v = json!({"status": "success", "data": {"resultType": "vector", "result": [{"metric": {}, "value": [1, "42.5"]}]}});
        assert_eq!(parse_query_response(&v), Ok(Some(42.5)));
        let s = json!({"status": "success", "data": {"resultType": "scalar", "result": [1, "3"]}});
        assert_eq!(parse_query_response(&s), Ok(Some(3.0)));
        let e = json!({"status": "success", "data": {"resultType": "vector", "result": []}});
        assert_eq!(parse_query_response(&e), Ok(None));
        let nan = json!({"status": "success", "data": {"resultType": "vector", "result": [{"metric": {}, "value": [1, "NaN"]}]}});
        assert_eq!(parse_query_response(&nan), Ok(None));
        let bad = json!({"status": "error", "error": "parse error"});
        assert!(parse_query_response(&bad).unwrap_err().contains("parse error"));
    }

    /// A tiny HTTP server answering each request with the body `answer(path_and_query)` returns (None: 500).
    fn serve(answer: fn(&str) -> Option<String>) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in l.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let resp = match answer(&path) {
                    Some(b) => format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}", b.len()),
                    None => "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                };
                let _ = s.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    fn prom(path: &str) -> Option<String> {
        // up{instance="desk"} -> 1; MemAvailable -> 8 GiB; anything with "absent" -> no series
        let q = path.replace("%22", "\"").replace("%7B", "{").replace("%7D", "}");
        let v = if q.contains("absent") {
            return Some(r#"{"status":"success","data":{"resultType":"vector","result":[]}}"#.into());
        } else if q.contains("MemAvailable") {
            "8589934592"
        } else if q.contains("up") {
            "1"
        } else {
            return None;
        };
        Some(format!(
            r#"{{"status":"success","data":{{"resultType":"vector","result":[{{"metric":{{}},"value":[1,"{v}"]}}]}}}}"#
        ))
    }

    fn status(path: &str) -> Option<String> {
        match path {
            "/ok" => Some(r#"{"vram_free_mb": 9000, "hot": false}"#.into()),
            _ => None,
        }
    }

    #[test]
    fn collects_queries_and_checks() {
        let p = serve(prom);
        let h = serve(status);
        let cfg = Config::from_toml(&format!(
            r#"
[metrics]
url = "{p}"
[machines.desk]
priority = 1
accel = "cuda"
presets = ["node"]
queries = {{ gone = 'absent_metric{{instance="{{instance}}"}}' }}
checks = [{{ signal = "status_up", url = "{h}/ok" }},
          {{ signal = "vram_free_mb", url = "{h}/ok", json = "/vram_free_mb" }},
          {{ signal = "hot", url = "{h}/ok", json = "/hot" }},
          {{ signal = "down", url = "{h}/missing" }},
          {{ signal = "nowhere", url = "http://127.0.0.1:9/" }}]
"#
        ))
        .unwrap();
        let got = collect(&cfg, 100);
        let s = &got.snapshot["desk"];
        assert_eq!(s["up"], (1.0, 100));
        assert_eq!(s["ram_usable_bytes"].0, 8589934592.0);
        assert!(!s.contains_key("gone"), "no series = unknown");
        assert_eq!(s["status_up"].0, 1.0);
        assert_eq!(s["vram_free_mb"].0, 9000.0);
        assert_eq!(s["hot"].0, 0.0);
        assert_eq!(s["down"].0, 0.0);
        assert_eq!(s["nowhere"].0, 0.0);
        // the node preset's cpu_busy_ratio and others hit the fake server's 500
        assert!(got.errors.iter().any(|e| e.starts_with("desk cpu_busy_ratio")));
    }

    #[test]
    fn presets_and_instance_labels() {
        let cfg = Config::from_toml(
            r#"
[presets.gpu]
vram_free_bytes = 'my_gpu_free{instance="{instance}"}'
[machines.a]
priority = 1
accel = "cpu"
instance = "a.example:9100"
presets = ["node", "gpu"]
queries = { load15 = 'custom_load{instance="{instance}"}' }
"#,
        )
        .unwrap();
        let q = cfg.queries_of("a");
        assert_eq!(q["vram_free_bytes"], r#"my_gpu_free{instance="a.example:9100"}"#);
        assert_eq!(
            q["load15"], r#"custom_load{instance="a.example:9100"}"#,
            "machine queries win over presets"
        );
        assert!(q["ram_usable_bytes"].contains("MemAvailable"));
        let bad = Config::from_toml("[machines.a]\npriority = 1\naccel = \"cpu\"\npresets = [\"nope\"]\n");
        assert!(bad.unwrap_err().to_string().contains("unknown preset nope"));
    }
}
