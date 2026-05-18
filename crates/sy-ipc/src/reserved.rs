//! Reserved `system.{describe,health,cancel}` methods every IPC v1
//! daemon answers (SPEC §4.2 "Reserved methods"). Daemons compose
//! [`SystemMethods`] with their own [`crate::Handler`] impl via
//! [`SystemMethods::try_handle`]: if the method namespace is
//! `system.*` the system handler answers, otherwise the daemon's
//! own dispatch takes over.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::cancel::CancelRegistry;
use crate::envelope::{ErrorBody, Request, Response, SCHEMA_VERSION};
use sy_core::{ErrorCode, Priority};

/// IPC v1 protocol version advertised in `system.describe.result.protocol_version`.
/// Bumps in lockstep with [`SCHEMA_VERSION`] for now (the protocol
/// vs. schema split exists per SPEC §4.2 for future capability
/// negotiation but is one-to-one at v1).
pub const PROTOCOL_VERSION: u32 = 1;

/// Build identifier surfaced via `system.describe`. Daemons fill
/// this at boot — the values get baked into release artefacts and
/// `sy doctor` cross-checks them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildInfo {
    pub name: String,
    pub version: String,
    pub git_sha: String,
}

/// Static capability map advertised by `system.describe`. Daemons
/// flip flags as features come online; LSP-style dynamic
/// negotiation is explicitly out of scope (ROADMAP "Out of Scope").
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Capabilities {
    /// `true` once the daemon supports the streaming response shape
    /// introduced in arch-ipc-v1 Step 6 (`sy agentd` event tail).
    pub streaming: bool,
    /// Scheduler priority classes the daemon recognises on the wire.
    /// `Priority::ALL` is the v1 baseline; surface it so clients can
    /// validate `--priority Foo` at submit time.
    pub priority_classes: Vec<Priority>,
}

impl Capabilities {
    /// Default capability map for an IPC v1 daemon: streaming off,
    /// full four-class scheduler vocabulary.
    pub fn baseline() -> Self {
        Self {
            streaming: false,
            priority_classes: Priority::ALL.to_vec(),
        }
    }
}

/// Worker-level health surface returned by `system.health` (SPEC
/// §4.2 "Reserved methods"). The state values match the
/// `WorkloadState`-flavoured vocabulary; status_line is a short
/// operator-friendly description that `sy doctor` and the stack-bar
/// tooltip embed verbatim.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct HealthSnapshot {
    pub state: HealthState,
    pub status_line: String,
    pub queue_depth: u32,
    pub warm_models: Vec<String>,
}

/// Coarse health tier — wire-stable identifiers used by every IPC v1
/// daemon. Matches the SPEC §4.2 example shape (`ready`, `degraded`,
/// `starting`, `failed`).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Ready,
    Degraded,
    Starting,
    Failed,
}

/// Health probe closure type — boxed so the daemon can plug in any
/// `Fn` it wants without leaking concrete types through the
/// `SystemMethods` constructor.
pub type HealthFn = Arc<dyn Fn() -> HealthSnapshot + Send + Sync>;

/// Reserved-method handler shared by every IPC v1 daemon. The
/// daemon constructs one at boot, calls [`Self::try_handle`] from
/// its top-level [`crate::Handler`] impl, and falls through to its
/// own dispatch for non-`system.*` methods.
pub struct SystemMethods {
    build_info: BuildInfo,
    health_fn: HealthFn,
    cancel_registry: Arc<CancelRegistry>,
    capabilities: Capabilities,
    daemon_methods: Vec<String>,
}

impl SystemMethods {
    pub fn new(
        build_info: BuildInfo,
        health_fn: HealthFn,
        cancel_registry: Arc<CancelRegistry>,
        capabilities: Capabilities,
        daemon_methods: Vec<String>,
    ) -> Self {
        Self {
            build_info,
            health_fn,
            cancel_registry,
            capabilities,
            daemon_methods,
        }
    }

    /// Sorted, deduplicated list of every method the daemon
    /// advertises — `system.*` first, then the domain methods.
    pub fn describe_methods(&self) -> Vec<String> {
        let mut out: Vec<String> = SYSTEM_METHODS.iter().map(|s| (*s).to_string()).collect();
        out.extend(self.daemon_methods.iter().cloned());
        out.sort();
        out.dedup();
        out
    }

    /// Dispatch `req` if it's a `system.*` method, returning the
    /// response envelope to send back. Returns `None` for any other
    /// method so the caller's domain handler can take over —
    /// this is how daemons compose [`SystemMethods`] with their own
    /// [`crate::Handler`] impl.
    pub fn try_handle(&self, req: &Request) -> Option<Response> {
        match req.method.as_str() {
            "system.describe" => Some(self.describe(req)),
            "system.health" => Some(self.health(req)),
            "system.cancel" => Some(self.cancel(req)),
            _ => None,
        }
    }

    fn describe(&self, req: &Request) -> Response {
        let result = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "methods": self.describe_methods(),
            "capabilities": self.capabilities,
            "build_info": self.build_info,
        });
        ok(req.request_id, result)
    }

    fn health(&self, req: &Request) -> Response {
        let snapshot = (self.health_fn)();
        match serde_json::to_value(&snapshot) {
            Ok(result) => ok(req.request_id, result),
            Err(e) => err_response(
                req.request_id,
                ErrorCode::Internal,
                format!("serialise health snapshot: {e}"),
            ),
        }
    }

    fn cancel(&self, req: &Request) -> Response {
        #[derive(Deserialize)]
        struct CancelParams {
            target_request_id: Ulid,
        }
        let parsed: Result<CancelParams, _> = serde_json::from_value(req.params.clone());
        let target = match parsed {
            Ok(p) => p.target_request_id,
            Err(e) => {
                return err_response(
                    req.request_id,
                    ErrorCode::BadRequest,
                    format!("system.cancel params: {e}"),
                );
            }
        };
        let cancelled = self.cancel_registry.cancel(target);
        let result = serde_json::json!({
            "target_request_id": target,
            "cancelled": cancelled,
        });
        ok(req.request_id, result)
    }
}

/// Wire-stable list of reserved method names. Exposed so daemons
/// can also serve them as a static array if they prefer.
pub const SYSTEM_METHODS: &[&str] = &["system.describe", "system.health", "system.cancel"];

fn ok(request_id: Ulid, result: serde_json::Value) -> Response {
    Response::Ok {
        schema_version: SCHEMA_VERSION,
        request_id,
        result,
        blob: None,
    }
}

fn err_response(request_id: Ulid, code: ErrorCode, message: String) -> Response {
    Response::Err {
        schema_version: SCHEMA_VERSION,
        request_id,
        error: ErrorBody {
            code,
            message,
            retry_after_ms: None,
            details: serde_json::Value::Null,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use sy_core::Priority;

    const DAEMON_METHOD: &str = "knowledge.search";

    fn build_info() -> BuildInfo {
        BuildInfo {
            name: "test-daemon".into(),
            version: "0.0.0".into(),
            git_sha: "deadbeef".into(),
        }
    }

    fn request(method: &str, params: serde_json::Value) -> Request {
        Request {
            schema_version: SCHEMA_VERSION,
            request_id: Ulid::new(),
            trace_id: None,
            parent_span_id: None,
            deadline_ms: None,
            priority: Priority::Interactive,
            method: method.into(),
            params,
        }
    }

    #[test]
    fn system_describe_lists_methods() {
        // SPEC §4.2: `system.describe.result.methods` must enumerate
        // every method the daemon serves — reserved `system.*` plus
        // the daemon's own. Sorted + deduplicated so clients can
        // binary-search.
        let registry = Arc::new(CancelRegistry::new());
        let snapshot = HealthSnapshot {
            state: HealthState::Ready,
            status_line: "ok".into(),
            queue_depth: 0,
            warm_models: vec![],
        };
        let sys = SystemMethods::new(
            build_info(),
            Arc::new(move || snapshot.clone()),
            registry,
            Capabilities::baseline(),
            vec![DAEMON_METHOD.into()],
        );

        let resp = sys
            .try_handle(&request("system.describe", serde_json::Value::Null))
            .expect("system.describe must be handled");
        match resp {
            Response::Ok { result, .. } => {
                let methods = result["methods"].as_array().expect("methods array");
                let names: Vec<&str> = methods.iter().filter_map(|v| v.as_str()).collect();
                assert!(names.contains(&"system.describe"));
                assert!(names.contains(&"system.health"));
                assert!(names.contains(&"system.cancel"));
                assert!(names.contains(&DAEMON_METHOD));
                assert_eq!(
                    result["protocol_version"].as_u64(),
                    Some(u64::from(PROTOCOL_VERSION))
                );
                let classes = result["capabilities"]["priority_classes"]
                    .as_array()
                    .expect("priority_classes array");
                assert_eq!(classes.len(), Priority::ALL.len());
            }
            other => panic!("expected Response::Ok, got {other:?}"),
        }
    }

    #[test]
    fn system_health_returns_ready_then_degraded() {
        // Flip the health closure's return value between calls; the
        // second `system.health` must reflect the new state — proves
        // the closure is invoked on every call rather than baked at
        // construction time.
        let registry = Arc::new(CancelRegistry::new());
        let degraded = Arc::new(AtomicBool::new(false));
        let degraded_for_fn = Arc::clone(&degraded);
        let health_fn: HealthFn = Arc::new(move || {
            let state = if degraded_for_fn.load(Ordering::SeqCst) {
                HealthState::Degraded
            } else {
                HealthState::Ready
            };
            HealthSnapshot {
                state,
                status_line: format!("{state:?}"),
                queue_depth: 0,
                warm_models: vec![],
            }
        });
        let sys = SystemMethods::new(
            build_info(),
            health_fn,
            registry,
            Capabilities::baseline(),
            vec![],
        );

        let first = extract_state(
            sys.try_handle(&request("system.health", serde_json::Value::Null))
                .expect("first health"),
        );
        assert_eq!(first, HealthState::Ready);

        degraded.store(true, Ordering::SeqCst);
        let second = extract_state(
            sys.try_handle(&request("system.health", serde_json::Value::Null))
                .expect("second health"),
        );
        assert_eq!(second, HealthState::Degraded);
    }

    #[test]
    fn try_handle_returns_none_for_non_system_methods() {
        // Composition contract: the daemon's outer handler relies on
        // `None` to take over for its own namespace. A regression
        // that answered `BadRequest` here would short-circuit every
        // daemon method.
        let sys = SystemMethods::new(
            build_info(),
            Arc::new(|| HealthSnapshot {
                state: HealthState::Ready,
                status_line: "ok".into(),
                queue_depth: 0,
                warm_models: vec![],
            }),
            Arc::new(CancelRegistry::new()),
            Capabilities::baseline(),
            vec![DAEMON_METHOD.into()],
        );
        assert!(sys
            .try_handle(&request(DAEMON_METHOD, serde_json::Value::Null))
            .is_none());
    }

    #[test]
    fn system_cancel_targets_registered_request() {
        // End-to-end: register an id with the cancel registry, then
        // submit a `system.cancel` carrying that id. The reserved
        // handler reads `target_request_id` from params, calls
        // `registry.cancel(target)`, and reports `cancelled: true`.
        let registry = Arc::new(CancelRegistry::new());
        let target = Ulid::from_string("01HXYZ0000000000000000000Z").expect("ulid");
        let _guard = registry.register(target);

        let sys = SystemMethods::new(
            build_info(),
            Arc::new(|| HealthSnapshot {
                state: HealthState::Ready,
                status_line: "ok".into(),
                queue_depth: 0,
                warm_models: vec![],
            }),
            Arc::clone(&registry),
            Capabilities::baseline(),
            vec![],
        );

        let resp = sys
            .try_handle(&request(
                "system.cancel",
                serde_json::json!({ "target_request_id": target }),
            ))
            .expect("system.cancel must be handled");
        match resp {
            Response::Ok { result, .. } => {
                assert_eq!(result["cancelled"], serde_json::Value::Bool(true));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        assert!(
            _guard.token().is_cancelled(),
            "registered request was actually cancelled"
        );
    }

    fn extract_state(resp: Response) -> HealthState {
        match resp {
            Response::Ok { result, .. } => {
                let snap: HealthSnapshot =
                    serde_json::from_value(result).expect("HealthSnapshot deserialise");
                snap.state
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }
}
