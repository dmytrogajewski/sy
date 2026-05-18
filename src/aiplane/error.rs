//! Scheduler / dispatcher error vocabulary (SPEC §4.3 / ROADMAP
//! arch-aiplane-scheduler Step 2). One step removed from the v1
//! wire-level [`sy_core::ErrorCode`]: this enum tracks daemon-local
//! context (which class overloaded, what payload failed); the IPC
//! bridge serialises it into a [`sy_ipc::ErrorBody`] before it hits
//! the socket.
//!
//! `Timeout` and `NpuUnavailable` ship in later steps when the
//! per-queue deadline check (Step 2 follow-on) and the CPU-fallback
//! refusal (Step 3) gain real producers.

use sy_core::{ErrorCode, Priority};

#[derive(Debug)]
pub enum AiplaneError {
    /// Per-class queue depth hit the SPEC §4.3 cap and the
    /// `ModelQueuePolicy` is `Reject`. The caller backs off and
    /// retries after `retry_after_ms`.
    Overloaded {
        class: Priority,
        queue_depth: usize,
        retry_after_ms: u64,
    },
    /// `system.cancel` arrived, or the dispatcher pulled a request
    /// whose `CancellationToken` had already been tripped (e.g.
    /// caller dropped the future before it reached the head of the
    /// queue).
    Cancelled,
    /// Workload-side failure (ORT, malformed input, …). Carries the
    /// underlying anyhow chain for diagnostic context.
    WorkloadFailed(anyhow::Error),
}

impl AiplaneError {
    /// Map to the wire-stable error code per SPEC §4.2.
    pub fn wire_code(&self) -> ErrorCode {
        match self {
            AiplaneError::Overloaded { .. } => ErrorCode::Overloaded,
            AiplaneError::Cancelled => ErrorCode::Cancelled,
            AiplaneError::WorkloadFailed(_) => ErrorCode::Internal,
        }
    }

    /// `retry_after_ms` hint for the wire envelope. Only `Overloaded`
    /// surfaces a value; other variants return `None` so the IPC
    /// bridge can elide the field.
    pub fn retry_after_ms(&self) -> Option<u64> {
        match self {
            AiplaneError::Overloaded { retry_after_ms, .. } => Some(*retry_after_ms),
            _ => None,
        }
    }
}

impl std::fmt::Display for AiplaneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AiplaneError::Overloaded {
                class,
                queue_depth,
                retry_after_ms,
            } => write!(
                f,
                "queue overloaded for class={class:?} depth={queue_depth} retry_after={retry_after_ms}ms"
            ),
            AiplaneError::Cancelled => f.write_str("cancelled"),
            AiplaneError::WorkloadFailed(e) => write!(f, "workload failed: {e}"),
        }
    }
}

impl std::error::Error for AiplaneError {}

#[cfg(test)]
mod tests {
    use super::*;
    use sy_ipc::ErrorBody;

    #[test]
    fn overloaded_carries_retry_after() {
        // SPEC §4.2 example response for an Overloaded daemon carries
        // both an `ErrorCode::Overloaded` and a `retry_after_ms`. The
        // scheduler-local `AiplaneError` must hand both to the IPC
        // bridge cleanly — otherwise callers can't back off.
        let err = AiplaneError::Overloaded {
            class: Priority::Background,
            queue_depth: 256,
            retry_after_ms: 200,
        };
        let body = ErrorBody {
            code: err.wire_code(),
            message: err.to_string(),
            retry_after_ms: err.retry_after_ms(),
            details: serde_json::Value::Null,
        };
        assert_eq!(body.code, ErrorCode::Overloaded);
        assert_eq!(body.retry_after_ms, Some(200));
        assert!(body.message.contains("Background"));
    }

    #[test]
    fn cancelled_and_workload_failed_have_no_retry_hint() {
        assert!(AiplaneError::Cancelled.retry_after_ms().is_none());
        assert!(AiplaneError::WorkloadFailed(anyhow::anyhow!("oops"))
            .retry_after_ms()
            .is_none());
    }

    #[test]
    fn wire_code_maps_each_variant() {
        assert_eq!(
            AiplaneError::Overloaded {
                class: Priority::Batch,
                queue_depth: 0,
                retry_after_ms: 0,
            }
            .wire_code(),
            ErrorCode::Overloaded
        );
        assert_eq!(AiplaneError::Cancelled.wire_code(), ErrorCode::Cancelled);
        assert_eq!(
            AiplaneError::WorkloadFailed(anyhow::anyhow!("boom")).wire_code(),
            ErrorCode::Internal
        );
    }
}
