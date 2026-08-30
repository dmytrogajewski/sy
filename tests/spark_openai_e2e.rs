#![cfg(feature = "spark-agent")]

#[path = "../src/spark/gateway.rs"]
#[cfg_attr(test, allow(dead_code))]
mod gateway;
#[path = "../src/spark/upstream.rs"]
#[cfg_attr(test, allow(dead_code))]
mod upstream;

use std::collections::BTreeSet;
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    thread,
};

async fn read_async_headers(stream: &mut tokio::net::TcpStream) {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    while !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
        let mut chunk = [0_u8; 1024];
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&chunk[..count]);
    }
}

fn read_http_body(stream: &mut std::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).unwrap();
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let length = headers
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap();
    while bytes.len() - header_end < length {
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).unwrap();
        bytes.extend_from_slice(&chunk[..count]);
    }
    String::from_utf8(bytes[header_end..header_end + length].to_vec()).unwrap()
}

fn write_sse(stream: &mut std::net::TcpStream, events: &[serde_json::Value]) {
    let body = events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {}\n\n",
                event["type"].as_str().unwrap(),
                event
            )
        })
        .collect::<String>();
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
}

fn completed_response(output: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"type":"response.completed","sequence_number":2,"response":{
        "id":"resp_fixture","object":"response","created_at":0,"status":"completed",
        "model":"ornith","output":[output],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}})
}

#[test]
fn responses_stateless_function_and_custom_tool_continuation_is_protocol_native() {
    let request = gateway::rewrite_responses_request(br#"{"model":"ornith","input":[{"type":"message","role":"developer","content":"rules"},{"type":"message","role":"user","content":"do the work"},{"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"q\":\"x\"}"},{"type":"function_call_output","call_id":"call_1","output":"found"},{"type":"custom_tool_call","call_id":"call_2","name":"patch","input":"*** Begin Patch"},{"type":"custom_tool_call_output","call_id":"call_2","output":"Done"}],"tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}},{"type":"custom","name":"patch"}],"stream":true}"#,
        "Ornith-1.5-9B").unwrap();
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["messages"][3]["tool_call_id"], "call_1");
    assert_eq!(body["messages"][5]["tool_call_id"], "call_2");
    assert!(request.custom_tools.contains("patch"));
    assert!(gateway::rewrite_responses_request(
        br#"{"input":[{"type":"function_call_output","call_id":"call_1","output":"orphan"}]}"#,
        "Ornith-1.5-9B"
    )
    .is_err());
}

#[test]
fn responses_function_tool_sse_finishes_before_response_completed() {
    let mut encoder = gateway::ResponsesEncoder::new("ornith".into(), BTreeSet::new());
    encoder
        .accept(upstream::GenerationEvent::ToolCallDelta {
            index: 0,
            call_id: Some("call_1".into()),
            name: Some("lookup".into()),
            arguments: "{\"q\":\"x\"}".into(),
        })
        .unwrap();
    encoder
        .accept(upstream::GenerationEvent::Finished {
            finish_reason: Some("tool_calls".into()),
        })
        .unwrap();
    encoder.accept(upstream::GenerationEvent::Done).unwrap();
    let events = std::iter::from_fn(|| encoder.pop()).collect::<Vec<_>>();
    let completed = events
        .iter()
        .position(|event| event.name == "response.completed")
        .unwrap();
    let tool_done = events
        .iter()
        .position(|event| event.name == "response.output_item.done")
        .unwrap();
    assert!(
        tool_done < completed
            && events
                .iter()
                .all(|event| event.data["sequence_number"].is_u64())
    );
}

#[test]
fn responses_usage_incomplete_and_error_states_are_explicit() {
    let mut incomplete = gateway::ResponsesEncoder::new("ornith".into(), BTreeSet::new());
    incomplete
        .accept(upstream::GenerationEvent::Usage {
            prompt_tokens: 3,
            completion_tokens: 5,
        })
        .unwrap();
    incomplete
        .accept(upstream::GenerationEvent::Finished {
            finish_reason: Some("length".into()),
        })
        .unwrap();
    incomplete.accept(upstream::GenerationEvent::Done).unwrap();
    let events = std::iter::from_fn(|| incomplete.pop()).collect::<Vec<_>>();
    let terminal = events.last().unwrap();
    assert_eq!(terminal.name, "response.incomplete");
    assert_eq!(terminal.data["response"]["usage"]["total_tokens"], 8);
    let mut failed = gateway::ResponsesEncoder::new("ornith".into(), BTreeSet::new());
    failed.fail();
    assert!(std::iter::from_fn(|| failed.pop()).any(|event| event.name == "response.failed"));
}

#[test]
fn responses_custom_tool_output_unwraps_the_native_function_argument() {
    let mut encoder =
        gateway::ResponsesEncoder::new("ornith".into(), BTreeSet::from(["patch".into()]));
    encoder
        .accept(upstream::GenerationEvent::ToolCallDelta {
            index: 0,
            call_id: Some("call_2".into()),
            name: Some("patch".into()),
            arguments: r#"{"input":"*** Begin Patch"}"#.into(),
        })
        .unwrap();
    encoder.accept(upstream::GenerationEvent::Done).unwrap();
    let events = std::iter::from_fn(|| encoder.pop()).collect::<Vec<_>>();
    let done = events
        .iter()
        .find(|event| event.name == "response.output_item.done")
        .unwrap();
    assert_eq!(done.data["item"]["type"], "custom_tool_call");
    assert_eq!(done.data["item"]["input"], "*** Begin Patch");
}

#[test]
fn responses_rejects_malformed_function_arguments_at_completion() {
    let mut encoder = gateway::ResponsesEncoder::new("fixture".into(), BTreeSet::new());
    encoder
        .accept(upstream::GenerationEvent::ToolCallDelta {
            index: 0,
            call_id: Some("call_1".into()),
            name: Some("lookup".into()),
            arguments: "{".into(),
        })
        .unwrap();
    assert!(encoder.accept(upstream::GenerationEvent::Done).is_err());
}

#[test]
fn responses_stream_terminal_is_semantically_equal_to_non_streaming() {
    let mut encoder = gateway::ResponsesEncoder::new("fixture".into(), BTreeSet::new());
    for event in [
        upstream::GenerationEvent::ReasoningDelta {
            text: "inspect".into(),
        },
        upstream::GenerationEvent::TextDelta {
            text: "answer".into(),
        },
        upstream::GenerationEvent::Usage {
            prompt_tokens: 7,
            completion_tokens: 3,
        },
        upstream::GenerationEvent::Finished {
            finish_reason: Some("stop".into()),
        },
        upstream::GenerationEvent::Done,
    ] {
        encoder.accept(event).unwrap();
    }
    let terminal = std::iter::from_fn(|| encoder.pop()).last().unwrap().data;
    assert_eq!(
        terminal["response"]["output"],
        encoder.final_document()["output"]
    );
    assert_eq!(
        terminal["response"]["usage"],
        encoder.final_document()["usage"]
    );
}

#[test]
fn responses_preserves_two_parallel_function_calls() {
    let mut encoder = gateway::ResponsesEncoder::new("fixture".into(), BTreeSet::new());
    for (index, id, name) in [(0, "call_1", "lookup"), (1, "call_2", "patch")] {
        encoder
            .accept(upstream::GenerationEvent::ToolCallDelta {
                index,
                call_id: Some(id.into()),
                name: Some(name.into()),
                arguments: format!(r#"{{"index":{index}}}"#),
            })
            .unwrap();
    }
    encoder
        .accept(upstream::GenerationEvent::Finished {
            finish_reason: Some("tool_calls".into()),
        })
        .unwrap();
    encoder.accept(upstream::GenerationEvent::Done).unwrap();
    let output = encoder.final_document()["output"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(
        output
            .iter()
            .map(|item| &item["call_id"])
            .collect::<Vec<_>>(),
        ["call_1", "call_2"]
    );
}

#[test]
fn chat_stream_keeps_reasoning_text_finish_and_usage_separate() {
    let events = [
        upstream::GenerationEvent::ReasoningDelta {
            text: "inspect".into(),
        },
        upstream::GenerationEvent::TextDelta {
            text: "answer".into(),
        },
        upstream::GenerationEvent::Finished {
            finish_reason: Some("stop".into()),
        },
        upstream::GenerationEvent::Usage {
            prompt_tokens: 7,
            completion_tokens: 3,
        },
    ];
    let chunks = events
        .into_iter()
        .filter_map(|event| gateway::chat_stream_document(event, "chat_fixture", "fixture"))
        .collect::<Vec<_>>();
    assert_eq!(
        chunks[0]["choices"][0]["delta"]["reasoning_content"],
        "inspect"
    );
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "answer");
    assert_eq!(chunks[2]["choices"][0]["finish_reason"], "stop");
    assert_eq!(chunks[3]["usage"]["total_tokens"], 10);
}

#[test]
fn hosted_tools_unknown_fields_images_and_recipe_ceiling_fail_before_upstream() {
    for request in [
        br#"{"model":"ornith","input":"x","tools":[{"type":"web_search"}]}"#.as_slice(),
        br#"{"model":"ornith","input":"x","unknown":true}"#.as_slice(),
        br#"{"model":"ornith","input":[{"type":"message","role":"user","content":[{"type":"input_image","image_url":"http://example"}]}]}"#.as_slice(),
        br#"{"model":"ornith","input":"x","max_output_tokens":32769}"#.as_slice(),
    ] {
        assert!(gateway::rewrite_responses_request(request, "Ornith-1.5-9B").is_err());
    }
}

#[tokio::test]
async fn slow_tool_stream_disconnect_and_retry_are_bounded() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        read_async_headers(&mut first).await;
        let tool = serde_json::json!({"choices":[{"delta":{"tool_calls":[{
            "index":0,"id":"call_1","function":{"name":"lookup","arguments":"{"}
        }]} ,"finish_reason":null}]})
        .to_string();
        first.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {tool}\n\n").as_bytes()).await.unwrap();
        let mut disconnected = [0_u8; 1];
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                first.read(&mut disconnected)
            )
            .await
            .unwrap()
            .unwrap(),
            0
        );
        let (mut second, _) = listener.accept().await.unwrap();
        read_async_headers(&mut second).await;
        second.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"ready\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n").await.unwrap();
    });
    let route = upstream::ObservedRoute::new(
        "i_11111111111111111111111111111111",
        1,
        address.ip(),
        address.port(),
        [("POST", "/v1/chat/completions")],
    )
    .unwrap();
    let mut first = route.chat_stream(br#"{}"#).await.unwrap();
    assert!(matches!(
        first.next().await,
        Some(Ok(upstream::GenerationEvent::ToolCallDelta { .. }))
    ));
    drop(first);
    let mut second = route.chat_stream(br#"{}"#).await.unwrap();
    assert!(
        matches!(second.next().await, Some(Ok(upstream::GenerationEvent::TextDelta { text })) if text == "ready")
    );
    assert!(matches!(
        second.next().await,
        Some(Ok(upstream::GenerationEvent::Done))
    ));
    server.await.unwrap();
}

#[test]
#[ignore = "set SY_SPARK_CODEX_BIN to the pinned Codex 0.149.0 binary"]
fn pinned_codex_0_149_completes_one_streamed_client_tool_round_trip() {
    let binary = std::env::var("SY_SPARK_CODEX_BIN").expect("exact Codex binary path");
    let version = Command::new(&binary).arg("--version").output().unwrap();
    assert!(String::from_utf8_lossy(&version.stdout).contains("0.149.0"));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        let first_body = read_http_body(&mut first);
        gateway::rewrite_responses_request(first_body.as_bytes(), "Ornith-1.5-9B").unwrap();
        let item = serde_json::json!({"id":"fc_fixture","type":"function_call","status":"completed",
            "call_id":"call_fixture","name":"update_plan","arguments":"{\"plan\":[{\"step\":\"fixture\",\"status\":\"completed\"}]}"});
        write_sse(
            &mut first,
            &[
                serde_json::json!({"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":item}),
                serde_json::json!({"type":"response.output_item.done","sequence_number":1,"output_index":0,"item":item}),
                completed_response(item),
            ],
        );
        let (mut second, _) = listener.accept().unwrap();
        let body = read_http_body(&mut second);
        gateway::rewrite_responses_request(body.as_bytes(), "Ornith-1.5-9B").unwrap();
        let message = serde_json::json!({"id":"msg_fixture","type":"message","status":"completed","role":"assistant",
            "content":[{"type":"output_text","text":"fixture complete","annotations":[]}]});
        write_sse(&mut second, &[completed_response(message)]);
        body
    });
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("config.toml"), format!("model='ornith'\nmodel_provider='spark'\nweb_search='disabled'\n[model_providers.spark]\nname='Spark'\nbase_url='http://{address}/v1'\nenv_key='SY_SPARK_INFERENCE_TOKEN'\nwire_api='responses'\nsupports_standalone_web_search=false\nsupports_websockets=false\n")).unwrap();
    let output = Command::new(binary)
        .args([
            "exec",
            "--skip-git-repo-check",
            "--sandbox",
            "read-only",
            "Use update_plan exactly once, then say fixture complete.",
        ])
        .env("CODEX_HOME", home.path())
        .env("SY_SPARK_INFERENCE_TOKEN", "fixture-only-token")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let second = server.join().unwrap();
    assert!(second.contains("function_call_output") && second.contains("call_fixture"));
}
