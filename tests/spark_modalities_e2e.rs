#![cfg(feature = "spark-agent")]

#[path = "../src/spark/gateway.rs"]
#[cfg_attr(test, allow(dead_code))]
mod gateway;
#[path = "../src/spark/upstream.rs"]
#[cfg_attr(test, allow(dead_code))]
mod upstream;

#[test]
fn embedding_only_profile_denies_generation_routes() {
    let profile = gateway::GatewayProfile::embedding(1_024, 32, 65_536, true, 1_000);

    assert!(profile.allows(gateway::PublicAction::Embeddings));
    assert!(!profile.allows(gateway::PublicAction::Responses));
}

fn vision_profile() -> gateway::GatewayProfile {
    gateway::GatewayProfile {
        capabilities: ["text_generation".into(), "vision".into()].into(),
        vision: Some(gateway::VisionPolicy {
            processor_sha256: "a".repeat(64),
            media_types: ["image/png".into()].into(),
            max_bytes: 1024,
            max_total_bytes: 1024,
            max_count: 1,
            max_width: 16,
            max_height: 16,
            health_media_type: "image/png".into(),
            health_image_base64: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=".into(),
            health_image_sha256: "431ced6916a2a21a156e38701afe55bbd7f88969fbbfc56d7fe099d47f265460".into(),
            health_prompt: "name the color".into(),
            health_expected_text: "black".into(),
            health_max_tokens: 64,
            health_disable_thinking: true,
        }),
        embeddings: None,
        sampling: gateway::SamplingPolicy::default(),
    }
}

#[test]
fn both_public_adapters_preserve_same_local_image_semantics() {
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
    let openai = format!(
        r#"{{"input":[{{"type":"message","role":"user","content":[{{"type":"input_text","text":"describe"}},{{"type":"input_image","image_url":"data:image/png;base64,{PNG}"}}]}}]}}"#
    );
    let anthropic = format!(
        r#"{{"model":"ornith","messages":[{{"role":"user","content":[{{"type":"text","text":"describe"}},{{"type":"image","source":{{"type":"base64","media_type":"image/png","data":"{PNG}"}}}}]}}],"max_tokens":8}}"#
    );

    let openai = gateway::rewrite_responses_request_with_profile(
        openai.as_bytes(),
        "Ornith-1.5-9B",
        &vision_profile(),
    )
    .unwrap();
    let anthropic = gateway::rewrite_anthropic_request_with_profile(
        anthropic.as_bytes(),
        "Ornith-1.5-9B",
        &vision_profile(),
    )
    .unwrap();
    let openai: serde_json::Value = serde_json::from_slice(&openai.body).unwrap();
    let anthropic: serde_json::Value = serde_json::from_slice(&anthropic.body).unwrap();

    assert_eq!(openai["messages"], anthropic["messages"]);
}

#[test]
fn embeddings_preserve_order_identity_dimension_normalization_and_usage() {
    let profile = gateway::GatewayProfile::embedding(4, 3, 64, true, 1_000);
    let request = gateway::rewrite_embeddings_request(
        br#"{"model":"public","input":["rust ownership","borrow checker"]}"#,
        "Qwen3-Embedding-0.6B",
        &profile,
    )
    .unwrap();
    let request: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(
        request["input"],
        serde_json::json!(["rust ownership", "borrow checker"])
    );
    assert!(request.get("dimensions").is_none());

    let upstream = br#"{"object":"list","model":"Qwen3-Embedding-0.6B","data":[{"object":"embedding","index":0,"embedding":[1.0,0.0,0.0,0.0]},{"object":"embedding","index":1,"embedding":[0.8,0.6,0.0,0.0]}],"usage":{"prompt_tokens":5,"total_tokens":5}}"#;
    let document = gateway::rewrite_embeddings_response(
        upstream,
        "qwen3-embedding:0.6b",
        "Qwen3-Embedding-0.6B",
        &profile,
        2,
    )
    .unwrap();

    assert_eq!(document["model"], "qwen3-embedding:0.6b");
    assert_eq!(document["data"][1]["index"], 1);
    assert_eq!(
        document["data"][0]["embedding"].as_array().unwrap().len(),
        4
    );
    assert_eq!(document["usage"]["prompt_tokens"], 5);
}

#[test]
fn published_route_keeps_executor_verified_capability_profile() {
    let upstream = upstream::ObservedRoute::new(
        "i_11111111111111111111111111111111",
        1,
        "172.30.0.2".parse().unwrap(),
        8000,
        [("GET", "/v1/models"), ("POST", "/v1/embeddings")],
    )
    .unwrap();
    let routes = gateway::RouteRegistry::default();
    routes.publish_with_profile(
        "embeddings",
        "qwen3-embedding:0.6b".into(),
        "Qwen3-Embedding-0.6B".into(),
        gateway::GatewayProfile::embedding(1_024, 16, 32_768, true, 1_000),
        upstream,
    );
    let gateway::RouteLookup::Healthy(route) = routes.lookup("embeddings") else {
        panic!("embedding route was not published")
    };

    assert!(route.profile.allows(gateway::PublicAction::Embeddings));
}

#[test]
fn embedding_instance_survives_stop_restart_with_same_identity() {
    let routes = gateway::RouteRegistry::default();
    let profile = gateway::GatewayProfile::embedding(1_024, 16, 32_768, true, 1_000);
    for generation in [1, 2] {
        let upstream = upstream::ObservedRoute::new(
            "i_11111111111111111111111111111111",
            generation,
            "172.30.0.2".parse().unwrap(),
            8000,
            [("GET", "/v1/models"), ("POST", "/v1/embeddings")],
        )
        .unwrap();
        routes.publish_with_profile(
            "embeddings",
            "qwen3-embedding:0.6b".into(),
            "Qwen3-Embedding-0.6B".into(),
            profile.clone(),
            upstream,
        );
        if generation == 1 {
            routes.drain("embeddings", generation);
        }
    }
    let gateway::RouteLookup::Healthy(route) = routes.lookup("embeddings") else {
        panic!("restarted embedding route was not published")
    };
    assert_eq!(route.public_model, "qwen3-embedding:0.6b");
    assert_eq!(route.generation, 2);
    assert!(route.profile.allows(gateway::PublicAction::Embeddings));
}

#[test]
fn image_urls_files_magic_limits_and_text_only_profiles_fail_before_upstream() {
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
    let requests = [
        r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_image","image_url":"https://example.invalid/x.png"}]}]}"#.to_owned(),
        r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_image","image_url":"file:///../../etc/passwd"}]}]}"#.to_owned(),
        format!(r#"{{"input":[{{"type":"message","role":"user","content":[{{"type":"input_image","image_url":"data:image/jpeg;base64,{PNG}"}}]}}]}}"#),
        format!(r#"{{"input":[{{"type":"message","role":"user","content":[{{"type":"input_image","image_url":"data:image/png;base64,{PNG}"}},{{"type":"input_image","image_url":"data:image/png;base64,{PNG}"}}]}}]}}"#),
    ];
    assert!(requests.iter().all(|request| {
        gateway::rewrite_responses_request_with_profile(
            request.as_bytes(),
            "Ornith-1.5-9B",
            &vision_profile(),
        )
        .is_err()
    }));
    let valid = format!(
        r#"{{"input":[{{"type":"message","role":"user","content":[{{"type":"input_text","text":"x"}},{{"type":"input_image","image_url":"data:image/png;base64,{PNG}"}}]}}]}}"#
    );
    assert!(gateway::rewrite_responses_request(valid.as_bytes(), "Ornith-1.5-9B").is_err());
    let mut too_small = vision_profile();
    too_small.vision.as_mut().unwrap().max_width = 0;
    assert!(gateway::rewrite_responses_request_with_profile(
        valid.as_bytes(),
        "Ornith-1.5-9B",
        &too_small,
    )
    .is_err());
    assert!(gateway::rewrite_chat_request(
        br#"{"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.invalid/x.png"}}]}]}"#,
        "Ornith-1.5-9B",
    )
    .is_err());
}

#[test]
fn embedding_limits_and_invalid_vectors_fail_closed() {
    let profile = gateway::GatewayProfile::embedding(2, 2, 8, true, 1_000);
    for request in [
        br#"{"input":[]}"#.as_slice(),
        br#"{"input":["a","b","c"]}"#.as_slice(),
        br#"{"input":"123456789"}"#.as_slice(),
        br#"{"input":"x","dimensions":1}"#.as_slice(),
        br#"{"input":"x","encoding_format":"base64"}"#.as_slice(),
        br#"{"input":"x","url":"http://private"}"#.as_slice(),
    ] {
        assert!(gateway::rewrite_embeddings_request(request, "internal", &profile).is_err());
    }
    for response in [
        br#"{"object":"list","model":"internal","data":[{"object":"embedding","index":0,"embedding":[1.0]}],"usage":{"prompt_tokens":1,"total_tokens":1}}"#.as_slice(),
        br#"{"object":"list","model":"internal","data":[{"object":"embedding","index":1,"embedding":[1.0,0.0]}],"usage":{"prompt_tokens":1,"total_tokens":1}}"#.as_slice(),
        br#"{"object":"list","model":"internal","data":[{"object":"embedding","index":0,"embedding":[0.5,0.0]}],"usage":{"prompt_tokens":1,"total_tokens":1}}"#.as_slice(),
        br#"{"object":"list","model":"spoofed","data":[{"object":"embedding","index":0,"embedding":[1.0,0.0]}],"usage":{"prompt_tokens":1,"total_tokens":1}}"#.as_slice(),
        br#"{"object":"list","model":"internal","data":[{"object":"embedding","index":0,"embedding":[1.0,0.0]}],"usage":{"prompt_tokens":1,"total_tokens":2}}"#.as_slice(),
    ] {
        assert!(gateway::rewrite_embeddings_response(
            response, "public", "internal", &profile, 1
        )
        .is_err());
    }
}

#[test]
fn normalized_embedding_similarity_preserves_expected_ranking() {
    let query = [1.0_f32, 0.0];
    let close = [0.8_f32, 0.6];
    let distant = [0.0_f32, 1.0];
    let cosine = |vector: [f32; 2]| {
        query
            .iter()
            .zip(vector)
            .map(|(left, right)| left * right)
            .sum::<f32>()
    };

    assert!(cosine(close) > cosine(distant));
}

#[tokio::test]
async fn embedding_readiness_uses_models_and_embedding_contract_not_generation() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for index in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let count = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            let body = if index == 0 {
                assert!(request.starts_with("GET /v1/models "));
                r#"{"data":[{"id":"Qwen3-Embedding-0.6B"}]}"#
            } else {
                assert!(request.starts_with("POST /v1/embeddings "));
                assert!(!request.contains(r#""dimensions""#));
                r#"{"object":"list","model":"Qwen3-Embedding-0.6B","data":[{"object":"embedding","index":0,"embedding":[1.0,0.0]}],"usage":{"prompt_tokens":1,"total_tokens":1}}"#
            };
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    let route = upstream::ObservedRoute::new(
        "i_11111111111111111111111111111111",
        1,
        address.ip(),
        address.port(),
        [("GET", "/v1/models"), ("POST", "/v1/embeddings")],
    )
    .unwrap();

    route
        .embedding_probe(
            "Qwen3-Embedding-0.6B",
            "Represent this sentence for retrieval.",
            2,
            true,
            1_000,
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn vision_readiness_uses_the_exact_signed_inline_fixture() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for index in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let count = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            let body = if index == 0 {
                assert!(request.starts_with("GET /v1/models "));
                r#"{"data":[{"id":"Ornith-1.5-9B"}]}"#
            } else {
                assert!(request.starts_with("POST /v1/chat/completions "));
                assert!(request.contains("data:image/png;base64,iVBORw0KGgo"));
                assert!(request.contains(r#""enable_thinking":false"#));
                r#"{"object":"chat.completion","model":"Ornith-1.5-9B","choices":[{"index":0,"message":{"role":"assistant","content":"black"},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":1,"total_tokens":10}}"#
            };
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    let route = upstream::ObservedRoute::new(
        "i_11111111111111111111111111111111",
        1,
        address.ip(),
        address.port(),
        [("GET", "/v1/models"), ("POST", "/v1/chat/completions")],
    )
    .unwrap();
    let policy = vision_profile().vision.unwrap();
    let image = gateway::vision_health_image(&policy).unwrap();

    route
        .vision_probe(
            upstream::VisionProbe {
                served_model: "Ornith-1.5-9B",
                prompt: &policy.health_prompt,
                image: &image,
                expected_text: &policy.health_expected_text,
                max_tokens: policy.health_max_tokens,
                disable_thinking: policy.health_disable_thinking,
            },
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn vision_probe_rejection_diagnostic_excludes_engine_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for index in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 8192];
            let count = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            let (status, body) = if index == 0 {
                assert!(request.starts_with("GET /v1/models "));
                ("200 OK", r#"{"data":[{"id":"Ornith-1.5-9B"}]}"#)
            } else {
                assert!(request.starts_with("POST /v1/chat/completions "));
                (
                    "400 Bad Request",
                    r#"{"error":{"message":"caller prompt and private engine detail"}}"#,
                )
            };
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    let route = upstream::ObservedRoute::new(
        "i_11111111111111111111111111111111",
        1,
        address.ip(),
        address.port(),
        [("GET", "/v1/models"), ("POST", "/v1/chat/completions")],
    )
    .unwrap();
    let policy = vision_profile().vision.unwrap();
    let image = gateway::vision_health_image(&policy).unwrap();

    let error = route
        .vision_probe(
            upstream::VisionProbe {
                served_model: "Ornith-1.5-9B",
                prompt: &policy.health_prompt,
                image: &image,
                expected_text: &policy.health_expected_text,
                max_tokens: policy.health_max_tokens,
                disable_thinking: policy.health_disable_thinking,
            },
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap_err();
    assert_eq!(error.diagnostic(), "vision probe rejected with HTTP 4xx");
    assert!(!error.to_string().contains("private engine detail"));
    server.await.unwrap();
}
