//! Structured error codes carried on the IPC v1 wire (SPEC §4.2)
//! and mapped to stable process exit codes (SPEC §4.7). Each variant
//! is a contract with both CLI consumers (humans + agents) and the
//! scheduler / sandbox / supervisor layers that emit them.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Wire-stable error discriminator. Lives in `sy-core` so daemons
/// and clients agree on the shape without forming a dep cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ErrorCode {
    /// Scheduler queue is full for this class. Response carries
    /// `retry_after_ms` per SPEC §4.2 example.
    Overloaded,
    /// Caller (or a higher-priority preempt) cancelled the
    /// in-flight request. See SPEC §4.2 cancellation pattern.
    Cancelled,
    /// `deadline_ms` elapsed before the worker could yield a
    /// result. Distinct from `Cancelled` so callers can decide
    /// whether to retry with a longer deadline.
    Timeout,
    /// Sandbox policy refused the exec / read / write / network
    /// access. Audit log carries the matched rule (SPEC §4.4).
    PolicyDenied,
    /// Sandbox needs a TTY consent before this tool call can
    /// proceed. Response carries an opaque approval token
    /// (`sy approve <token>`).
    ConsentRequired,
    /// `schema_version` on the request doesn't match what this
    /// daemon speaks. No backward-compat for unversioned IPC per
    /// SPEC §3.4 anti-goal.
    IncompatibleSchema,
    /// Daemon is starting / draining and can't serve yet.
    /// Distinct from `Overloaded` so callers can poll.
    NotReady,
    /// NPU device missing or in a failed state and the caller's
    /// `Priority` class refuses CPU fallback (SPEC §4.3
    /// "Realtime: refuse fallback").
    NpuUnavailable,
    /// Unrecoverable daemon-side error. Maps to CLI exit 1
    /// (SPEC §4.7 "generic error").
    Internal,
    /// Malformed request — bad params, unknown method, etc. Maps
    /// to CLI exit 2 (SPEC §4.7 "usage error").
    BadRequest,
}

impl ErrorCode {
    /// Stable enumeration order. Used by `sy ipc describe` and by
    /// the doctor / status surfaces that report which codes a
    /// daemon may emit.
    pub const ALL: [ErrorCode; 10] = [
        ErrorCode::Overloaded,
        ErrorCode::Cancelled,
        ErrorCode::Timeout,
        ErrorCode::PolicyDenied,
        ErrorCode::ConsentRequired,
        ErrorCode::IncompatibleSchema,
        ErrorCode::NotReady,
        ErrorCode::NpuUnavailable,
        ErrorCode::Internal,
        ErrorCode::BadRequest,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Overloaded => "Overloaded",
            ErrorCode::Cancelled => "Cancelled",
            ErrorCode::Timeout => "Timeout",
            ErrorCode::PolicyDenied => "PolicyDenied",
            ErrorCode::ConsentRequired => "ConsentRequired",
            ErrorCode::IncompatibleSchema => "IncompatibleSchema",
            ErrorCode::NotReady => "NotReady",
            ErrorCode::NpuUnavailable => "NpuUnavailable",
            ErrorCode::Internal => "Internal",
            ErrorCode::BadRequest => "BadRequest",
        }
    }

    /// Map to the SPEC §4.7 stable process exit code. Multiple
    /// error variants can map to the same exit (e.g. `Internal` /
    /// `Timeout` both surface as exit 1) — the wire code keeps
    /// the finer distinction; exit codes are the CLIG-stable
    /// coarse signal for shell consumers.
    pub fn exit_code(self) -> i32 {
        match self {
            // 1 generic error
            ErrorCode::Internal | ErrorCode::Timeout | ErrorCode::Cancelled => 1,
            // 2 usage error
            ErrorCode::BadRequest | ErrorCode::IncompatibleSchema => 2,
            // 4 not ready
            ErrorCode::NotReady | ErrorCode::NpuUnavailable => 4,
            // 5 overloaded / rate-limited
            ErrorCode::Overloaded => 5,
            // 6 consent required
            ErrorCode::ConsentRequired => 6,
            // 7 policy denied
            ErrorCode::PolicyDenied => 7,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ErrorCode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        for c in ErrorCode::ALL {
            if s == c.as_str() {
                return Ok(c);
            }
        }
        anyhow::bail!(
            "unknown error code {s:?}; one of {:?}",
            ErrorCode::ALL.map(|c| c.as_str())
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_pascal_case_wire() {
        assert_eq!(
            serde_json::to_string(&ErrorCode::Overloaded).expect("serialize"),
            "\"Overloaded\""
        );
        // SPEC §4.2 example uses `"code": "Overloaded"`; a snake-case
        // regression would invalidate every example in the doc and
        // every audit log line.
        assert_ne!(
            serde_json::to_string(&ErrorCode::Overloaded).expect("serialize"),
            "\"overloaded\""
        );
    }

    #[test]
    fn error_code_round_trip_all_variants() {
        for c in ErrorCode::ALL {
            let j = serde_json::to_string(&c).expect("serialize");
            let back: ErrorCode = serde_json::from_str(&j).expect("deserialize");
            assert_eq!(back, c);
        }
    }

    #[test]
    fn error_code_all_listed_count_matches_spec() {
        // SPEC §3.3 / §4.2 / §4.7 enumerate ten codes. Guard the
        // count so a typo on a future addition doesn't silently
        // shrink the surface.
        assert_eq!(ErrorCode::ALL.len(), 10);
    }

    #[test]
    fn error_code_from_str_round_trips_via_as_str() {
        for c in ErrorCode::ALL {
            assert_eq!(c.as_str().parse::<ErrorCode>().expect("parse"), c);
        }
    }

    #[test]
    fn error_code_from_str_rejects_unknown() {
        assert!("Bogus".parse::<ErrorCode>().is_err());
    }

    #[test]
    fn error_code_exit_codes_match_spec_table() {
        // SPEC §4.7 stable exit codes:
        //   1 generic / 2 usage / 4 not ready / 5 overloaded /
        //   6 consent / 7 policy denied. 0 is success; 3 is doctor-
        //   drift, not a wire-side error.
        assert_eq!(ErrorCode::Internal.exit_code(), 1);
        assert_eq!(ErrorCode::Timeout.exit_code(), 1);
        assert_eq!(ErrorCode::Cancelled.exit_code(), 1);
        assert_eq!(ErrorCode::BadRequest.exit_code(), 2);
        assert_eq!(ErrorCode::IncompatibleSchema.exit_code(), 2);
        assert_eq!(ErrorCode::NotReady.exit_code(), 4);
        assert_eq!(ErrorCode::NpuUnavailable.exit_code(), 4);
        assert_eq!(ErrorCode::Overloaded.exit_code(), 5);
        assert_eq!(ErrorCode::ConsentRequired.exit_code(), 6);
        assert_eq!(ErrorCode::PolicyDenied.exit_code(), 7);
    }

    #[test]
    fn error_code_no_variant_maps_to_zero_exit() {
        // Exit 0 is reserved for success — an `ErrorCode` ever
        // mapping to it would smuggle a failure through `$?`.
        for c in ErrorCode::ALL {
            assert_ne!(c.exit_code(), 0, "{c} mapped to exit 0");
        }
    }
}
