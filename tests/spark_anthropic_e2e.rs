#![cfg(feature = "spark-agent")]

#[path = "../src/spark/gateway.rs"]
#[cfg_attr(test, allow(dead_code))]
mod gateway;
#[path = "../src/spark/upstream.rs"]
#[cfg_attr(test, allow(dead_code))]
mod upstream;

use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    thread,
};

fn read_http_request(stream: &mut std::net::TcpStream) -> (String, String) {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).unwrap();
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let length = headers.lines().find_map(|line| {
        line.to_ascii_lowercase()
            .strip_prefix("content-length:")
            .and_then(|value| value.trim().parse::<usize>().ok())
    });
    if headers
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        let mut body = Vec::new();
        let mut cursor = header_end;
        loop {
            while !bytes[cursor..].windows(2).any(|part| part == b"\r\n") {
                let mut chunk = [0_u8; 4096];
                let count = stream.read(&mut chunk).unwrap();
                bytes.extend_from_slice(&chunk[..count]);
            }
            let line_end = cursor
                + bytes[cursor..]
                    .windows(2)
                    .position(|part| part == b"\r\n")
                    .unwrap();
            let size =
                usize::from_str_radix(std::str::from_utf8(&bytes[cursor..line_end]).unwrap(), 16)
                    .unwrap();
            cursor = line_end + 2;
            while bytes.len() < cursor + size + 2 {
                let mut chunk = [0_u8; 4096];
                let count = stream.read(&mut chunk).unwrap();
                bytes.extend_from_slice(&chunk[..count]);
            }
            if size == 0 {
                return (headers, String::from_utf8(body).unwrap());
            }
            body.extend_from_slice(&bytes[cursor..cursor + size]);
            cursor += size + 2;
        }
    }
    let length = length.unwrap_or_default();
    while bytes.len() - header_end < length {
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).unwrap();
        bytes.extend_from_slice(&chunk[..count]);
    }
    (
        headers,
        String::from_utf8(bytes[header_end..header_end + length].to_vec()).unwrap(),
    )
}

fn accept_anthropic_post(listener: &TcpListener) -> (std::net::TcpStream, String) {
    loop {
        let (mut stream, _) = listener.accept().unwrap();
        let (headers, body) = read_http_request(&mut stream);
        if headers.starts_with("POST /v1/messages") {
            return (stream, body);
        }
        write!(
            stream,
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n"
        )
        .unwrap();
    }
}

fn write_anthropic_sse(stream: &mut std::net::TcpStream, events: &[serde_json::Value]) {
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

#[test]
fn system_text_and_identity_become_native_openai_chat() {
    let request = gateway::rewrite_anthropic_request(
        br#"{"model":"ornith","system":[{"type":"text","text":"rules"}],"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}],"max_tokens":64,"top_k":12,"stream":true}"#,
        "Ornith-1.5-9B",
    )
    .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["model"], "Ornith-1.5-9B");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][1]["content"], "hello");
    assert_eq!(body["top_k"], 12);
    assert!(request.stream);
}

#[test]
fn multiple_tool_uses_and_results_preserve_call_identity() {
    let request = gateway::rewrite_anthropic_request(
        br#"{"model":"ornith","messages":[{"role":"user","content":"do work"},{"role":"assistant","content":[{"type":"tool_use","id":"tool_1","name":"lookup","input":{"q":"x"}},{"type":"tool_use","id":"tool_2","name":"patch","input":{"path":"a"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool_1","content":"found"},{"type":"tool_result","tool_use_id":"tool_2","content":[{"type":"text","text":"done"}]}]}],"max_tokens":64,"tools":[{"name":"lookup","description":"lookup","input_schema":{"type":"object"}},{"name":"patch","input_schema":{"type":"object"}}]}"#,
        "Ornith-1.5-9B",
    )
    .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["messages"][1]["tool_calls"][1]["id"], "tool_2");
    assert_eq!(body["messages"][3]["tool_call_id"], "tool_2");
    assert_eq!(body["tools"][0]["type"], "function");
}

#[test]
fn native_sse_orders_message_blocks_stop_and_usage() {
    let mut encoder = gateway::AnthropicEncoder::new("ornith".into());
    encoder
        .accept(upstream::GenerationEvent::ToolCallDelta {
            index: 0,
            call_id: Some("tool_1".into()),
            name: Some("lookup".into()),
            arguments: r#"{"q":"x"}"#.into(),
        })
        .unwrap();
    encoder
        .accept(upstream::GenerationEvent::Usage {
            prompt_tokens: 7,
            completion_tokens: 3,
        })
        .unwrap();
    encoder
        .accept(upstream::GenerationEvent::Finished {
            finish_reason: Some("tool_calls".into()),
        })
        .unwrap();
    encoder.accept(upstream::GenerationEvent::Done).unwrap();
    let events = std::iter::from_fn(|| encoder.pop()).collect::<Vec<_>>();
    assert_eq!(
        events.iter().map(|event| event.name).collect::<Vec<_>>(),
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop"
        ]
    );
    assert_eq!(events[2].data["delta"]["type"], "input_json_delta");
    assert_eq!(events[4].data["delta"]["stop_reason"], "tool_use");
    assert_eq!(encoder.final_document()["usage"]["input_tokens"], 7);
}

#[test]
fn count_tokens_uses_private_recipe_tokenizer_and_native_shape() {
    let request = gateway::rewrite_anthropic_count_request(
        br#"{"model":"ornith","system":"rules","messages":[{"role":"user","content":"hello"}]}"#,
        "Ornith-1.5-9B",
    )
    .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&request).unwrap();
    assert_eq!(body["model"], "Ornith-1.5-9B");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(
        gateway::rewrite_anthropic_count_response(br#"{"count":11,"tokens":[1,2]}"#).unwrap(),
        11
    );
}

#[test]
fn claude_code_2_1_241_payload_is_an_exact_compatibility_fixture() {
    let request = gateway::rewrite_anthropic_request(
        br#"{"model":"ornith","messages":[{"role":"user","content":[{"type":"text","text":"hello","cache_control":{"type":"ephemeral"}}]}],"system":[{"type":"text","text":"rules","cache_control":{"type":"ephemeral"}}],"tools":[],"metadata":{"user_id":"fixture"},"max_tokens":32000,"thinking":{"type":"adaptive","display":"omitted"},"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},"output_config":{"effort":"high"},"stream":true}"#,
        "Ornith-1.5-9B",
    )
    .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["reasoning_effort"], "high");
    assert!(body.get("thinking").is_none());
    assert!(body.get("metadata").is_none());
}

#[test]
fn claude_code_2_1_241_trailing_system_message_is_promoted() {
    let request = gateway::rewrite_anthropic_request(
        br#"{"model":"ornith","system":[{"type":"text","text":"root rules"}],"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]},{"role":"system","content":"runtime context"}],"max_tokens":32000,"stream":true}"#,
        "Ornith-1.5-9B",
    )
    .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(
        body["messages"][0]["content"],
        "root rules\n\nruntime context"
    );
    assert_eq!(body["messages"][1]["role"], "user");
}

#[test]
fn claude_code_2_1_241_title_schema_becomes_openai_response_format() {
    let request = gateway::rewrite_anthropic_request(
        br#"{"model":"ornith","messages":[{"role":"user","content":"title this"}],"max_tokens":64,"output_config":{"effort":"high","format":{"type":"json_schema","schema":{"type":"object","properties":{"title":{"type":"string"}},"required":["title"]}}}}"#,
        "Ornith-1.5-9B",
    )
    .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(
        body["response_format"]["json_schema"]["schema"]["required"][0],
        "title"
    );
}

#[test]
fn malformed_pairing_oversize_image_and_unknown_content_fail_before_upstream() {
    for body in [
        br#"{"model":"ornith","messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"orphan","content":"x"}]}],"max_tokens":8}"#.as_slice(),
        br#"{"model":"ornith","messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"x"}}]}],"max_tokens":8}"#.as_slice(),
        br#"{"model":"ornith","messages":[{"role":"user","content":[{"type":"search_result","content":"x"}]}],"max_tokens":8}"#.as_slice(),
    ] {
        assert!(gateway::rewrite_anthropic_request(body, "Ornith-1.5-9B").is_err());
    }
    let oversized = serde_json::json!({"model":"ornith","messages":[{"role":"user",
        "content":"x".repeat(gateway::MAX_COMPLETION_BODY_BYTES)}],"max_tokens":8});
    assert!(gateway::rewrite_anthropic_request(
        &serde_json::to_vec(&oversized).unwrap(),
        "Ornith-1.5-9B"
    )
    .is_err());
}

#[test]
fn equivalent_tool_task_preserves_stop_and_usage() {
    let events = [
        upstream::GenerationEvent::ToolCallDelta {
            index: 0,
            call_id: Some("tool_1".into()),
            name: Some("lookup".into()),
            arguments: r#"{"q":"x"}"#.into(),
        },
        upstream::GenerationEvent::Usage {
            prompt_tokens: 5,
            completion_tokens: 2,
        },
        upstream::GenerationEvent::Finished {
            finish_reason: Some("tool_calls".into()),
        },
        upstream::GenerationEvent::Done,
    ];
    let mut openai = gateway::ResponsesEncoder::new("ornith".into(), Default::default());
    let mut anthropic = gateway::AnthropicEncoder::new("ornith".into());
    for event in events {
        openai.accept(event.clone()).unwrap();
        anthropic.accept(event).unwrap();
    }
    assert_eq!(openai.final_document()["usage"]["total_tokens"], 7);
    assert_eq!(anthropic.final_document()["stop_reason"], "tool_use");
    assert_eq!(anthropic.final_document()["content"][0]["name"], "lookup");
}

#[test]
fn anthropic_stream_and_document_share_thinking_signature_text_and_usage() {
    let mut encoder = gateway::AnthropicEncoder::new("fixture".into());
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
    let events = std::iter::from_fn(|| encoder.pop()).collect::<Vec<_>>();
    let signature = &encoder.final_document()["content"][0]["signature"];
    assert!(events
        .iter()
        .any(|event| event.data["delta"]["signature"] == *signature));
    assert_eq!(encoder.final_document()["usage"]["output_tokens"], 3);
}

#[test]
fn anthropic_stream_preserves_two_parallel_tool_inputs() {
    let mut encoder = gateway::AnthropicEncoder::new("fixture".into());
    for (index, id, name) in [(0, "tool_1", "lookup"), (1, "tool_2", "patch")] {
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
    let content = encoder.final_document()["content"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(
        content.iter().map(|item| &item["id"]).collect::<Vec<_>>(),
        ["tool_1", "tool_2"]
    );
}

#[test]
fn anthropic_follow_up_accepts_its_signed_thinking_block() {
    let mut encoder = gateway::AnthropicEncoder::new("fixture".into());
    encoder
        .accept(upstream::GenerationEvent::ReasoningDelta {
            text: "inspect".into(),
        })
        .unwrap();
    encoder
        .accept(upstream::GenerationEvent::TextDelta {
            text: "answer".into(),
        })
        .unwrap();
    encoder.accept(upstream::GenerationEvent::Done).unwrap();
    let body = serde_json::json!({"model":"fixture","max_tokens":8,"messages":[
        {"role":"user","content":"start"},
        {"role":"assistant","content":encoder.final_document()["content"]},
        {"role":"user","content":"continue"}]});
    let rewritten =
        gateway::rewrite_anthropic_request(&serde_json::to_vec(&body).unwrap(), "served").unwrap();
    let upstream: serde_json::Value = serde_json::from_slice(&rewritten.body).unwrap();
    assert_eq!(upstream["messages"][1]["reasoning_content"], "inspect");
}

#[test]
#[ignore = "set SY_SPARK_CLAUDE_BIN to the pinned Claude Code 2.1.241 binary"]
fn pinned_claude_code_2_1_241_completes_one_streamed_client_tool_round_trip() {
    let binary = std::env::var("SY_SPARK_CLAUDE_BIN").expect("exact Claude Code binary path");
    let version = Command::new(&binary).arg("--version").output().unwrap();
    assert!(String::from_utf8_lossy(&version.stdout).contains("2.1.241"));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut first, body) = accept_anthropic_post(&listener);
        gateway::rewrite_anthropic_request(body.as_bytes(), "Ornith-1.5-9B").unwrap();
        write_anthropic_sse(
            &mut first,
            &[
                serde_json::json!({"type":"message_start","message":{"id":"msg_fixture","type":"message","role":"assistant","content":[],"model":"ornith","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":0}}}),
                serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tool_fixture","name":"Bash","input":{}}}),
                serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"printf fixture-tool-result\"}"}}),
                serde_json::json!({"type":"content_block_stop","index":0}),
                serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":2}}),
                serde_json::json!({"type":"message_stop"}),
            ],
        );
        let (mut second, continuation) = accept_anthropic_post(&listener);
        gateway::rewrite_anthropic_request(continuation.as_bytes(), "Ornith-1.5-9B").unwrap();
        write_anthropic_sse(
            &mut second,
            &[
                serde_json::json!({"type":"message_start","message":{"id":"msg_done","type":"message","role":"assistant","content":[],"model":"ornith","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":3,"output_tokens":0}}}),
                serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"fixture complete"}}),
                serde_json::json!({"type":"content_block_stop","index":0}),
                serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2}}),
                serde_json::json!({"type":"message_stop"}),
            ],
        );
        continuation
    });
    let output = Command::new(binary)
        .args([
            "--bare",
            "--safe-mode",
            "--print",
            "--output-format",
            "json",
            "--no-session-persistence",
            "--tools",
            "Bash",
            "--allowedTools",
            "Bash",
            "--dangerously-skip-permissions",
            "Use Bash exactly once to run printf fixture-tool-result, then say fixture complete.",
        ])
        .env("ANTHROPIC_BASE_URL", format!("http://{address}"))
        .env("ANTHROPIC_API_KEY", "fixture-only-token")
        .env("ANTHROPIC_MODEL", "ornith")
        .env("ANTHROPIC_SMALL_FAST_MODEL", "ornith")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", "1")
        .env("CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let continuation = server.join().unwrap();
    assert!(continuation.contains("tool_result") && continuation.contains("tool_fixture"));
}
