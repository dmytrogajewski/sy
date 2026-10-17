//! Metrics socket for the iced-owned stack bar process.

use std::{path::PathBuf, sync::mpsc, thread};

use anyhow::{Context, Result};

pub struct Guard {
    path: PathBuf,
    shutdown: mpsc::Sender<()>,
}

impl Guard {
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.shutdown.send(());
    }
}

pub fn spawn(plane: &'static str) -> Result<Guard> {
    let base = std::env::var("XDG_RUNTIME_DIR").context("read XDG_RUNTIME_DIR")?;
    anyhow::ensure!(!base.is_empty(), "XDG_RUNTIME_DIR is empty");
    let path = PathBuf::from(base)
        .join("sy")
        .join(plane)
        .join("metrics.sock");
    let (ready_tx, ready_rx) = mpsc::sync_channel(0);
    let (shutdown_tx, shutdown_rx) = mpsc::channel();
    let thread_path = path.clone();
    thread::Builder::new()
        .name(format!("sy-{plane}-metrics"))
        .spawn(move || {
            let result = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build();
            match result {
                Ok(runtime) => {
                    let _enter = runtime.enter();
                    match sy_core::obs::mon_exporter::install(thread_path.clone()) {
                        Ok(guard) => {
                            use std::os::unix::fs::PermissionsExt;
                            if let Ok(metadata) = std::fs::metadata(&thread_path) {
                                let mut permissions = metadata.permissions();
                                permissions.set_mode(0o600);
                                let _ = std::fs::set_permissions(&thread_path, permissions);
                            }
                            let _ = ready_tx.send(Ok(()));
                            let _ = shutdown_rx.recv();
                            drop(guard);
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(anyhow::anyhow!(e.to_string())));
                        }
                    }
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e.into()));
                }
            }
        })
        .context("spawn metrics thread")?;
    ready_rx
        .recv()
        .context("metrics thread exited before readiness")??;
    Ok(Guard {
        path,
        shutdown: shutdown_tx,
    })
}
