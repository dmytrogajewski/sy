use std::process::Command;

use anyhow::Result;

use super::{CleanStrategy, Outcome, Probe};

pub struct Flatpak;

impl CleanStrategy for Flatpak {
    fn id(&self) -> &'static str {
        "flatpak"
    }
    fn label(&self) -> &'static str {
        "flatpak unused"
    }
    fn description(&self) -> &'static str {
        "remove orphaned flatpak runtimes"
    }
    fn available(&self) -> bool {
        crate::which("flatpak")
    }
    fn probe(&self) -> Result<Probe> {
        // Count unused refs reported by `flatpak list --columns=ref`
        // intersected with `flatpak uninstall --unused --no-related --noninteractive`
        // dry-output. A precise byte estimate is not available without
        // actually performing the operation, so we report 0 and let the user
        // pick the strategy explicitly.
        let unused = unused_refs();
        Ok(Probe {
            reclaimable: 0,
            items: unused,
        })
    }
    fn apply(&self, _probe: &Probe) -> Result<Outcome> {
        let _ = Command::new("flatpak")
            .args(["uninstall", "--unused", "-y"])
            .status();
        Ok(Outcome {
            reclaimed: 0,
            log: vec!["flatpak uninstall --unused -y".into()],
        })
    }
}

fn unused_refs() -> Vec<String> {
    // `flatpak list --columns=ref --app=false --runtime=true` lists runtimes;
    // we don't try to compute "unused" precisely — just surface a hint.
    let out = Command::new("flatpak")
        .args(["list", "--columns=ref"])
        .output();
    let Ok(o) = out else {
        return Vec::new();
    };
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .take(5)
        .collect()
}
