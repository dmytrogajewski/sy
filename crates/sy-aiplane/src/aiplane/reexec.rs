//! The AMD venv re-exec dance: when `/opt/AMD/ryzenai/venv` is present
//! and we haven't already re-execed, replace the current process with
//! itself plus `LD_LIBRARY_PATH`/`ORT_DYLIB_PATH`/etc. baked in.
//!
//! Why: glibc snapshots `LD_LIBRARY_PATH` once at startup. The
//! VitisAI EP dlopens a dozen sibling `.so`s (voe, flexml,
//! flexml_extras, vaimlpl_be, flexmlrt, xrt) that aren't in any
//! ldconfig path. Setting `LD_LIBRARY_PATH` *after* main starts is a
//! no-op for the EP plugin's dependencies — they have to be resolvable
//! at the moment libonnxruntime is dlopened. The re-exec puts those
//! envs in place before any thread spawns.
//!
//! Why not setcap: AT_SECURE is set on capset binaries, and the
//! dynamic linker drops `LD_LIBRARY_PATH` under AT_SECURE. The
//! systemd unit's `AmbientCapabilities=CAP_IPC_LOCK` elevates without
//! AT_SECURE, which is the only combination that lets us have both
//! the cap and a working LD_LIBRARY_PATH.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Sentinel env var. Presence means "the re-exec already fired in a
/// parent of this process; don't loop." Workloads also check this
/// before attempting to load the VitisAI EP — without it, the EP's
/// dlopen of sibling .so files would fail.
pub const REEXEC_SENTINEL: &str = "SY_AMD_REEXECED";

/// Standard install path of the AMD Ryzen AI 1.7.x venv. Validated by
/// the ryzenai-rpm packaging.
const AMD_VENV_ROOT: &str = "/opt/AMD/ryzenai/venv";
const XRT_LIB: &str = "/opt/xilinx/xrt/lib";
const XRT_ROOT: &str = "/opt/xilinx/xrt";

/// Run before any thread spawns in `main()`. If the AMD venv is
/// present and we haven't re-exec'd yet, this never returns — it
/// `execve`s the current binary with the right env.
///
/// If the AMD venv is missing the call is a silent no-op; sy works
/// without an NPU (Embed workload falls through to CPU EP).
pub fn maybe_reexec_with_amd_env() {
    if std::env::var_os(REEXEC_SENTINEL).is_some() {
        return;
    }
    let amd_venv = amd_venv_dir();
    if !amd_venv.is_dir() {
        return;
    }
    let Ok(ort_so) = pick_ort_so(&amd_venv.join("onnxruntime/capi")) else {
        return;
    };

    // Put the canonical ORT dir FIRST so any dlopen-by-soname inside
    // VitisAI EP for `libonnxruntime.so.1` resolves to the same binary
    // that `ORT_DYLIB_PATH` points at — voe/lib also ships a copy of
    // ORT and loading both double-inits the C++ runtime singletons.
    // /opt/xilinx/xrt/lib hosts libxrt_coreutil.so which the VAIML
    // custom op dlopens; without it, the EP silently downgrades to a
    // "compile-only" session (`XRT is not installed`) and run() fails.
    let lib_dirs = [
        amd_venv.join("onnxruntime/capi"),
        amd_venv.join("flexml/flexml_extras/lib"),
        amd_venv.join("voe/lib"),
        amd_venv.join("vaimlpl_be/lib"),
        amd_venv.join("flexmlrt/lib"),
        PathBuf::from(XRT_LIB),
    ];
    let prev = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    let mut ld_path: Vec<String> = lib_dirs.iter().map(|p| p.display().to_string()).collect();
    if !prev.is_empty() {
        ld_path.push(prev);
    }

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let args: Vec<_> = std::env::args_os().skip(1).collect();

    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&exe)
        .args(&args)
        .env("LD_LIBRARY_PATH", ld_path.join(":"))
        .env("ORT_DYLIB_PATH", &ort_so)
        .env(
            "RYZEN_AI_INSTALLATION_PATH",
            amd_venv
                .ancestors()
                .nth(3)
                .unwrap_or(Path::new(AMD_VENV_ROOT)),
        )
        .env("XILINX_VITIS", &amd_venv)
        .env("XILINX_VITIS_AIETOOLS", &amd_venv)
        .env("XILINX_XRT", XRT_ROOT)
        .env(REEXEC_SENTINEL, "1")
        .exec();
    // exec() only returns on failure. Fall through to the in-process
    // path so the user at least gets a chance via CPU fallback.
    tracing::error!(
        target: "sy::aiplane::reexec",
        error = %err,
        "re-exec for VitisAI env failed; staying in-process"
    );
}

/// True iff the re-exec already fired in an ancestor. Workloads use
/// this to short-circuit VitisAI EP attempts before the EP's dlopen
/// chain blows up trying to find voe/lib/libvaiml2.so.
pub fn reexec_fired() -> bool {
    std::env::var_os(REEXEC_SENTINEL).is_some()
}

/// Best matching `libonnxruntime.so.<version>` under `dir`. AMD ships
/// just one per venv, but we max-by-name so a future venv update with
/// multiple SONAMEs still picks the newest.
pub fn pick_ort_so(dir: &Path) -> Result<PathBuf> {
    let pick = std::fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .filter_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("libonnxruntime.so.") {
                Some(e.path())
            } else {
                None
            }
        })
        .max()
        .with_context(|| format!("no libonnxruntime.so.* in {}", dir.display()))?;
    Ok(pick)
}

/// Locate AMD's Python package tree without pinning the Fedora Python ABI.
/// Exposed for workloads that need the EP config shipped in that tree.
pub fn amd_venv_dir() -> PathBuf {
    let root = std::env::var_os("SY_AMD_VENV")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(AMD_VENV_ROOT));
    find_site_packages(&root).unwrap_or_else(|| root.join("lib/python3.12/site-packages"))
}

fn find_site_packages(root: &Path) -> Option<PathBuf> {
    let mut candidates = std::fs::read_dir(root.join("lib"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("site-packages"))
        .filter(|path| pick_ort_so(&path.join("onnxruntime/capi")).is_ok())
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.pop()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_constant_matches_real_env_name() {
        // Trip wire: if we ever rename the sentinel, the daemon and
        // CLI must agree. Reading the constant ensures both call
        // sites end up at the same key.
        assert_eq!(REEXEC_SENTINEL, "SY_AMD_REEXECED");
    }

    #[test]
    fn reexec_fired_reads_env() {
        let exe = std::env::current_exe().expect("test binary path");
        let test = "aiplane::reexec::tests::reexec_fired_child";
        let absent = std::process::Command::new(&exe)
            .args(["--exact", test])
            .env_remove(REEXEC_SENTINEL)
            .env("SY_REEXEC_EXPECTED", "0")
            .status()
            .expect("run without sentinel");
        assert!(absent.success());
        let present = std::process::Command::new(exe)
            .args(["--exact", test])
            .env(REEXEC_SENTINEL, "1")
            .env("SY_REEXEC_EXPECTED", "1")
            .status()
            .expect("run with sentinel");
        assert!(present.success());
    }

    #[test]
    fn reexec_fired_child() {
        if let Ok(expected) = std::env::var("SY_REEXEC_EXPECTED") {
            assert_eq!(reexec_fired(), expected == "1");
        }
    }

    #[test]
    fn amd_venv_path_matches_documented_layout() {
        assert!(amd_venv_dir().ends_with("site-packages"));
    }

    #[test]
    fn discovers_new_python_abi_with_ort() {
        let root = tempfile::tempdir().unwrap();
        let capi = root
            .path()
            .join("lib/python3.13/site-packages/onnxruntime/capi");
        std::fs::create_dir_all(&capi).unwrap();
        std::fs::write(capi.join("libonnxruntime.so.1"), b"fixture").unwrap();
        assert_eq!(
            find_site_packages(root.path()).unwrap(),
            capi.parent().unwrap().parent().unwrap()
        );
    }
}
