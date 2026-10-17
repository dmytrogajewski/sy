//! Shared IPC types, framing, clients, and servers for the sy workspace.
//! `envelope` carries IPC v1; `aiplane` carries legacy workload payloads
//! used by the knowledge bridge. Neither payload module depends on a daemon.

pub mod aiplane;
pub mod blocking;
pub mod cancel;
pub mod client;
pub mod codec;
pub mod envelope;
pub mod knowledge;
pub mod paths;
pub mod reserved;
pub mod server;
pub mod stream;

pub use cancel::{CancelGuard, CancelRegistry, Dispatched, dispatch_with_cancel};
pub use client::{CallOpts, Client};
pub use codec::{RequestCodec, ResponseCodec};
pub use envelope::{
    BlobKind, BlobRef, ErrorBody, ParseRequestError, Request, Response, SCHEMA_VERSION, SpanId,
    TraceId, parse_request_strict,
};
pub use reserved::{
    BuildInfo, Capabilities, HealthFn, HealthSnapshot, HealthState, MON_SECONDS_MAX,
    MON_SECONDS_MIN, MonHistoryParams, MonSnapshotParams, MonSubscribeParams, PROTOCOL_VERSION,
    SYSTEM_METHODS, SystemMethods,
};
pub use server::{Handler, PeerAuthorizer, PeerCredentials, SameEuidAuthorizer, Server};
pub use stream::{Event, EventCodec, KIND_CLOSED};
