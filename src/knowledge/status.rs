//! Knowledge-plane status snapshot.
//!
//! As of the aiplane refactor the `Status` shape + the
//! `$XDG_STATE_HOME/sy/aiplane/status.json` writer live in
//! `crate::aiplane::status`; this module re-exports them so daemon
//! and CLI keep importing `status::Status`, `status::save`,
//! `status::WAYBAR_SIGNAL_OFFSET`, etc. unchanged.

pub use crate::aiplane::status::*;
