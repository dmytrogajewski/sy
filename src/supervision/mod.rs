// arch-supervision (`specs/roadmaps/arch-supervision/ROADMAP.md`):
//   * Step 2 — `apply`: `sy apply` syncs `systemd --user` unit files.
//   * Step 3 — `service` / `status` / `logs`: `sy service
//     start|stop|restart|status|enable|disable|logs` per SPEC §4.7.
//   * Step 4 — `notify` plumbing (sd_notify) lands later.

pub mod apply;
pub mod logs;
pub mod service;
pub mod status;

/// sy-mon Step 20: emit one tick's worth of
/// `sy_supervisor_plane_state{plane, state}` gauges. Re-export of
/// [`sy_core::sensors::supervisor::emit_plane_state`] so the
/// binary's call sites can use the historical
/// `crate::supervision::*` path without dragging the sensor crate
/// into every site.
pub use sy_core::sensors::supervisor::emit_plane_state;
