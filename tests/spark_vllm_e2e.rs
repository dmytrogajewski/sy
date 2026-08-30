#![cfg(feature = "spark-agent")]

#[path = "../src/spark/gateway.rs"]
#[cfg_attr(test, allow(dead_code))]
mod gateway;
#[path = "../src/spark/upstream.rs"]
#[cfg_attr(test, allow(dead_code))]
mod upstream;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn fake_openai_engine() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for _ in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let count = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            let body = if request.starts_with("GET /v1/models ") {
                r#"{"object":"list","data":[{"id":"Ornith-1.5-9B"}]}"#.to_owned()
            } else if request.contains("\"stream\":true") {
                "data: {\"choices\":[{\"text\":\"OK\",\"finish_reason\":null}]}\n\ndata: [DONE]\n\n"
                    .into()
            } else {
                r#"{"id":"cmpl-e2e","object":"text_completion","model":"Ornith-1.5-9B","choices":[{"index":0,"text":"","finish_reason":"length"}],"usage":{"prompt_tokens":5,"completion_tokens":1}}"#.to_owned()
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (address, server)
}

#[tokio::test]
async fn engine_neutral_wire_identity_stream_start_stop_and_forbidden_route_are_exact() {
    let (address, server) = fake_openai_engine().await;
    let route = upstream::ObservedRoute::new(
        "i_11111111111111111111111111111111",
        1,
        address.ip(),
        address.port(),
        [("GET", "/v1/models"), ("POST", "/v1/completions")],
    )
    .unwrap();
    route
        .semantic_probe(
            "Ornith-1.5-9B",
            "Generate one completion token.",
            1,
            upstream::MAX_SEMANTIC_PROBE_TIMEOUT,
        )
        .await
        .unwrap();
    assert!(route.request("GET", "/health", 0).is_err());

    let routes = gateway::RouteRegistry::default();
    routes.mark_warming("ornith", 1);
    assert!(matches!(
        routes.lookup("ornith"),
        gateway::RouteLookup::Warming
    ));
    routes.publish(
        "ornith",
        "ornith-1.5:9b".into(),
        "Ornith-1.5-9B".into(),
        route.clone(),
    );
    let gateway::RouteLookup::Healthy(published) = routes.lookup("ornith") else {
        panic!("semantic-ready route was not published")
    };
    assert_eq!(
        gateway::models_document(&published)["data"][0]["id"],
        "ornith-1.5:9b"
    );
    assert_eq!(published.served_model, "Ornith-1.5-9B");
    assert_eq!(published.upstream.identity().1, 1);
    assert_eq!(
        upstream::UpstreamError::identity_mismatch().to_string(),
        "engine route identity changed"
    );
    assert_eq!(
        gateway::public_action("POST", "completions"),
        Some(gateway::PublicAction::Completions)
    );
    let (rewritten, streaming) = gateway::rewrite_completion_request(
        br#"{"model":"public","prompt":"OK","stream":true}"#,
        "Ornith-1.5-9B",
    )
    .unwrap();
    let rewritten: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
    assert!(streaming && rewritten["model"] == "Ornith-1.5-9B");
    assert_eq!(rewritten["stream_options"]["include_usage"], true);
    assert_eq!(gateway::RETRY_AFTER_SECONDS, "1");
    assert_eq!(
        gateway::rewrite_completion_response(
            br#"{"model":"internal","choices":[]}"#,
            "ornith-1.5:9b",
        )
        .unwrap()["model"],
        "ornith-1.5:9b"
    );
    let mut stream = route
        .completion_stream(
            br#"{"model":"Ornith-1.5-9B","prompt":"OK","max_tokens":1,"stream":true}"#,
        )
        .await
        .unwrap();
    assert_eq!(
        stream.next().await.unwrap().unwrap(),
        upstream::GenerationEvent::TextDelta { text: "OK".into() }
    );
    drop(stream);
    routes.drain("ornith", 1);
    assert!(matches!(
        routes.lookup("ornith"),
        gateway::RouteLookup::Missing
    ));
    server.await.unwrap();
}
