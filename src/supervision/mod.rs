// arch-supervision (`specs/roadmaps/arch-supervision/ROADMAP.md`):
//   * Step 2 — `apply`: `sy apply` syncs `systemd --user` unit files.
//   * Step 3 — `service` / `status` / `logs`: `sy service
//     start|stop|restart|status|enable|disable|logs` per SPEC §4.7.
//   * Step 4 — `notify` plumbing (sd_notify) lands later.

pub mod apply;
pub mod logs;
pub mod service;
pub mod status;
