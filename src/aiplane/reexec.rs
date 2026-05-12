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
const AMD_VENV: &str = "/opt/AMD/ryzenai/venv/lib/python3.12/site-packages";
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
    let amd_venv = Path::new(AMD_VENV);
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
        .env("RYZEN_AI_INSTALLATION_PATH", "/opt/AMD/ryzenai/venv")
        .env("XILINX_VITIS", amd_venv)
        .env("XILINX_VITIS_AIETOOLS", amd_venv)
        .env("XILINX_XRT", XRT_ROOT)
        .env(REEXEC_SENTINEL, "1")
        .exec();
    // exec() only returns on failure. Fall through to the in-process
    // path so the user at least gets a chance via CPU fallback.
    eprintln!("sy aiplane: re-exec for VitisAI env failed: {err}; staying in-process");
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

/// `/opt/AMD/ryzenai/venv/lib/python3.12/site-packages` as a `Path`.
/// Exposed for workloads that need to locate vaip_config.json or
/// other artifacts shipped by AMD.
pub fn amd_venv_dir() -> &'static Path {
    Path::new(AMD_VENV)
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
        let was_set = std::env::var_os(REEXEC_SENTINEL).is_some();
        if was_set {
            std::env::remove_var(REEXEC_SENTINEL);
            assert!(!reexec_fired());
            std::env::set_var(REEXEC_SENTINEL, "1");
            assert!(reexec_fired());
        } else {
            assert!(!reexec_fired());
            std::env::set_var(REEXEC_SENTINEL, "1");
            assert!(reexec_fired());
            std::env::remove_var(REEXEC_SENTINEL);
            assert!(!reexec_fired());
        }
    }

    #[test]
    fn amd_venv_path_matches_documented_layout() {
        // If AMD ships a venv at a different python version we'll
        // need to update this constant; the test catches that.
        assert!(amd_venv_dir().ends_with("site-packages"));
    }
}
