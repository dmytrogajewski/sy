pub mod agt;
mod mon_exporter;

#[cfg(test)]
pub(crate) fn test_workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}
mod popup;
mod wifi;
