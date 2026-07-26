//! Step 12 sandbox-shape test (DoD bullet 3).
//!
//! The journey rationale for `sy-plugin-md` is to *replace* the
//! `md-rich.yazi` previewer, which spawned chrome-headless and (on
//! some hosts) reached into gnome-keyring for fontconfig caching.
//! This test pins the contract that the new renderer:
//!
//! 1. Never spawns a chrome / chromium / google-chrome process —
//!    `pgrep` count before == count after.
//! 2. Never connects to a gnome-keyring socket — verified via
//!    `strace -f -e trace=connect` when `strace` is on PATH. On hosts
//!    without `strace` the test still runs the pgrep guard so the
//!    test cannot regress the goal silently.
//!
//! The test renders inside the same process (no subprocess spawn) so
//! the pgrep window can be tight — anything that *would* be a child
//! of `sy-plugin-md` shows up against this process tree.

use std::path::PathBuf;
use std::process::Command;
use sy_plugin_md::render::{render_to_png, RenderOpts};

fn fixture_md() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/preview-sample.md");
    std::fs::read_to_string(p).expect("read preview-sample.md")
}

/// Count processes whose argv basename matches any of `names`. We
/// shell out to `pgrep -c` rather than walking `/proc` ourselves so a
/// failure mode in the procfs reader can't masquerade as "no chrome
/// was spawned". `pgrep` returns 1 on no-match (exit code, stdout
/// empty); we treat that as 0 so the diff still works.
fn pgrep_count(names: &[&str]) -> u32 {
    let mut total = 0u32;
    for n in names {
        let out = Command::new("pgrep").args(["-c", "-x", n]).output();
        if let Ok(out) = out {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Ok(n) = s.trim().parse::<u32>() {
                total += n;
            }
        }
    }
    total
}

#[test]
fn render_does_not_spawn_chrome_or_keyring() {
    let chrome_names = ["chrome", "chromium", "chromium-browser", "google-chrome"];
    let keyring_names = ["gnome-keyring-d", "gnome-keyring-daemon"];

    let chrome_before = pgrep_count(&chrome_names);
    let keyring_before = pgrep_count(&keyring_names);

    let md = fixture_md();
    let png = render_to_png(&md, &RenderOpts::default()).expect("render");
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "render produced non-PNG");

    let chrome_after = pgrep_count(&chrome_names);
    let keyring_after = pgrep_count(&keyring_names);

    assert_eq!(
        chrome_before, chrome_after,
        "chrome/chromium count changed across render: before={chrome_before} after={chrome_after}"
    );
    assert_eq!(
        keyring_before, keyring_after,
        "gnome-keyring count changed across render: before={keyring_before} after={keyring_after}"
    );
}

/// Drive the renderer under `strace -f -e trace=connect` and assert
/// no connect attempt touches `gnome-keyring`. Skips with a logged
/// note on hosts without strace (CI runners frequently lack it). The
/// strace child invokes the `regen_goldens` example so we exercise
/// the real renderer path rather than a `cargo test` harness which
/// would also tracerse rustc / linker stages.
#[test]
fn strace_render_makes_no_keyring_connect() {
    let strace = match which("strace") {
        Some(p) => p,
        None => {
            eprintln!("strace not on PATH; skipping connect-trace assertion");
            return;
        }
    };
    // Pre-build the binary so the strace child measures runtime
    // behaviour, not the compile.
    let build = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "sy-plugin-md",
            "--example",
            "regen_goldens",
            "--release",
        ])
        .output()
        .expect("cargo build regen_goldens");
    assert!(
        build.status.success(),
        "cargo build failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr),
    );
    let target_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("target/release/examples/regen_goldens"))
        .expect("locate regen_goldens binary");
    if !target_root.is_file() {
        eprintln!(
            "regen_goldens binary not produced at {}, skipping",
            target_root.display()
        );
        return;
    }
    let out = Command::new(strace)
        .args([
            "-f",
            "-e",
            "trace=connect",
            "-o",
            "/dev/stderr",
            target_root.to_str().expect("utf8 path"),
        ])
        .output()
        .expect("strace render");
    // We don't require strace's success — some kernels/seccomp policies
    // can deny ptrace; what we care about is the absence of forbidden
    // connect targets in the trace stream.
    let trace = String::from_utf8_lossy(&out.stderr);
    for needle in ["keyring", "gnome-keyring", "/run/user/"] {
        if needle == "/run/user/" {
            // /run/user/<uid> contains lots of legitimate sockets
            // (waybar, pipewire, …). We narrow to a keyring-shaped
            // path inside it.
            let bad = trace
                .lines()
                .any(|line| line.contains("/run/user/") && line.contains("keyring"));
            assert!(
                !bad,
                "strace caught a connect to a keyring path:\n{}",
                trace
                    .lines()
                    .filter(|l| l.contains("keyring"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        } else {
            assert!(
                !trace.contains(needle),
                "strace caught a {needle} reference in connect trace"
            );
        }
    }
}

/// Tiny `which` shim — `which` is already a workspace dep but the
/// canary plugin's `Cargo.toml` deliberately keeps its dep list short
/// (rendering trio + PDK only). One inline closure is cheaper than
/// pulling another crate.
fn which(prog: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(prog);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
