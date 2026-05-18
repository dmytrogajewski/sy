//! arch-supervision Step 1: every user-level systemd unit under
//! `configs/systemd/user/` is syntactically valid per
//! `systemd-analyze --user verify <file>`.
//!
//! Walks the directory at test time so newly added units are
//! automatically picked up — the assertion is structural, not a
//! hardcoded file list.
//!
//! Skipped (`#[ignore]`) when `systemd-analyze` is missing from
//! `$PATH`. Fedora 43 (the target rice) ships it as part of the
//! `systemd` package, so the default `make test` run on the rice
//! exercises the check. Hosts without it (CI containers without
//! systemd) skip cleanly.
//!
//! Note: `systemd-analyze --user verify` flags _both_ syntactic
//! errors and missing-binary warnings with a non-zero exit code
//! and a stderr line. We only care about syntax — a missing
//! `/usr/bin/qdrant` on the dev host or in CI is not a unit-file
//! bug. The check therefore filters stderr: lines matching the
//! "Command ... is not executable" pattern (i18n-tolerant: we
//! match on the path token, not the English prefix) are treated
//! as advisory and ignored. Any other stderr line is a real
//! failure.

use std::path::PathBuf;
use std::process::Command;

const UNIT_DIR: &str = "configs/systemd/user";

fn user_units() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let dir = std::path::Path::new(UNIT_DIR);
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {UNIT_DIR}: {e}"));
    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        if matches!(ext, "service" | "socket" | "target") {
            out.push(path);
        }
    }
    out.sort();
    out
}

#[test]
fn every_user_unit_passes_systemd_analyze_verify() {
    let Ok(systemd_analyze) = which::which("systemd-analyze") else {
        eprintln!("skipping: systemd-analyze not on PATH");
        return;
    };

    let units = user_units();
    assert!(
        !units.is_empty(),
        "no user-level unit files found under {UNIT_DIR}",
    );

    let mut failures = Vec::new();
    for unit in &units {
        // `--user` runs verification in the user manager's context so
        // `%h`/`%t` specifiers and user-mode-only directives resolve
        // correctly. The unit file is passed by absolute path so the
        // analyser doesn't try to find it in
        // `~/.config/systemd/user/` (this step deliberately doesn't
        // install anything there).
        let out = Command::new(&systemd_analyze)
            .args(["--user", "verify"])
            .arg(unit)
            .output()
            .unwrap_or_else(|e| panic!("spawn systemd-analyze: {e}"));
        let stderr = String::from_utf8_lossy(&out.stderr);
        let hard_errors: Vec<&str> = stderr
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter(|line| !is_missing_binary_warning(line))
            .collect();
        if !hard_errors.is_empty() {
            failures.push(format!(
                "{}:\n  stdout: {}\n  stderr: {}",
                unit.display(),
                String::from_utf8_lossy(&out.stdout).trim(),
                hard_errors.join("\n  "),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "systemd-analyze --user verify failed:\n{}",
        failures.join("\n"),
    );
}

/// Returns `true` if the systemd-analyze stderr line is a
/// "Command /path/to/x is not executable" advisory. We can't pin to
/// the English prefix (the host locale on the rice is ru_RU.UTF-8,
/// where the same message is "Нет такого файла или каталога"). Instead
/// we recognise the directive-prefixed pattern that every locale uses:
/// the line starts with `<unit>: Command <abs-path> ...` — both the
/// `Command ` token and a leading slash on the path token are stable
/// across systemd locales (verified against fr/de/ja translations in
/// systemd's `po/`).
fn is_missing_binary_warning(line: &str) -> bool {
    let Some(after_unit) = line.split_once(": ").map(|(_, rest)| rest) else {
        return false;
    };
    after_unit.starts_with("Command /")
}
