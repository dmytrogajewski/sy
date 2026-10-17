//! Standalone inference daemon: owns the worker supervisor and AI socket.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result};

use super::{ipc, runtime_status::RuntimeStatus, supervisor};

pub fn run() -> Result<()> {
    let _obs_guard = sy_core::obs::init(sy_core::obs::Mode::Daemon { name: "sy-aiplane" })?;
    if legacy_knowledge_owns_ai()? {
        anyhow::bail!(
            "the running knowledge daemon still owns AI workers; restart sy-knowledge.service before starting sy-aiplane.service"
        );
    }
    let sup = Arc::new(supervisor::Supervisor::new());
    supervisor::set_current(Arc::clone(&sup));
    ipc::serve_aiplane().context("bind aiplane IPC")?;
    super::runtime_status::save(&RuntimeStatus::capture(&sup, true))?;
    #[cfg(feature = "mon-exporter")]
    let _metrics = super::mon_exporter::spawn().context("aiplane metrics socket")?;
    let polling = Arc::new(AtomicBool::new(true));
    let polling_thread = Arc::clone(&polling);
    let poll_supervisor = Arc::clone(&sup);
    let poller = thread::Builder::new()
        .name("sy-aiplane-health".into())
        .spawn(move || {
            while polling_thread.load(Ordering::SeqCst) {
                poll_supervisor.poll_once();
                if let Err(e) = super::runtime_status::save(&RuntimeStatus::capture(&poll_supervisor, true)) {
                    tracing::warn!(target: "sy::aiplane::daemon", error = %format!("{e:#}"), "write runtime status");
                }
                thread::sleep(Duration::from_secs(1));
            }
        })
        .context("spawn aiplane health poll")?;
    sy_core::notify::ready();
    let _watchdog = sy_core::notify::spawn_watchdog();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("aiplane signal runtime")?;
    rt.block_on(async {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).context("SIGTERM handler")?;
        tokio::select! {
            _ = term.recv() => {},
            result = tokio::signal::ctrl_c() => result.context("SIGINT handler")?,
        }
        Ok::<(), anyhow::Error>(())
    })?;
    sy_core::notify::stopping();
    polling.store(false, Ordering::SeqCst);
    poller.join().expect("aiplane health poll panicked");
    sup.shutdown(Duration::from_secs(5));
    super::runtime_status::save(&RuntimeStatus::capture(&sup, false))?;
    match std::fs::remove_file(ipc::aiplane_socket_path()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("remove aiplane socket"),
    }
    Ok(())
}

fn legacy_knowledge_owns_ai() -> Result<bool> {
    let path = ipc::socket_path();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("legacy ownership probe runtime")?;
    rt.block_on(async {
        let connect =
            tokio::time::timeout(Duration::from_secs(2), sy_ipc::Client::connect(&path)).await;
        let mut client = match connect {
            Ok(Ok(client)) => client,
            Ok(Err(e))
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.kind() == std::io::ErrorKind::ConnectionRefused =>
            {
                return Ok(false);
            }
            Ok(Err(e)) => return Err(e).with_context(|| format!("connect {}", path.display())),
            Err(_) => anyhow::bail!(
                "knowledge socket {} did not accept within 2s",
                path.display()
            ),
        };
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            client.call(
                "system.describe",
                serde_json::json!({}),
                sy_ipc::CallOpts::default(),
            ),
        )
        .await
        .context("knowledge describe timed out")?
        .context("knowledge describe failed")?;
        match response {
            sy_ipc::Response::Ok { result, .. } => Ok(result["methods"]
                .as_array()
                .is_some_and(|methods| methods.iter().any(|method| method == "aiplane.run"))),
            sy_ipc::Response::Err { error, .. } => {
                anyhow::bail!("knowledge describe rejected: {}", error.message)
            }
        }
    })
}
