//! tasukeai-core: decide which machine should take a piece of work now, among machines that also have other lives
//! (games, travel, heat). Configuration, rules, the three modes (exclusive, spread, rank) and the reasons behind each
//! decision. The `tasukeai` binary adds HTTP, a CLI and shared leases on top; see docs/design.md.

pub mod collect;
pub mod config;
pub mod decide;

pub use collect::{collect, Collected};
pub use config::{Check, Config, ConfigError, Job, Machine, Metrics, Mode, Rule};
pub use decide::{assess, pick, rank, spread, Assessment, Candidate, DecideError, Lease, Rejected, Signals, Snapshot};

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &str = r#"
stale_after_seconds = 120

[machines.desk]
priority = 1
accel = "cuda"
vram_gb = 12
ram_gb = 96
rules = [{ signal = "gpu_temp_celsius", max = 80, because = "hot" },
         { signal = "idle_seconds", min = 600, because = "someone is using it" }]
jobs = { train = 1, extract = 2, chat = 1 }

[machines.mini]
priority = 2
accel = "metal"
ram_gb = 16
rules = [{ signal = "thermal_state", max = 1 }]
jobs = { extract = 1, chat = 1 }

[machines.laptop]
priority = 4
accel = "metal"
ram_gb = 16
rules = [{ signal = "on_ac", min = 1, because = "on battery" }]
jobs = { extract = 1, chat = 1 }

[jobs.train]
mode = "exclusive"
accel = ["cuda"]
vram_gb = 11
lease_seconds = 1800

[jobs.extract]
mode = "spread"

[jobs.chat]
mode = "rank"
"#;

    const NOW: u64 = 10_000;

    fn snap(entries: &[(&str, &str, f64)]) -> Snapshot {
        let mut s = Snapshot::new();
        for (m, k, v) in entries {
            s.entry(m.to_string()).or_default().insert(k.to_string(), (*v, NOW - 10));
        }
        s
    }

    fn all_fine() -> Snapshot {
        snap(&[("desk", "gpu_temp_celsius", 50.0), ("desk", "idle_seconds", 3600.0),
               ("mini", "thermal_state", 0.0), ("laptop", "on_ac", 1.0)])
    }

    #[test]
    fn exclusive_takes_the_first_and_respects_leases() {
        let cfg = Config::from_toml(CFG).unwrap();
        let (m, a) = pick(&cfg, "train", &all_fine(), &[], NOW).unwrap();
        assert_eq!(m.as_deref(), Some("desk"));
        // mini cannot train: not installed, metal, no VRAM
        let mini = a.rejected.iter().find(|r| r.machine == "mini").unwrap();
        assert!(mini.reasons.iter().any(|r| r.contains("not installed")));
        assert!(mini.reasons.iter().any(|r| r.contains("needs cuda")));
        let lease = Lease { id: "l1".into(), machine: "desk".into(), job: "train".into(), until: NOW + 100 };
        let (m, a) = pick(&cfg, "train", &all_fine(), &[lease.clone()], NOW).unwrap();
        assert_eq!(m, None);
        assert!(a.rejected[0].reasons.iter().any(|r| r.contains("leased")));
        // an expired lease does not count
        let old = Lease { until: NOW - 1, ..lease };
        assert_eq!(pick(&cfg, "train", &all_fine(), &[old], NOW).unwrap().0.as_deref(), Some("desk"));
    }

    #[test]
    fn owner_rules_give_reasons() {
        let cfg = Config::from_toml(CFG).unwrap();
        let s = snap(&[("desk", "gpu_temp_celsius", 85.0), ("desk", "idle_seconds", 30.0)]);
        let (m, a) = pick(&cfg, "train", &s, &[], NOW).unwrap();
        assert_eq!(m, None);
        let desk = &a.rejected.iter().find(|r| r.machine == "desk").unwrap().reasons;
        assert!(desk.contains(&"gpu_temp_celsius = 85 > 80 (hot)".to_string()));
        assert!(desk.contains(&"idle_seconds = 30 < 600 (someone is using it)".to_string()));
    }

    #[test]
    fn unknown_and_stale_signals_fail() {
        let cfg = Config::from_toml(CFG).unwrap();
        let mut s = all_fine();
        s.get_mut("laptop").unwrap().insert("on_ac".into(), (1.0, NOW - 500));
        s.remove("mini");
        let (order, a) = rank(&cfg, "chat", &s, NOW).unwrap();
        assert_eq!(order, vec!["desk"]);
        let reasons = |m: &str| a.rejected.iter().find(|r| r.machine == m).unwrap().reasons.join("; ");
        assert!(reasons("laptop").contains("stale (500 s old)"));
        assert!(reasons("mini").contains("thermal_state unknown"));
    }

    #[test]
    fn spread_fills_slots_in_priority_order() {
        let cfg = Config::from_toml(CFG).unwrap();
        let (got, _) = spread(&cfg, "extract", 3, &all_fine(), &[], NOW).unwrap();
        assert_eq!(got, vec!["desk", "desk", "mini"]);
        let (got, _) = spread(&cfg, "extract", 10, &all_fine(), &[], NOW).unwrap();
        assert_eq!(got, vec!["desk", "desk", "mini", "laptop"]);
        let busy = Lease { id: "x".into(), machine: "desk".into(), job: "extract".into(), until: NOW + 60 };
        let (got, _) = spread(&cfg, "extract", 10, &all_fine(), &[busy], NOW).unwrap();
        assert_eq!(got, vec!["desk", "mini", "laptop"]);
    }

    #[test]
    fn an_exclusive_lease_holds_the_whole_machine() {
        let cfg = Config::from_toml(CFG).unwrap();
        let train = Lease { id: "t".into(), machine: "desk".into(), job: "train".into(), until: NOW + 60 };
        let (got, a) = spread(&cfg, "extract", 10, &all_fine(), &[train.clone()], NOW).unwrap();
        assert_eq!(got, vec!["mini", "laptop"]);
        assert!(a.rejected[0].reasons.contains(&"held by train (exclusive)".to_string()));
        // a spread lease does not hold the exclusive job's slot, only its own
        let ex = Lease { id: "e".into(), machine: "desk".into(), job: "extract".into(), until: NOW + 60 };
        assert_eq!(pick(&cfg, "train", &all_fine(), &[ex], NOW).unwrap().0.as_deref(), Some("desk"));
        let _ = train;
    }

    #[test]
    fn modes_and_config_are_checked() {
        let cfg = Config::from_toml(CFG).unwrap();
        assert!(matches!(pick(&cfg, "chat", &all_fine(), &[], NOW), Err(DecideError::WrongMode { .. })));
        assert!(matches!(rank(&cfg, "nope", &all_fine(), NOW), Err(DecideError::UnknownJob(_))));
        let bad = CFG.replace("jobs = { extract = 1, chat = 1 }\n\n[machines.laptop]", "jobs = { paint = 1 }\n\n[machines.laptop]");
        assert!(Config::from_toml(&bad).unwrap_err().to_string().contains("unknown job paint"));
    }
}
