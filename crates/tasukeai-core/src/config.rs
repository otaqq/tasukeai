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
}

fn default_lease() -> u64 {
    600
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Signals older than this many seconds count as unknown.
    #[serde(default = "default_stale")]
    pub stale_after_seconds: u64,
    pub machines: BTreeMap<String, Machine>,
    #[serde(default)]
    pub jobs: BTreeMap<String, Job>,
}

fn default_stale() -> u64 {
    120
}

impl Config {
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let c: Config = toml::from_str(text)?;
        c.validate()?;
        Ok(c)
    }

    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Read(path.display().to_string(), e))?;
        Self::from_toml(&text)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for (name, m) in &self.machines {
            for job in m.jobs.keys() {
                if !self.jobs.contains_key(job) {
                    return Err(ConfigError::Invalid(format!("machine {name} lists unknown job {job}")));
                }
            }
            for r in &m.rules {
                check_rule(r, &format!("machine {name}"))?;
            }
        }
        for (name, j) in &self.jobs {
            for r in &j.rules {
                check_rule(r, &format!("job {name}"))?;
            }
        }
        Ok(())
    }
}

fn check_rule(r: &Rule, at: &str) -> Result<(), ConfigError> {
    if r.min.is_none() && r.max.is_none() {
        return Err(ConfigError::Invalid(format!("{at}: rule on {} has neither min nor max", r.signal)));
    }
    Ok(())
}
