//! sy-ipc: IPC v1 envelope + framing for the sy workspace.
//!
//! Step 1 of `specs/roadmaps/arch-ipc-v1/ROADMAP.md` — pure types +
//! codec, no I/O, no daemon plumbing. The wire shape lives in
//! `envelope.rs` and matches SPEC §4.2 byte-for-byte; the framing
//! lives in `codec.rs` and wraps `tokio_util::codec::LengthDelimitedCodec`
//! per the same SPEC section.
//!
//! Server/Client + reserved methods land in Steps 2 and 3; consumers
//! (knowledge, aiplane, agt, stack) migrate in Steps 4–6.

pub mod blocking;
pub mod cancel;
pub mod client;
pub mod codec;
pub mod envelope;
pub mod paths;
pub mod reserved;
pub mod server;
pub mod stream;

pub use cancel::{dispatch_with_cancel, CancelGuard, CancelRegistry, Dispatched};
pub use client::{CallOpts, Client};
pub use codec::{RequestCodec, ResponseCodec};
pub use envelope::{
    parse_request_strict, BlobKind, BlobRef, ErrorBody, ParseRequestError, Request, Response,
    SpanId, TraceId, SCHEMA_VERSION,
};
pub use reserved::{
    BuildInfo, Capabilities, HealthFn, HealthSnapshot, HealthState, SystemMethods,
    PROTOCOL_VERSION, SYSTEM_METHODS,
};
pub use server::{Handler, Server};
pub use stream::{Event, EventCodec, KIND_CLOSED};
