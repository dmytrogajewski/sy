// arch-supervision Step 2 (`specs/roadmaps/arch-supervision/ROADMAP.md`):
// `sy apply` symlinks every file under `configs/systemd/user/` into
// `~/.config/systemd/user/`, then runs `systemctl --user daemon-reload`.
//
// Output shapes match SPEC §4.5 / SPEC §4.12 (CLIG):
//
//   * `--dry-run` (alias `--diff`)  — print/return the planned ops, no
//     filesystem writes, no `daemon-reload`.
//   * `--json`                      — emit the `UnitDiff` struct as
//     stable JSON on stdout. Implies dry-run-style read-only inspection
//     when paired with `--dry-run`; otherwise applies, then prints the
//     post-apply diff so an agent can diff before/after.
//   * `--yes`                       — bypass confirmation for two
//     destructive paths: (a) overwriting a regular file at the target
//     symlink path, (b) removing the legacy system-level
//     `/etc/systemd/system/sy-knowledge.service` (SPEC §4.9 migration).
//
// Destructive-op policy: we never `sudo rm` from `sy`. When the legacy
// system-level unit is present, `dry_run=true` always lists it under
// `removed_stale_requires_confirm`; with `dry_run=false && yes=true`
// **and** `uid == 0` we remove it ourselves; otherwise we emit a
// `sudo rm <path>` instruction on stderr and leave the file in place.
// Tests redirect the legacy path to a tempdir via `ApplyOpts.legacy_
// system_path` so they never touch `/etc`.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use walkdir::WalkDir;

/// Default location of the legacy system-level unit that `sy apply`
/// migrates away from in SPEC §4.9. Tests inject a tempdir-backed
/// override via `ApplyOpts.legacy_system_path`.
pub const LEGACY_SYSTEM_UNIT: &str = "/etc/systemd/system/sy-knowledge.service";

/// Options controlling a single `sync_units` invocation.
///
/// The split between `source_dir` / `target_dir` / `legacy_system_path`
/// (path injection) and `dry_run` / `json` / `yes` (CLI flags) keeps
/// the function fully hermetic for tests — no `$HOME`, no `/etc`, no
/// real `systemctl` calls.
#[derive(Debug, Clone)]
pub struct ApplyOpts {
    /// Source directory holding the unit files (typically
    /// `<repo>/configs/systemd/user/`).
    pub source_dir: PathBuf,
    /// Destination directory for the symlinks (typically
    /// `<XDG_CONFIG_HOME>/systemd/user/`).
    pub target_dir: PathBuf,
    /// Path of the legacy system-level unit to migrate away from.
    /// Defaults to `LEGACY_SYSTEM_UNIT`.
    pub legacy_system_path: PathBuf,
    /// Print the diff without writing anything when true.
    pub dry_run: bool,
    /// Skip the interactive confirmation gate for destructive ops.
    pub yes: bool,
    /// Whether to invoke `systemctl --user daemon-reload` after a
    /// successful apply. Production always sets this; tests set it
    /// false to keep `make test` hermetic.
    pub daemon_reload: bool,
}

#[cfg(test)]
impl ApplyOpts {
    /// Test-friendly option set with `dry_run=true`,
    /// `daemon_reload=false`, and no destructive consent. Production
    /// code constructs the struct field-by-field.
    fn for_test(source: PathBuf, target: PathBuf, legacy: PathBuf) -> Self {
        Self {
            source_dir: source,
            target_dir: target,
            legacy_system_path: legacy,
            dry_run: true,
            yes: false,
            daemon_reload: false,
        }
    }
}

/// The shape of `sy apply --json`. Stable; documented inline. Keys
/// are always present (empty arrays when nothing falls into the
/// bucket) so a consumer can rely on `.created` etc. unconditionally.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct UnitDiff {
    /// Symlinks that did not exist at the target and were (or would
    /// be, under `--dry-run`) created.
    pub created: Vec<PathBuf>,
    /// Symlinks pointing at a different source — replaced (or would
    /// be, under `--dry-run`).
    pub updated: Vec<PathBuf>,
    /// Symlinks already pointing at the same source — no-op.
    pub unchanged: Vec<PathBuf>,
    /// Targets that are regular files (not symlinks); overwriting
    /// requires explicit `--yes`.
    pub update_requires_confirm: Vec<PathBuf>,
    /// Legacy system-level units present at `legacy_system_path`.
    /// Removal requires `--yes` **and** uid 0 (otherwise we emit
    /// a `sudo rm` instruction on stderr; see module head comment).
    pub removed_stale_requires_confirm: Vec<PathBuf>,
    /// SPEC §3.2 K5 + §4.5 "BindsTo qdrant": map every source unit
    /// file (basename) → its declared `BindsTo=` targets. Surfaced
    /// in `sy apply --diff --json` so operators / agents can audit
    /// the binding graph at apply time (Step 5 of the
    /// arch-supervision roadmap). `BTreeMap` for stable JSON key
    /// ordering. Units without a `BindsTo=` line are omitted.
    #[serde(default)]
    pub bound_to: BTreeMap<String, Vec<String>>,
}

/// Walk `opts.source_dir`, diff each file against the symlink that
/// would land at `opts.target_dir/<basename>`, and (unless
/// `opts.dry_run`) apply the diff. On a non-dry-run, runs
/// `systemctl --user daemon-reload` afterwards when
/// `opts.daemon_reload` is true.
///
/// Returns the diff so callers can render it (`--json` or human).
pub fn sync_units(opts: &ApplyOpts) -> Result<UnitDiff> {
    if !opts.source_dir.is_dir() {
        return Err(anyhow!(
            "source {} is not a directory",
            opts.source_dir.display()
        ));
    }

    let mut diff = UnitDiff::default();

    for entry in WalkDir::new(&opts.source_dir).min_depth(1).max_depth(1) {
        let entry = entry.context("walk source_dir")?;
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name();
        // Skip the `.gitkeep` sentinel used to keep empty dirs
        // (e.g. `sy.target.wants/`) under version control.
        if name == ".gitkeep" {
            continue;
        }
        classify_one(entry.path(), &opts.target_dir, &mut diff)?;
        collect_binds_to(entry.path(), &mut diff)?;
    }

    if opts.legacy_system_path.exists() {
        diff.removed_stale_requires_confirm
            .push(opts.legacy_system_path.clone());
    }

    if opts.dry_run {
        return Ok(diff);
    }

    apply_diff(opts, &diff)?;

    if opts.daemon_reload {
        run_daemon_reload()?;
    }

    Ok(diff)
}

/// Classify a single source file against its potential target symlink
/// and append the result to the matching `UnitDiff` bucket.
fn classify_one(source: &Path, target_dir: &Path, diff: &mut UnitDiff) -> Result<()> {
    let basename = source
        .file_name()
        .ok_or_else(|| anyhow!("source path has no basename: {}", source.display()))?;
    let dest = target_dir.join(basename);
    let canonical_source = source
        .canonicalize()
        .with_context(|| format!("canonicalize {}", source.display()))?;

    let meta = fs::symlink_metadata(&dest);
    match meta {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            diff.created.push(dest);
        }
        Err(e) => return Err(anyhow::Error::new(e).context(format!("stat {}", dest.display()))),
        Ok(m) if m.file_type().is_symlink() => {
            let link_target =
                fs::read_link(&dest).with_context(|| format!("read_link {}", dest.display()))?;
            // Resolve the link target relative to the symlink's
            // parent so a relative symlink matches the absolute
            // `canonical_source`.
            let resolved = if link_target.is_absolute() {
                link_target
            } else {
                dest.parent()
                    .map(|p| p.join(&link_target))
                    .unwrap_or(link_target)
            };
            let resolved = resolved.canonicalize().unwrap_or(resolved);
            if resolved == canonical_source {
                diff.unchanged.push(dest);
            } else {
                diff.updated.push(dest);
            }
        }
        Ok(_) => {
            // Regular file (or directory) — overwriting needs `--yes`.
            diff.update_requires_confirm.push(dest);
        }
    }
    Ok(())
}

/// Scan a unit file for `BindsTo=` directives in the `[Unit]` section
/// and record them under `diff.bound_to[<basename>]`. Multi-value lines
/// (whitespace-separated, per `man systemd.unit`) are split. Lines
/// outside `[Unit]`, comments, and empty lists are ignored.
///
/// Deliberately minimal: a line scanner, not a full ini parser. The
/// directive we care about — `BindsTo=` — is unambiguous and only
/// valid inside `[Unit]`; adding a full parser dep for one directive
/// would violate AGENTS.md's "vendor-neutral, minimal" rule.
fn collect_binds_to(source: &Path, diff: &mut UnitDiff) -> Result<()> {
    let body = fs::read_to_string(source)
        .with_context(|| format!("read {} for BindsTo scan", source.display()))?;
    let basename = source
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("non-utf8 unit basename: {}", source.display()))?
        .to_string();
    let mut in_unit_section = false;
    let mut targets: Vec<String> = Vec::new();
    for raw in body.lines() {
        let line = raw.trim();
        if line.starts_with('#') || line.starts_with(';') || line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_unit_section = line.eq_ignore_ascii_case("[Unit]");
            continue;
        }
        if !in_unit_section {
            continue;
        }
        if let Some(rest) = line.strip_prefix("BindsTo=") {
            for t in rest.split_whitespace() {
                if !t.is_empty() {
                    targets.push(t.to_string());
                }
            }
        }
    }
    if !targets.is_empty() {
        diff.bound_to.insert(basename, targets);
    }
    Ok(())
}

/// Apply the diff: create or replace symlinks for `created`/`updated`,
/// overwrite `update_requires_confirm` only when `opts.yes`. The
/// legacy system unit is never removed by `sy` itself unless the
/// process runs as uid 0; otherwise we print a `sudo rm` recipe on
/// stderr and skip it.
fn apply_diff(opts: &ApplyOpts, diff: &UnitDiff) -> Result<()> {
    fs::create_dir_all(&opts.target_dir)
        .with_context(|| format!("mkdir -p {}", opts.target_dir.display()))?;

    for dest in diff.created.iter().chain(diff.updated.iter()) {
        place_symlink(opts, dest)?;
    }

    if opts.yes {
        for dest in &diff.update_requires_confirm {
            place_symlink(opts, dest)?;
        }
        for stale in &diff.removed_stale_requires_confirm {
            remove_legacy(stale)?;
        }
    } else {
        for dest in &diff.update_requires_confirm {
            eprintln!(
                "sy apply: refusing to overwrite regular file {}; re-run with --yes",
                dest.display()
            );
        }
        for stale in &diff.removed_stale_requires_confirm {
            eprintln!(
                "sy apply: legacy unit present; run: sudo rm {}",
                stale.display()
            );
        }
    }
    Ok(())
}

/// `(re)create` the symlink at `dest` pointing at the matching source
/// file. Removes any pre-existing entry (symlink or regular file) first
/// so the operation is idempotent.
fn place_symlink(opts: &ApplyOpts, dest: &Path) -> Result<()> {
    let basename = dest
        .file_name()
        .ok_or_else(|| anyhow!("dest path has no basename: {}", dest.display()))?;
    let source = opts.source_dir.join(basename);
    let canonical_source = source
        .canonicalize()
        .with_context(|| format!("canonicalize {}", source.display()))?;
    if fs::symlink_metadata(dest).is_ok() {
        fs::remove_file(dest).with_context(|| format!("remove {}", dest.display()))?;
    }
    std::os::unix::fs::symlink(&canonical_source, dest).with_context(|| {
        format!(
            "symlink {} -> {}",
            dest.display(),
            canonical_source.display()
        )
    })
}

/// Remove the legacy system-level unit. Only callable when the
/// process runs as uid 0; otherwise we emit a `sudo rm` recipe and
/// return Ok so the rest of the apply succeeds.
fn remove_legacy(path: &Path) -> Result<()> {
    // SAFETY: getuid is async-signal-safe and has no preconditions.
    let uid = unsafe { libc::getuid() };
    if uid == 0 {
        fs::remove_file(path).with_context(|| format!("rm {}", path.display()))
    } else {
        eprintln!(
            "sy apply: cannot remove {} as uid {}; run: sudo rm {}",
            path.display(),
            uid,
            path.display()
        );
        Ok(())
    }
}

/// CLI entry: wire `Cmd::Apply` flags into `sync_units` and render
/// the resulting diff to stdout. Pulled out of `main.rs` so the file
/// stays under the LOC ceiling enforced by `check_main_rs_loc.sh`.
///
/// * `root`        — repo root (we look at `configs/systemd/user/`).
/// * `xdg_config`  — base for the symlink dir (`<this>/systemd/user/`).
/// * `dry`, `json`, `yes` — see `ApplyOpts`.
pub fn run_cli(root: &Path, xdg_config: &Path, dry: bool, json: bool, yes: bool) -> Result<()> {
    let source_dir = root.join("configs/systemd/user");
    if !source_dir.is_dir() {
        // No unit set under this repo — nothing to sync. Quietly skip.
        return Ok(());
    }
    let opts = ApplyOpts {
        source_dir,
        target_dir: xdg_config.join("systemd/user"),
        legacy_system_path: PathBuf::from(LEGACY_SYSTEM_UNIT),
        dry_run: dry,
        yes,
        daemon_reload: !dry,
    };
    let diff = sync_units(&opts)?;
    render_diff(&diff, json)
}

/// Pretty-print (`json=false`) or stable JSON (`json=true`) renderer
/// for a `UnitDiff`. Kept separate from `sync_units` so callers can
/// post-process the diff before printing.
fn render_diff(diff: &UnitDiff, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(diff)?);
        return Ok(());
    }
    println!();
    println!("systemd units:");
    for p in &diff.created {
        println!("  + {}", p.display());
    }
    for p in &diff.updated {
        println!("  ~ {}", p.display());
    }
    for p in &diff.unchanged {
        println!("  = {}", p.display());
    }
    for p in &diff.update_requires_confirm {
        println!(
            "  ! {} (regular file; pass --yes to overwrite)",
            p.display()
        );
    }
    for p in &diff.removed_stale_requires_confirm {
        println!(
            "  - {} (legacy system unit; sudo rm to migrate)",
            p.display()
        );
    }
    if !diff.bound_to.is_empty() {
        println!();
        println!("BindsTo edges (SPEC §3.2 K5 / §4.5):");
        for (unit, targets) in &diff.bound_to {
            println!("  {} -> {}", unit, targets.join(", "));
        }
    }
    Ok(())
}

/// `systemctl --user daemon-reload` is idempotent and cheap; we run
/// it unconditionally after a successful apply so the freshly-linked
/// units are visible to systemd.
fn run_daemon_reload() -> Result<()> {
    let st = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()
        .context("spawn systemctl --user daemon-reload")?;
    if !st.success() {
        return Err(anyhow!(
            "systemctl --user daemon-reload exited with status {}",
            st
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Canonical user-mode unit set from SPEC §4.5; matches Step 1's
    /// `configs/systemd/user/` layout (six files + the empty
    /// `sy.target.wants/` directory which sync_units only handles
    /// when it materialises as files — out of scope for max_depth(1)).
    const UNIT_FILES: &[&str] = &[
        "sy.target",
        "sy-aiplane.service",
        "sy-agentd.service",
        "sy-knowledge.service",
        "sy-knowledge.socket",
        "sy-qdrant.service",
    ];

    /// Drop six empty unit files into `dir`. Their *content* is
    /// irrelevant to the diff logic — only the basenames and symlink
    /// targets matter.
    fn seed_source(dir: &Path) {
        for name in UNIT_FILES {
            fs::write(dir.join(name), "").unwrap();
        }
    }

    fn empty_legacy(td: &TempDir) -> PathBuf {
        td.path().join("nonexistent-legacy-unit")
    }

    #[test]
    fn diff_against_empty_target_creates_all() {
        let td = TempDir::new().unwrap();
        let src = td.path().join("src");
        let tgt = td.path().join("tgt");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&tgt).unwrap();
        seed_source(&src);

        let opts = ApplyOpts::for_test(src, tgt, empty_legacy(&td));
        let diff = sync_units(&opts).unwrap();

        assert_eq!(diff.created.len(), UNIT_FILES.len());
        assert!(diff.updated.is_empty());
        assert!(diff.unchanged.is_empty());
        assert!(diff.update_requires_confirm.is_empty());
        assert!(diff.removed_stale_requires_confirm.is_empty());
    }

    #[test]
    fn diff_against_identical_target_is_noop() {
        let td = TempDir::new().unwrap();
        let src = td.path().join("src");
        let tgt = td.path().join("tgt");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&tgt).unwrap();
        seed_source(&src);
        // Pre-populate `tgt` with symlinks pointing at the canonical
        // source paths.
        for name in UNIT_FILES {
            let from = src.join(name).canonicalize().unwrap();
            std::os::unix::fs::symlink(&from, tgt.join(name)).unwrap();
        }

        let opts = ApplyOpts::for_test(src, tgt, empty_legacy(&td));
        let diff = sync_units(&opts).unwrap();

        assert_eq!(diff.unchanged.len(), UNIT_FILES.len());
        assert!(diff.created.is_empty());
        assert!(diff.updated.is_empty());
    }

    #[test]
    fn diff_with_divergent_regular_file_requires_confirm() {
        let td = TempDir::new().unwrap();
        let src = td.path().join("src");
        let tgt = td.path().join("tgt");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&tgt).unwrap();
        seed_source(&src);
        // Plant a regular file at one of the target paths.
        fs::write(tgt.join("sy.target"), "hand-edited").unwrap();

        let opts = ApplyOpts::for_test(src, tgt, empty_legacy(&td));
        let diff = sync_units(&opts).unwrap();

        assert_eq!(diff.update_requires_confirm.len(), 1);
        assert!(diff.update_requires_confirm[0].ends_with("sy.target"));
        // The remaining five files still land in `created`.
        assert_eq!(diff.created.len(), UNIT_FILES.len() - 1);
    }

    #[test]
    fn diff_flags_legacy_system_unit_for_removal() {
        let td = TempDir::new().unwrap();
        let src = td.path().join("src");
        let tgt = td.path().join("tgt");
        let legacy = td.path().join("fake-etc-sy-knowledge.service");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&tgt).unwrap();
        seed_source(&src);
        fs::write(&legacy, "[Unit]\nDescription=stale\n").unwrap();

        let opts = ApplyOpts::for_test(src, tgt, legacy.clone());
        let diff = sync_units(&opts).unwrap();

        assert_eq!(diff.removed_stale_requires_confirm, vec![legacy]);
    }

    #[test]
    fn diff_lists_binds_to_relationships() {
        let td = TempDir::new().unwrap();
        let src = td.path().join("src");
        let tgt = td.path().join("tgt");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&tgt).unwrap();
        // Minimal unit with `BindsTo=` line in the `[Unit]` section.
        fs::write(
            src.join("sy-knowledge.service"),
            "[Unit]\nBindsTo=sy-qdrant.service\nAfter=sy-qdrant.service\n\n[Service]\nExecStart=/bin/true\n",
        )
        .unwrap();
        fs::write(src.join("sy-qdrant.service"), "[Unit]\nDescription=q\n").unwrap();

        let opts = ApplyOpts::for_test(src, tgt, empty_legacy(&td));
        let diff = sync_units(&opts).unwrap();

        let expected: Vec<String> = vec!["sy-qdrant.service".to_string()];
        assert_eq!(
            diff.bound_to.get("sy-knowledge.service"),
            Some(&expected),
            "bound_to: {:?}",
            diff.bound_to
        );
        // Units without BindsTo are not listed.
        assert!(!diff.bound_to.contains_key("sy-qdrant.service"));
    }

    #[test]
    fn dry_run_emits_stable_json_schema() {
        let td = TempDir::new().unwrap();
        let src = td.path().join("src");
        let tgt = td.path().join("tgt");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&tgt).unwrap();
        seed_source(&src);

        let opts = ApplyOpts {
            source_dir: src,
            target_dir: tgt,
            legacy_system_path: empty_legacy(&td),
            dry_run: true,
            yes: false,
            daemon_reload: false,
        };
        let diff = sync_units(&opts).unwrap();
        let v: serde_json::Value = serde_json::to_value(&diff).unwrap();

        // Every documented key MUST be present even when empty so
        // agent consumers can treat the schema as total.
        for key in [
            "created",
            "updated",
            "unchanged",
            "update_requires_confirm",
            "removed_stale_requires_confirm",
        ] {
            assert!(v.get(key).is_some(), "missing key: {key}");
            assert!(v[key].is_array(), "{key} is not an array");
        }
        // SPEC §3.2 K5 + §4.5 BindsTo edges (Step 5): present as a
        // (possibly empty) JSON object so agents can rely on the
        // key existing.
        assert!(v.get("bound_to").is_some(), "missing key: bound_to");
        assert!(v["bound_to"].is_object(), "bound_to is not an object");
    }

    /// SPEC §4.8 "E2E manual recipe" — kept as `#[ignore]` because it
    /// drives the real `systemctl --user` on the rice. Run with
    /// `cargo test -- --ignored binds_to_e2e_systemctl_recipe`.
    ///
    /// Manual recipe (rice-level, requires a populated `~/.config/`):
    /// ```text
    /// # 1. sy apply --yes
    /// # 2. systemctl --user start sy.target
    /// # 3. systemctl --user kill sy-qdrant.service
    /// # 4. wait 5s, then: systemctl --user is-active sy-knowledge.service
    /// #    -> expect "inactive" or "activating" (BindsTo triggered stop)
    /// # 5. systemctl --user start sy-qdrant.service
    /// # 6. wait 5s, then: systemctl --user is-active sy-knowledge.service
    /// #    -> expect "active" (Restart=on-failure brought it back)
    /// ```
    #[test]
    #[ignore]
    fn binds_to_e2e_systemctl_recipe_documented() {
        // Body intentionally empty: the documentation above is the
        // test. Listed via `cargo test -- --list --ignored` so it
        // shows up in audit trails.
    }

    /// SPEC §4.9 migration row — Step 6. When the legacy
    /// `/etc/systemd/system/sy-knowledge.service` is present, an apply
    /// run flags it in `removed_stale_requires_confirm`, and the
    /// non-dry-run path leaves the file in place (uid != 0 here) while
    /// emitting a `sudo rm` recipe on stderr. We assert the diff slot
    /// AND that the file is untouched after the apply, which is the
    /// non-destructive contract Step 2 codified.
    #[test]
    fn migration_flags_legacy_system_unit_when_present() {
        let td = TempDir::new().unwrap();
        let src = td.path().join("src");
        let tgt = td.path().join("tgt");
        let legacy = td.path().join("fake-etc-sy-knowledge.service");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&tgt).unwrap();
        seed_source(&src);
        fs::write(&legacy, "[Unit]\nDescription=stale legacy\n").unwrap();

        let opts = ApplyOpts {
            source_dir: src,
            target_dir: tgt,
            legacy_system_path: legacy.clone(),
            dry_run: false,
            yes: true,
            daemon_reload: false,
        };
        let diff = sync_units(&opts).unwrap();

        assert_eq!(diff.removed_stale_requires_confirm, vec![legacy.clone()]);
        assert!(
            legacy.exists(),
            "legacy unit must be left in place when uid != 0; sy only emits a sudo recipe"
        );
    }

    /// SPEC §4.9 migration row — Step 6. With no legacy unit on disk,
    /// repeated apply runs report an empty `removed_stale_requires_confirm`
    /// slot. Captures the idempotency contract from Step 2.
    #[test]
    fn migration_idempotent_when_legacy_absent() {
        let td = TempDir::new().unwrap();
        let src = td.path().join("src");
        let tgt = td.path().join("tgt");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&tgt).unwrap();
        seed_source(&src);

        let opts = ApplyOpts::for_test(src, tgt, empty_legacy(&td));
        let first = sync_units(&opts).unwrap();
        let second = sync_units(&opts).unwrap();

        assert!(first.removed_stale_requires_confirm.is_empty());
        assert!(second.removed_stale_requires_confirm.is_empty());
    }
}
