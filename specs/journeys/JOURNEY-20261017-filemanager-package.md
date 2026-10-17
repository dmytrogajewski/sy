# Journey: install the file manager without the desktop shell

**Actor:** Fedora user who wants the sy file manager with a different compositor or desktop.
**Goal:** install `sy` and `sy-filemanager`, open files, and use plugin previews without `sy-desktop`.

1. Install matching base and file manager RPMs.
2. Run `sy file --help` and `sy file doctor --json` to inspect local dependencies.
3. Start `sy-file.socket` with the user manager; invoke `sy file ipc state` to activate the daemon.
4. Open `sy file` under Wayland. The Markdown plugin is discovered from the packaged manifest and executed from `/usr/libexec/sy`.
5. If knowledge is absent, filename search still works and the knowledge chip reports the missing daemon.

## Failure paths

- Missing feature RPM: the launcher names `sy-filemanager` and exits 69.
- Missing Wayland session: headless file and plugin CLI operations remain available.
- Missing knowledge daemon: semantic search degrades to filename search within its timeout budget.
