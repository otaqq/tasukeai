//! Configuration: machines (static facts, rules, job slots) and job kinds (needs, mode). One TOML file, the same for
//! the library and the process.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {0}: {1}")]
    Read(String, std::io::Error),
    #[error("bad configuration: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("bad configuration: {0}")]
    Invalid(String),
}

/// How a job kind is placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// One machine for one job, in priority order, with a lease.
    Exclusive,
    /// Up to N assignments over the machines' slots, in priority order, each with a lease.
    Spread,
    /// The usable machines in order, no lease (short requests).
    Rank,
}

/// A rule a machine must pass: a signal compared with a bound. A missing or stale signal fails the rule.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Rule {
    pub signal: String,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    /// Shown in the reason when the rule fails, e.g. "someone is using it".
    #[serde(default)]
    pub because: Option<String>,
    /// Only for these jobs (a machine rule such as "nobody is using it" for long batch work, but not for short chat
    /// requests). None: every job.
    #[serde(default)]
    pub jobs: Option<Vec<String>>,
}

impl Rule {
    /// Whether the rule counts for `job`.
    pub fn applies_to(&self, job: &str) -> bool {
        self.jobs.as_ref().map_or(true, |js| js.iter().any(|j| j == job))
    }
}

/// A quick HTTP check at decision time: the signal is 1 when the URL answers 2xx in time and 0 when it does not; with
/// `json`, the number (or boolean) at that JSON pointer instead, e.g. "/vram_free_mb".
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Check {
    pub signal: String,
    pub url: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub json: Option<String>,
}

/// Where signals come from: a Prometheus-compatible query API (Prometheus, VictoriaMetrics, Thanos ...).
#[derive(Debug, Clone, Deserialize)]
pub struct Metrics {
    /// Base URL, e.g. http://prometheus:9090 (the API is under /api/v1/query). None: only HTTP checks.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// The process keeps collected signals this long before asking again.
    #[serde(default = "default_cache")]
    pub cache_seconds: u64,
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics { url: None, timeout_ms: default_timeout(), cache_seconds: default_cache() }
    }
}

fn default_timeout() -> u64 {
    3000
}

fn default_cache() -> u64 {
    10
}

#[derive(Debug, Clone, Deserialize)]
pub struct Machine {
    /// Lower is tried first.
    pub priority: i64,
    /// cuda, rocm, metal, cpu ...
    pub accel: String,
    #[serde(default)]
    pub vram_gb: f64,
    #[serde(default)]
    pub ram_gb: f64,
    /// Owner rules: apply to every job on this machine.
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// Job kinds installed here, with how many can run at once (slots).
    #[serde(default)]
    pub jobs: BTreeMap<String, u32>,
    /// The value of the `instance` label of its metrics; defaults to the machine's name.
    #[serde(default)]
    pub instance: Option<String>,
    /// Presets whose signals this machine has (built-in node, node_darwin, windows, or the configuration's own);
    /// later ones win for the same signal.
    #[serde(default)]
    pub presets: Vec<String>,
    /// Signals of this machine only (PromQL, {instance} replaced); they win over presets.
    #[serde(default)]
    pub queries: BTreeMap<String, String>,
    #[serde(default)]
    pub checks: Vec<Check>,
    /// Named addresses of services on this machine, e.g. llm = "http://10.0.0.5:11434/v1". A job that names one gets
    /// its URL with each lease (and in rank answers), so callers keep no address table of their own.
    #[serde(default)]
    pub endpoints: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Job {
    pub mode: Mode,
    /// Accelerators the job can use; empty means any.
    #[serde(default)]
    pub accel: Vec<String>,
    /// Installed VRAM / RAM the job needs at least (static check; usable amounts are rules on signals).
    #[serde(default)]
    pub vram_gb: f64,
    #[serde(default)]
    pub ram_gb: f64,
    /// Rules of the job, on top of the machine's.
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// How long a lease lasts, in seconds.
    #[serde(default = "default_lease")]
    pub lease_seconds: u64,
    /// The machine endpoint this job sends its work to (a key of [machines.x.endpoints]); every machine with a slot
    /// for the job must have it.
    #[serde(default)]
    pub endpoint: Option<String>,
}

fn default_lease() -> u64 {
    600
}

/// Where the process keeps its leases and holds across restarts.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct State {
    /// A JSON file, written whole on each change (to a temporary file, then renamed). None: memory only.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Signals older than this many seconds count as unknown.
    #[serde(default = "default_stale")]
    pub stale_after_seconds: u64,
    #[serde(default)]
    pub metrics: Metrics,
    /// Presets of this configuration, added to (and overriding) the built-in ones.
    #[serde(default)]
    pub presets: BTreeMap<String, BTreeMap<String, String>>,
    pub machines: BTreeMap<String, Machine>,
    #[serde(default)]
    pub jobs: BTreeMap<String, Job>,
    #[serde(default)]
    pub state: State,
}

fn default_stale() -> u64 {
    120
}

impl Config {
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let mut c: Config = toml::from_str(text)?;
        let builtin: BTreeMap<String, BTreeMap<String, String>> =
            toml::from_str(include_str!("presets.toml")).expect("built-in presets parse");
        for (name, signals) in builtin {
            let own = c.presets.remove(&name).unwrap_or_default();
            let mut merged = signals;
            merged.extend(own);
            c.presets.insert(name, merged);
        }
        c.validate()?;
        Ok(c)
    }

    /// The PromQL expression of every signal of a machine, {instance} replaced.
    pub fn queries_of(&self, machine: &str) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        let Some(m) = self.machines.get(machine) else { return out };
        let instance = m.instance.clone().unwrap_or_else(|| machine.to_string());
        for p in &m.presets {
            if let Some(signals) = self.presets.get(p) {
                out.extend(signals.iter().map(|(k, v)| (k.clone(), v.replace("{instance}", &instance))));
            }
        }
        out.extend(m.queries.iter().map(|(k, v)| (k.clone(), v.replace("{instance}", &instance))));
        out
    }

    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Read(path.display().to_string(), e))?;
        Self::from_toml(&text)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for (name, m) in &self.machines {
            for (job, slots) in &m.jobs {
                let Some(j) = self.jobs.get(job) else {
                    return Err(ConfigError::Invalid(format!("machine {name} lists unknown job {job}")));
                };
                if let Some(e) = &j.endpoint {
                    if *slots > 0 && !m.endpoints.contains_key(e) {
                        return Err(ConfigError::Invalid(format!("job {job} uses endpoint {e}, which machine {name} does not have")));
                    }
                }
            }
            for r in &m.rules {
                check_rule(r, &format!("machine {name}"), &self.jobs)?;
            }
            for p in &m.presets {
                if !self.presets.contains_key(p) {
                    return Err(ConfigError::Invalid(format!("machine {name} uses unknown preset {p}")));
                }
            }
        }
        for (name, j) in &self.jobs {
            for r in &j.rules {
                check_rule(r, &format!("job {name}"), &self.jobs)?;
            }
        }
        Ok(())
    }
}

fn check_rule(r: &Rule, at: &str, jobs: &BTreeMap<String, Job>) -> Result<(), ConfigError> {
    if r.min.is_none() && r.max.is_none() {
        return Err(ConfigError::Invalid(format!("{at}: rule on {} has neither min nor max", r.signal)));
    }
    for j in r.jobs.iter().flatten() {
        if !jobs.contains_key(j) {
            return Err(ConfigError::Invalid(format!("{at}: rule on {} names unknown job {j}", r.signal)));
        }
    }
    Ok(())
}
