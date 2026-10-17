pub mod file;
pub mod plugin;

#[cfg(feature = "gui-iced")]
mod mon {
    pub use sy_theme::mon_theme as theme;
}

mod knowledge {
    pub mod ipc {
        pub use sy_ipc::knowledge::HitRow;
    }

    pub mod cli {
        use anyhow::{Result, bail};
        use serde_json::json;
        use sy_ipc::{CallOpts, Client, Response};

        use super::ipc::HitRow;

        pub fn search_hits(query: &str, limit: usize, prefix: Option<&str>) -> Result<Vec<HitRow>> {
            let runtime_dir = std::env::var("XDG_RUNTIME_DIR")?;
            let path = std::path::Path::new(&runtime_dir).join("sy-knowledge.sock");
            search_hits_at(&path, query, limit, prefix)
        }

        fn search_hits_at(
            path: &std::path::Path,
            query: &str,
            limit: usize,
            prefix: Option<&str>,
        ) -> Result<Vec<HitRow>> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let response = rt.block_on(async {
                let mut client = Client::connect(path).await?;
                client
                    .call(
                        "knowledge.search_rerank",
                        json!({
                            "query": query,
                            "limit": limit,
                            "prefix": prefix,
                            "candidates": 30,
                            "filter": null,
                            "abstain_threshold": null
                        }),
                        CallOpts::default(),
                    )
                    .await
                    .map_err(anyhow::Error::from)
            })?;
            match response {
                Response::Ok { result, .. } => Ok(serde_json::from_value(result["hits"].clone())?),
                Response::Err { error, .. } => bail!("knowledge daemon: {}", error.message),
            }
        }

        #[cfg(test)]
        mod tests {
            use super::*;
            use std::io::{Read, Write};

            #[test]
            fn search_hits_round_trips_the_real_knowledge_wire() {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("sy-knowledge.sock");
                let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
                let server = std::thread::spawn(move || {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut length = [0u8; 4];
                    stream.read_exact(&mut length).unwrap();
                    let mut frame = vec![0; u32::from_be_bytes(length) as usize];
                    stream.read_exact(&mut frame).unwrap();
                    let req: sy_ipc::Request = serde_json::from_slice(&frame).unwrap();
                    assert_eq!(req.method, "knowledge.search_rerank");
                    let response = Response::Ok {
                        schema_version: sy_ipc::SCHEMA_VERSION,
                        request_id: req.request_id,
                        result: json!({"hits": [{"score":0.8,"file_path":"/tmp/example","chunk_index":0,"chunk_text":"example"}]}),
                        blob: None,
                    };
                    let bytes = serde_json::to_vec(&response).unwrap();
                    stream
                        .write_all(&(bytes.len() as u32).to_be_bytes())
                        .unwrap();
                    stream.write_all(&bytes).unwrap();
                });
                let hits = search_hits_at(&path, "example", 1, None).unwrap();
                server.join().unwrap();
                assert_eq!(hits[0].file_path, "/tmp/example");
            }
        }
    }
}
