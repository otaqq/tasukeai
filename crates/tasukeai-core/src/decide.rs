//! Decisions: which machines can take a job now, in priority order, with the reasons for the others.
//! Pure functions over a snapshot of signals and the current leases, so they are easy to test and to use from a
//! library caller that collects signals itself.

use crate::config::{Config, Job, Mode, Rule};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The signals of one machine at a moment: name -> (value, unix seconds when it was measured).
pub type Signals = BTreeMap<String, (f64, u64)>;

/// Every machine's signals.
pub type Snapshot = BTreeMap<String, Signals>;

/// A reservation of one slot of a machine for a job until a time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Lease {
    pub id: String,
    pub machine: String,
    pub job: String,
    pub until: u64,
    /// The URL of the job's endpoint on that machine, when the job names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

/// A machine kept out by a person: for every job (job None) or one job, until a time (None: until removed).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hold {
    pub id: String,
    pub machine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

impl Hold {
    pub fn active(&self, now: u64) -> bool {
        self.until.map_or(true, |u| u > now)
    }

    fn reason(&self) -> String {
        let job = self.job.as_deref().map(|j| format!(" for {j}")).unwrap_or_default();
        let until = self.until.map(|u| format!(" until {u}")).unwrap_or_default();
        let note = if self.note.is_empty() { String::new() } else { format!(": {}", self.note) };
        format!("held by hand{job}{until}{note}")
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Candidate {
    pub machine: String,
    pub priority: i64,
    /// Slots left for this job now (after leases).
    pub free_slots: u32,
    /// The URL of the job's endpoint on this machine, when the job names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Rejected {
    pub machine: String,
    pub reasons: Vec<String>,
}

/// The usable machines in priority order, and why the others are not.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Assessment {
    pub job: String,
    pub usable: Vec<Candidate>,
    pub rejected: Vec<Rejected>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum DecideError {
    #[error("unknown job {0}")]
    UnknownJob(String),
    #[error("job {job} is {mode:?}, not {wanted:?}")]
    WrongMode { job: String, mode: Mode, wanted: Mode },
}

fn rule_reason(rule: &Rule, signals: &Signals, now: u64, stale_after: u64) -> Option<String> {
    let because = rule.because.as_deref().map(|b| format!(" ({b})")).unwrap_or_default();
    match signals.get(&rule.signal) {
        None => Some(format!("{} unknown{because}", rule.signal)),
        Some((_, at)) if now.saturating_sub(*at) > stale_after => {
            Some(format!("{} stale ({} s old){because}", rule.signal, now.saturating_sub(*at)))
        }
        Some((v, _)) => {
            if let Some(min) = rule.min {
                if *v < min {
                    return Some(format!("{} = {v} < {min}{because}", rule.signal));
                }
            }
            if let Some(max) = rule.max {
                if *v > max {
                    return Some(format!("{} = {v} > {max}{because}", rule.signal));
                }
            }
            None
        }
    }
}

/// Which machines can take `job` now. Static checks (job installed, accelerator, installed memory) come first, then
/// leases and holds by hand, then the machine's and the job's rules on the signals; every failed check is a reason.
pub fn assess(cfg: &Config, job: &str, snap: &Snapshot, leases: &[Lease], holds: &[Hold], now: u64)
    -> Result<Assessment, DecideError> {
    let j: &Job = cfg.jobs.get(job).ok_or_else(|| DecideError::UnknownJob(job.to_string()))?;
    let mut machines: Vec<_> = cfg.machines.iter().collect();
    machines.sort_by_key(|(name, m)| (m.priority, (*name).clone()));
    let empty = Signals::new();
    let (mut usable, mut rejected) = (Vec::new(), Vec::new());
    for (name, m) in machines {
        let mut reasons = Vec::new();
        let slots = m.jobs.get(job).copied().unwrap_or(0);
        if slots == 0 {
            reasons.push("job not installed here".to_string());
        }
        if !j.accel.is_empty() && !j.accel.iter().any(|a| a == &m.accel) {
            reasons.push(format!("needs {}, has {}", j.accel.join("/"), m.accel));
        }
        if m.vram_gb < j.vram_gb {
            reasons.push(format!("{} GB VRAM installed, needs {}", m.vram_gb, j.vram_gb));
        }
        if m.ram_gb < j.ram_gb {
            reasons.push(format!("{} GB RAM installed, needs {}", m.ram_gb, j.ram_gb));
        }
        let live: Vec<&Lease> = leases.iter().filter(|l| &l.machine == name && l.until > now).collect();
        // A lease of an exclusive job holds the whole machine; other leases hold slots of their own job only.
        if let Some(l) = live.iter().find(|l| l.job != job && cfg.jobs.get(&l.job).map(|o| o.mode) == Some(Mode::Exclusive)) {
            reasons.push(format!("held by {} (exclusive)", l.job));
        }
        let held = live.iter().filter(|l| l.job == job).count() as u32;
        let free_slots = slots.saturating_sub(held);
        if slots > 0 && free_slots == 0 && j.mode != Mode::Rank {
            reasons.push(format!("all {slots} slot(s) leased"));
        }
        for h in holds.iter().filter(|h| h.active(now) && &h.machine == name && h.job.as_deref().map_or(true, |hj| hj == job)) {
            reasons.push(h.reason());
        }
        let signals = snap.get(name).unwrap_or(&empty);
        for rule in m.rules.iter().chain(j.rules.iter()) {
            if let Some(r) = rule_reason(rule, signals, now, cfg.stale_after_seconds) {
                reasons.push(r);
            }
        }
        if reasons.is_empty() {
            let endpoint = j.endpoint.as_ref().and_then(|e| m.endpoints.get(e)).cloned();
            usable.push(Candidate { machine: name.clone(), priority: m.priority, free_slots, endpoint });
        } else {
            rejected.push(Rejected { machine: name.clone(), reasons });
        }
    }
    Ok(Assessment { job: job.to_string(), usable, rejected })
}

fn expect_mode(cfg: &Config, job: &str, wanted: Mode) -> Result<(), DecideError> {
    let mode = cfg.jobs.get(job).ok_or_else(|| DecideError::UnknownJob(job.to_string()))?.mode;
    if mode != wanted {
        return Err(DecideError::WrongMode { job: job.to_string(), mode, wanted });
    }
    Ok(())
}

/// `exclusive`: the first usable machine, or None. The caller records a lease for it.
pub fn pick(cfg: &Config, job: &str, snap: &Snapshot, leases: &[Lease], holds: &[Hold], now: u64)
    -> Result<(Option<String>, Assessment), DecideError> {
    expect_mode(cfg, job, Mode::Exclusive)?;
    let a = assess(cfg, job, snap, leases, holds, now)?;
    Ok((a.usable.first().map(|c| c.machine.clone()), a))
}

/// `spread`: up to `count` machines (a machine appears once per slot it gives), filling slots in priority order.
pub fn spread(cfg: &Config, job: &str, count: u32, snap: &Snapshot, leases: &[Lease], holds: &[Hold], now: u64)
    -> Result<(Vec<String>, Assessment), DecideError> {
    expect_mode(cfg, job, Mode::Spread)?;
    let a = assess(cfg, job, snap, leases, holds, now)?;
    let mut out = Vec::new();
    for c in &a.usable {
        for _ in 0..c.free_slots {
            if out.len() as u32 >= count {
                return Ok((out, a));
            }
            out.push(c.machine.clone());
        }
    }
    Ok((out, a))
}

/// `rank`: the usable machines in order (no lease; holds by hand still count).
pub fn rank(cfg: &Config, job: &str, snap: &Snapshot, holds: &[Hold], now: u64) -> Result<(Vec<String>, Assessment), DecideError> {
    expect_mode(cfg, job, Mode::Rank)?;
    let a = assess(cfg, job, snap, &[], holds, now)?;
    Ok((a.usable.iter().map(|c| c.machine.clone()).collect(), a))
}
