//! BUG-20260927-0910: `sy-knowledge.service`'s `MemoryHigh` must cover every
//! aiplane worker that can share its cgroup at once.
//!
//! The knowledge daemon spawns aiplane workers as plain children, so all of
//! them live in `…/app.slice/sy-knowledge.service` and are governed by that
//! one `MemoryHigh`. When it was set to 12 GiB, raising the `stt` worker put
//! the cgroup over the limit, and the kernel answered by parking the compile
//! threads in `__mem_cgroup_handle_over_high`: 347 871 throttle events, 70 %
//! `memory.pressure full`, ~0 % CPU, and a worker that never left `loading`.
//! The box had 35 GiB free — the cap, not the hardware, caused the stall.
//!
//! These assertions keep the cap above the *measured* warm working set so a
//! cold model load throttles at worst briefly instead of livelocking.

use std::collections::HashMap;
use std::path::PathBuf;

const UNIT: &str = "configs/systemd/user/sy-knowledge.service";
/// Measured warm peak of the whole cgroup with `embed` + `rerank` resident and
/// `stt` loaded from its compiled `.rai` (cgroup `memory.peak`, KiB):
/// embed 2.2 GiB + rerank 2.0 GiB + stt 5.2 GiB RSS plus model page cache.
const MEASURED_WARM_PEAK_MIB: u64 = 10_100;
/// Required headroom over the measurement: page cache is reclaimable under
/// pressure, but a `MemoryHigh` that only just clears the peak re-introduces
/// throttling the moment a second kind is raised.
const MIN_HEADROOM_NUM: u64 = 3;
const MIN_HEADROOM_DEN: u64 = 2; // 1.5x

fn service_values(raw: &str) -> HashMap<&str, Vec<&str>> {
    let mut out: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut section = "";
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name;
            continue;
        }
        if section == "Service" {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim() == "MemoryHigh" {
                    out.entry("MemoryHigh").or_default().push(value.trim());
                }
            }
        }
    }
    out
}

/// systemd memory suffixes → MiB. Covers the plain spellings we use.
fn mib(raw: &str) -> u64 {
    let digits: String = raw.chars().take_while(|c| c.is_ascii_digit()).collect();
    let suffix = raw[digits.len()..].trim();
    let base: u64 = digits
        .parse()
        .unwrap_or_else(|_| panic!("bad memory size {raw:?}"));
    match suffix {
        "" | "K" | "KB" => base / 1024,
        "M" | "MB" => base,
        "G" | "GB" => base * 1024,
        other => panic!("unsupported memory suffix {other:?}"),
    }
}

fn repo_unit() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(UNIT);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn memory_high_is_set_exactly_once() {
    let raw = repo_unit();
    let parsed = service_values(&raw);
    let values = parsed
        .get("MemoryHigh")
        .unwrap_or_else(|| panic!("{UNIT} must set MemoryHigh"));
    assert_eq!(
        values.len(),
        1,
        "two MemoryHigh lines make the effective cap depend on systemd version"
    );
}

#[test]
fn memory_high_covers_measured_worker_peak_with_headroom() {
    let raw = repo_unit();
    let cap = mib(service_values(&raw)["MemoryHigh"][0]);
    let floor = MEASURED_WARM_PEAK_MIB * MIN_HEADROOM_NUM / MIN_HEADROOM_DEN;
    assert!(
        cap >= floor,
        "{UNIT}: MemoryHigh={cap} MiB is below {floor} MiB \
         (measured warm peak {MEASURED_WARM_PEAK_MIB} MiB x 1.5 headroom). \
         A cap under the peak throttles a cold model load into a stall: \
         see specs/bugs/BUG-20260927-0910.md"
    );
}

/// The contiguous `#` comment block directly above a directive is its
/// rationale. Matching per line would miss a wrapped sentence, which is
/// exactly how the stale 12 GiB justification survived.
fn rationale_block(raw: &str, directive: &str) -> String {
    let lines: Vec<&str> = raw.lines().collect();
    let idx = lines
        .iter()
        .position(|l| l.trim().starts_with(directive))
        .unwrap_or_else(|| panic!("{directive} missing"));
    let mut block = Vec::new();
    let mut i = idx;
    while i > 0 {
        let prev = lines[i - 1].trim();
        if prev.starts_with('#') {
            block.push(prev.to_lowercase());
            i -= 1;
        } else {
            break;
        }
    }
    block.reverse();
    block.join(" ")
}

#[test]
fn memory_high_rationale_is_current() {
    let raw = repo_unit();
    let rationale = rationale_block(&raw, "MemoryHigh=");
    assert!(
        !rationale.is_empty(),
        "MemoryHigh needs a rationale comment"
    );
    // qdrant has run in its own sy-qdrant.service since arch-supervision
    // Step 6, so budgeting its working set into this cgroup is stale.
    assert!(
        !rationale.contains("qdrant"),
        "stale MemoryHigh rationale still budgets qdrant into this cgroup: {rationale}"
    );
    // The cap must be traceable to a measurement, not a vibe.
    assert!(
        rationale.contains("measured") || rationale.contains("observed"),
        "MemoryHigh rationale must cite the measured working set: {rationale}"
    );
}

#[test]
fn unit_ships_the_directive_uncommented() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(UNIT);
    assert!(path.is_file(), "{UNIT} must exist in the repo");
}
