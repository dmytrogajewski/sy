//! QoS priority class declared by every IPC caller. Wire-stable
//! identifier consumed by the aiplane scheduler (SPEC §4.3 four-class
//! strict-priority dispatcher) and the IPC v1 envelope (SPEC §4.2).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Caller-declared scheduling tier. Names are wire-stable —
/// renaming a variant is a breaking change for IPC clients and
/// the on-disk audit log schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Priority {
    /// Sub-frame budgets (VAD, eye-track). Per SPEC §4.3, the
    /// scheduler refuses to queue these once the per-class cap is
    /// hit rather than absorb latency.
    Realtime,
    /// Foreground user-driven work (STT live, search). Default
    /// class for CLI and MCP surfaces.
    Interactive,
    /// Async pipelines that should yield to interactive load —
    /// embed passes, rerank, OCR.
    Background,
    /// Bulk reprocessing — KB rebuilds, bulk ingestion. No
    /// deadline; gets the bottom of the dispatcher.
    Batch,
}

impl Priority {
    /// Stable order: highest → lowest. Used by the scheduler's
    /// strict-priority dispatcher and by CLI `--help` output.
    pub const ALL: [Priority; 4] = [
        Priority::Realtime,
        Priority::Interactive,
        Priority::Background,
        Priority::Batch,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Priority::Realtime => "Realtime",
            Priority::Interactive => "Interactive",
            Priority::Background => "Background",
            Priority::Batch => "Batch",
        }
    }
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Priority {
    type Err = anyhow::Error;

    /// Case-sensitive — PascalCase is the canonical form per SPEC
    /// §4.7. Lowercase / kebab-case input rejects with a helpful
    /// message so a typo on `--priority interactive` surfaces at
    /// the CLI edge instead of silently defaulting.
    fn from_str(s: &str) -> anyhow::Result<Self> {
        for p in Priority::ALL {
            if s == p.as_str() {
                return Ok(p);
            }
        }
        anyhow::bail!(
            "unknown priority {s:?}; one of {:?}",
            Priority::ALL.map(|p| p.as_str())
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_round_trip() {
        for p in Priority::ALL {
            let j = serde_json::to_string(&p).expect("serialize");
            let back: Priority = serde_json::from_str(&j).expect("deserialize");
            assert_eq!(back, p);
        }
    }

    #[test]
    fn priority_pascal_case_on_wire() {
        assert_eq!(
            serde_json::to_string(&Priority::Realtime).expect("serialize"),
            "\"Realtime\""
        );
        assert_eq!(
            serde_json::to_string(&Priority::Interactive).expect("serialize"),
            "\"Interactive\""
        );
        assert_eq!(
            serde_json::to_string(&Priority::Background).expect("serialize"),
            "\"Background\""
        );
        assert_eq!(
            serde_json::to_string(&Priority::Batch).expect("serialize"),
            "\"Batch\""
        );
    }

    #[test]
    fn priority_from_str_is_case_sensitive_pascal() {
        assert_eq!(
            "Interactive".parse::<Priority>().expect("parse"),
            Priority::Interactive
        );
        // Lowercase, kebab-case, and screaming-snake are NOT accepted —
        // PascalCase is the canonical form per SPEC §4.7.
        assert!("interactive".parse::<Priority>().is_err());
        assert!("inter-active".parse::<Priority>().is_err());
        assert!("INTERACTIVE".parse::<Priority>().is_err());
    }

    #[test]
    fn priority_from_str_round_trips_via_as_str() {
        for p in Priority::ALL {
            assert_eq!(p.as_str().parse::<Priority>().expect("parse"), p);
        }
    }

    #[test]
    fn priority_all_has_four_entries() {
        // Spec §3.2 K3 and §4.3 commit to exactly four classes.
        // A fifth class would need a schema bump + scheduler caps
        // table update — guard the count.
        assert_eq!(Priority::ALL.len(), 4);
    }

    #[test]
    fn priority_display_matches_as_str() {
        for p in Priority::ALL {
            assert_eq!(format!("{p}"), p.as_str());
        }
    }
}
