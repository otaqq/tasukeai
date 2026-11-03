//! Leases: short reservations of machines handed out by `exclusive` and `spread`, so that the next decision (before
//! the metrics show the new load) does not hand the same machine out again; and holds: machines a person keeps out
//! by hand. In memory; the process keeps one book for all its callers, a library caller keeps its own.

use crate::config::Config;
use crate::decide::{pick, spread, Assessment, DecideError, Hold, Lease, Snapshot};

#[derive(Debug, Default)]
pub struct Leases {
    list: Vec<Lease>,
    holds: Vec<Hold>,
    counter: u64,
}

impl Leases {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget leases and holds that have run out.
    fn prune(&mut self, now: u64) {
        self.list.retain(|l| l.until > now);
        self.holds.retain(|h| h.active(now));
    }

    /// The leases that have not run out (and forget the others).
    pub fn active(&mut self, now: u64) -> &[Lease] {
        self.prune(now);
        &self.list
    }

    /// The holds that have not run out (and forget the others).
    pub fn holds(&mut self, now: u64) -> &[Hold] {
        self.prune(now);
        &self.holds
    }

    /// Keep a machine out by hand: for every job, or one job; until a time (None: until unhold). Leases already given
    /// stay until they run out or are released.
    pub fn hold(&mut self, cfg: &Config, machine: &str, job: Option<&str>, until: Option<u64>, note: &str, now: u64)
        -> Result<Hold, String> {
        if !cfg.machines.contains_key(machine) {
            return Err(format!("unknown machine {machine}"));
        }
        if let Some(j) = job {
            if !cfg.jobs.contains_key(j) {
                return Err(format!("unknown job {j}"));
            }
        }
        if until.is_some_and(|u| u <= now) {
            return Err("the hold would end before it starts".into());
        }
        self.counter += 1;
        let hold = Hold { id: format!("h{now:x}-{:x}", self.counter), machine: machine.into(), job: job.map(String::from), until, note: note.into() };
        self.holds.push(hold.clone());
        Ok(hold)
    }

    /// End a hold. False when there was no such hold.
    pub fn unhold(&mut self, id: &str) -> bool {
        let before = self.holds.len();
        self.holds.retain(|h| h.id != id);
        self.holds.len() != before
    }

    fn take(&mut self, cfg: &Config, job: &str, machine: &str, now: u64) -> Lease {
        self.counter += 1;
        let seconds = cfg.jobs.get(job).map(|j| j.lease_seconds).unwrap_or(600);
        let endpoint = cfg.jobs.get(job).and_then(|j| j.endpoint.as_ref())
            .and_then(|e| cfg.machines.get(machine).and_then(|m| m.endpoints.get(e))).cloned();
        let lease = Lease { id: format!("{now:x}-{:x}", self.counter), machine: machine.into(), job: job.into(), until: now + seconds, endpoint };
        self.list.push(lease.clone());
        lease
    }

    /// `exclusive`: decide and lease in one step (the caller holds the book while doing it, so two callers cannot
    /// get the same machine).
    pub fn pick(&mut self, cfg: &Config, job: &str, snap: &Snapshot, now: u64) -> Result<(Option<Lease>, Assessment), DecideError> {
        self.prune(now);
        let (m, a) = pick(cfg, job, snap, &self.list, &self.holds, now)?;
        Ok((m.map(|m| self.take(cfg, job, &m, now)), a))
    }

    /// `spread`: up to `count` leases, one per slot given.
    pub fn spread(&mut self, cfg: &Config, job: &str, count: u32, snap: &Snapshot, now: u64) -> Result<(Vec<Lease>, Assessment), DecideError> {
        self.prune(now);
        let (ms, a) = spread(cfg, job, count, snap, &self.list, &self.holds, now)?;
        Ok((ms.iter().map(|m| self.take(cfg, job, m, now)).collect(), a))
    }

    /// Give a machine back (the job ended or did not start). False when there was no such lease.
    pub fn release(&mut self, id: &str) -> bool {
        let before = self.list.len();
        self.list.retain(|l| l.id != id);
        self.list.len() != before
    }

    /// Extend a lease by its job's lease time from now (long jobs call this while they run).
    pub fn renew(&mut self, cfg: &Config, id: &str, now: u64) -> Option<Lease> {
        self.active(now);
        let l = self.list.iter_mut().find(|l| l.id == id)?;
        l.until = now + cfg.jobs.get(&l.job).map(|j| j.lease_seconds).unwrap_or(600);
        Some(l.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &str = r#"
[machines.a]
priority = 1
accel = "cuda"
jobs = { big = 1, many = 2 }
[machines.b]
priority = 2
accel = "cuda"
jobs = { big = 1, many = 1 }
[jobs.big]
mode = "exclusive"
lease_seconds = 100
[jobs.many]
mode = "spread"
lease_seconds = 50
"#;

    #[test]
    fn pick_leases_until_released_or_expired() {
        let cfg = Config::from_toml(CFG).unwrap();
        let mut book = Leases::new();
        let snap = Snapshot::new();
        let (l1, _) = book.pick(&cfg, "big", &snap, 1000).unwrap();
        let l1 = l1.unwrap();
        assert_eq!((l1.machine.as_str(), l1.until), ("a", 1100));
        let (l2, _) = book.pick(&cfg, "big", &snap, 1000).unwrap();
        assert_eq!(l2.unwrap().machine, "b");
        let (none, a) = book.pick(&cfg, "big", &snap, 1000).unwrap();
        assert!(none.is_none() && a.usable.is_empty());
        assert!(book.release(&l1.id));
        assert!(!book.release(&l1.id));
        assert_eq!(book.pick(&cfg, "big", &snap, 1000).unwrap().0.unwrap().machine, "a");
        // all leases run out at 1100
        assert_eq!(book.active(1100).len(), 0);
    }

    #[test]
    fn spread_and_renew() {
        let cfg = Config::from_toml(CFG).unwrap();
        let mut book = Leases::new();
        let snap = Snapshot::new();
        let (ls, _) = book.spread(&cfg, "many", 5, &snap, 0).unwrap();
        assert_eq!(ls.iter().map(|l| l.machine.as_str()).collect::<Vec<_>>(), vec!["a", "a", "b"]);
        assert!(book.spread(&cfg, "many", 1, &snap, 10).unwrap().0.is_empty(), "all slots leased");
        let renewed = book.renew(&cfg, &ls[0].id, 40).unwrap();
        assert_eq!(renewed.until, 90);
        assert_eq!(book.active(60).len(), 1, "the renewed one is still there");
        assert!(book.renew(&cfg, "nope", 60).is_none());
        // the exclusive job cannot take a while a spread lease... it can: spread leases hold their own slots only
        assert_eq!(book.pick(&cfg, "big", &snap, 60).unwrap().0.unwrap().machine, "a");
    }

    #[test]
    fn holds_by_hand() {
        let cfg = Config::from_toml(CFG).unwrap();
        let mut book = Leases::new();
        let snap = Snapshot::new();
        // a held for every job until 100: pick goes to b, with the reason
        let h = book.hold(&cfg, "a", None, Some(100), "playing", 0).unwrap();
        let (l, a) = book.pick(&cfg, "big", &snap, 10).unwrap();
        assert_eq!(l.unwrap().machine, "b");
        assert!(a.rejected.iter().any(|r| r.machine == "a" && r.reasons == vec!["held by hand until 100: playing".to_string()]));
        assert!(book.unhold(&h.id));
        // a hold for one job leaves the others (a new book: the exclusive lease above holds b for every job)
        let mut book = Leases::new();
        book.hold(&cfg, "a", Some("many"), None, "", 10).unwrap();
        assert_eq!(book.pick(&cfg, "big", &snap, 20).unwrap().0.unwrap().machine, "a", "big is not held");
        let mut book = Leases::new();
        book.hold(&cfg, "a", Some("many"), None, "", 10).unwrap();
        let (ls, _) = book.spread(&cfg, "many", 5, &snap, 20).unwrap();
        assert_eq!(ls.iter().map(|l| l.machine.as_str()).collect::<Vec<_>>(), vec!["b"]);
        assert!(book.hold(&cfg, "nope", None, None, "", 0).is_err());
        assert!(book.hold(&cfg, "a", Some("nope"), None, "", 0).is_err());
        assert!(book.hold(&cfg, "a", None, Some(5), "", 10).is_err(), "ends before it starts");
        // a timed hold runs out by itself
        let mut book = Leases::new();
        book.hold(&cfg, "a", None, Some(100), "", 0).unwrap();
        assert_eq!(book.holds(99).len(), 1);
        assert_eq!(book.holds(100).len(), 0);
        assert!(!book.unhold("nope"));
    }

    #[test]
    fn leases_carry_the_job_endpoint() {
        let cfg = Config::from_toml(r#"
[machines.a]
priority = 1
accel = "cuda"
jobs = { many = 1 }
endpoints = { llm = "http://a:11434/v1", other = "http://a:1" }
[jobs.many]
mode = "spread"
endpoint = "llm"
"#).unwrap();
        let (ls, a) = Leases::new().spread(&cfg, "many", 1, &Snapshot::new(), 0).unwrap();
        assert_eq!(ls[0].endpoint.as_deref(), Some("http://a:11434/v1"));
        assert_eq!(a.usable[0].endpoint.as_deref(), Some("http://a:11434/v1"));
        // a job naming an endpoint a machine with a slot lacks is a configuration error
        let bad = Config::from_toml(r#"
[machines.a]
priority = 1
accel = "cuda"
jobs = { many = 1 }
[jobs.many]
mode = "spread"
endpoint = "llm"
"#);
        assert!(bad.unwrap_err().to_string().contains("does not have"));
    }
}
