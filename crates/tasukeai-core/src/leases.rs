//! Leases: short reservations of machines handed out by `exclusive` and `spread`, so that the next decision (before
//! the metrics show the new load) does not hand the same machine out again. In memory; the process keeps one book for
//! all its callers, a library caller keeps its own.

use crate::config::Config;
use crate::decide::{pick, spread, Assessment, DecideError, Lease, Snapshot};

#[derive(Debug, Default)]
pub struct Leases {
    list: Vec<Lease>,
    counter: u64,
}

impl Leases {
    pub fn new() -> Self {
        Self::default()
    }

    /// The leases that have not run out (and forget the others).
    pub fn active(&mut self, now: u64) -> &[Lease] {
        self.list.retain(|l| l.until > now);
        &self.list
    }

    fn take(&mut self, cfg: &Config, job: &str, machine: &str, now: u64) -> Lease {
        self.counter += 1;
        let seconds = cfg.jobs.get(job).map(|j| j.lease_seconds).unwrap_or(600);
        let lease = Lease { id: format!("{now:x}-{:x}", self.counter), machine: machine.into(), job: job.into(), until: now + seconds };
        self.list.push(lease.clone());
        lease
    }

    /// `exclusive`: decide and lease in one step (the caller holds the book while doing it, so two callers cannot
    /// get the same machine).
    pub fn pick(&mut self, cfg: &Config, job: &str, snap: &Snapshot, now: u64) -> Result<(Option<Lease>, Assessment), DecideError> {
        let (m, a) = pick(cfg, job, snap, self.active(now), now)?;
        Ok((m.map(|m| self.take(cfg, job, &m, now)), a))
    }

    /// `spread`: up to `count` leases, one per slot given.
    pub fn spread(&mut self, cfg: &Config, job: &str, count: u32, snap: &Snapshot, now: u64) -> Result<(Vec<Lease>, Assessment), DecideError> {
        let (ms, a) = spread(cfg, job, count, snap, self.active(now), now)?;
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
}
