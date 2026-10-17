pub use sy_aiplane::aiplane;
#[cfg(feature = "mon-exporter")]
pub use sy_aiplane::mon_exporter;

pub mod auto;
pub mod auto_mcp;
pub mod knowledge;

#[cfg(test)]
static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Reports whether an executable name is available on `PATH`.
pub fn which(name: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .is_ok_and(|status| status.success())
}
