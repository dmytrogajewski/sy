use std::process::Command;

use anyhow::Result;
use serde_json::Value;

use super::{CleanStrategy, Outcome, Probe};

pub struct Podman;

impl CleanStrategy for Podman {
    fn id(&self) -> &'static str {
        "podman"
    }
    fn label(&self) -> &'static str {
        "podman prune"
    }
    fn description(&self) -> &'static str {
        "remove stopped containers + unused images/volumes"
    }
    fn available(&self) -> bool {
        crate::which("podman")
    }
    fn probe(&self) -> Result<Probe> {
        let out = Command::new("podman")
            .args(["system", "df", "--format", "json"])
            .output();
        let Ok(o) = out else {
            return Ok(Probe::default());
        };
        let v: Value = serde_json::from_slice(&o.stdout).unwrap_or(Value::Null);
        let total = sum_reclaimable(&v);
        Ok(Probe {
            reclaimable: total,
            items: vec!["podman system df reclaimable".into()],
        })
    }
    fn apply(&self, _probe: &Probe) -> Result<Outcome> {
        let before = self.probe().map(|p| p.reclaimable).unwrap_or(0);
        let _ = Command::new("podman")
            .args(["system", "prune", "-af", "--volumes"])
            .status();
        let after = self.probe().map(|p| p.reclaimable).unwrap_or(0);
        Ok(Outcome {
            reclaimed: before.saturating_sub(after),
            log: vec!["podman system prune -af --volumes".into()],
        })
    }
}

fn sum_reclaimable(v: &Value) -> u64 {
    let mut total = 0u64;
    let arr = match v {
        Value::Array(a) => a.clone(),
        // Some podman versions return an object with a "Reclaimable" field.
        _ => return 0,
    };
    for item in arr {
        if let Some(n) = item.get("Reclaimable").and_then(|x| x.as_u64()) {
            total += n;
        } else if let Some(s) = item.get("Reclaimable").and_then(|x| x.as_str()) {
            total += parse_size(s);
        }
    }
    total
}

// Parse podman's human size (e.g. "1.234GB", "0B").
fn parse_size(s: &str) -> u64 {
    let t = s.trim();
    let (num_part, suf_part): (String, String) =
        t.chars().partition(|c| c.is_ascii_digit() || *c == '.');
    let n: f64 = num_part.parse().unwrap_or(0.0);
    let mult: f64 = match suf_part.trim().to_ascii_lowercase().as_str() {
        "b" | "" => 1.0,
        "kb" | "k" => 1024.0,
        "mb" | "m" => 1024.0 * 1024.0,
        "gb" | "g" => 1024.0 * 1024.0 * 1024.0,
        "tb" | "t" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    };
    (n * mult) as u64
}
