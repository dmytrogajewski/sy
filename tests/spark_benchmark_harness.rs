use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    process::Command,
};

fn synthetic_result(engine: &str) -> serde_json::Value {
    let mut result = serde_json::json!({"schema":"sy.spark.benchmark-result/v1","mode":"live","plan_sha256":"sha256:plan","sampling_sha256":"sha256:sampling","model_fingerprint":"sha256:model","engine_fingerprint":engine,"image_digest":"sha256:image","profile_fingerprint":"sha256:profile","samples":[{"workload_id":"synthetic","kind":"synthetic","sample_index":0,"ttft_ms":1.0,"total_ms":2.0,"client_input_tokens_per_second_estimate":1000.0,"client_decode_tokens_per_second":1000.0,"input_tokens":1,"output_tokens":1,"total_tokens":2,"reasoning_events":0,"tool_events":0,"terminal_event":"response.completed","generated_sha256":"sha256:generated"}]});
    result["plan_shape"] = serde_json::json!({"warmup_samples":1,"measured_samples":1,"workloads":[{"id":"synthetic","kind":"synthetic"}]});
    result
}

fn compare_output(first: &serde_json::Value, second: &serde_json::Value) -> std::process::Output {
    let temp = tempfile::tempdir().unwrap();
    let left = temp.path().join("left.json");
    let right = temp.path().join("right.json");
    std::fs::write(&left, serde_json::to_vec(first).unwrap()).unwrap();
    std::fs::write(&right, serde_json::to_vec(second).unwrap()).unwrap();
    Command::new("python3")
        .arg("scripts/benchmark-spark-engine.py")
        .arg("--compare")
        .arg(left)
        .arg(right)
        .output()
        .unwrap()
}

#[test]
fn paired_plan_fixes_ten_samples_and_separate_workload_kinds() {
    let raw = std::fs::read("tests/fixtures/spark-benchmark/paired.json").unwrap();
    let plan: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let kinds: std::collections::BTreeSet<_> = plan["workloads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|workload| workload["kind"].as_str().unwrap())
        .collect();
    let request_sequences: Vec<_> = plan["workloads"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|workload| workload["requests"].as_array())
        .collect();
    let fixture_owns_timeouts = plan["workloads"]
        .as_array()
        .unwrap()
        .iter()
        .all(|workload| workload["timeout_seconds"].is_u64());
    assert!(
        plan["measured_samples"] == 10
            && kinds.len() == 5
            && request_sequences.len() == 2
            && request_sequences
                .iter()
                .all(|requests| requests.len() == 11)
            && fixture_owns_timeouts
    );
}

#[test]
fn dry_run_accepts_fixture_owned_request_sequences() {
    let output = Command::new("python3")
        .args([
            "scripts/benchmark-spark-engine.py",
            "--fixture",
            "tests/fixtures/spark-benchmark/paired.json",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cold_prefill_plan_is_bounded_and_disjoint() {
    let raw = std::fs::read("tests/fixtures/spark-benchmark/cold.json").unwrap();
    let plan: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let workloads = plan["workloads"].as_array().unwrap();
    let prefixes: std::collections::BTreeSet<_> = workloads
        .iter()
        .map(|workload| {
            workload["request"]["input_template"]["prefix"]
                .as_str()
                .unwrap()
        })
        .collect();
    assert!(
        plan["warmup_samples"] == 0
            && plan["measured_samples"] == 1
            && workloads.len() == 3
            && prefixes.len() == 3
    );
}

#[test]
fn plan_rejects_per_workload_sampling_overrides() {
    let temp = tempfile::tempdir().unwrap();
    let raw = std::fs::read("tests/fixtures/spark-benchmark/paired.json").unwrap();
    let mut plan: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    plan["workloads"][0]["request"]["temperature"] = 1.into();
    let fixture = temp.path().join("mixed.json");
    std::fs::write(&fixture, serde_json::to_vec(&plan).unwrap()).unwrap();
    let output = Command::new("python3")
        .args(["scripts/benchmark-spark-engine.py", "--fixture"])
        .arg(fixture)
        .arg("--dry-run")
        .output()
        .unwrap();
    assert!(
        !output.status.success() && String::from_utf8_lossy(&output.stderr).contains("sampling")
    );
}

#[test]
fn plan_rejects_invalid_fixture_owned_input_template() {
    let temp = tempfile::tempdir().unwrap();
    let mut plan: serde_json::Value = serde_json::from_slice(
        &std::fs::read("tests/fixtures/spark-benchmark/events.json").unwrap(),
    )
    .unwrap();
    plan["workloads"][0]["requests"][0]["input_template"]["repetitions"] = 0.into();
    let fixture = temp.path().join("invalid.json");
    std::fs::write(&fixture, serde_json::to_vec(&plan).unwrap()).unwrap();
    let output = Command::new("python3")
        .args(["scripts/benchmark-spark-engine.py", "--fixture"])
        .arg(fixture)
        .arg("--dry-run")
        .output()
        .unwrap();
    assert!(
        !output.status.success()
            && String::from_utf8_lossy(&output.stderr).contains("input template")
    );
}

#[test]
fn dry_fixture_run_is_byte_stable() {
    let run = || {
        Command::new("python3")
            .args([
                "scripts/benchmark-spark-engine.py",
                "--fixture",
                "tests/fixtures/spark-benchmark/smoke.json",
                "--dry-run",
            ])
            .output()
            .unwrap()
    };
    let first = run();
    let second = run();
    let result: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert!(
        first.status.success()
            && first.stdout == second.stdout
            && result["sampling_sha256"].as_str().is_some()
            && !String::from_utf8_lossy(&first.stdout).contains("Reply with OK")
    );
}

#[test]
fn live_run_parses_fragmented_sse_without_retaining_generated_text() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        for connection in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut content_length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length: ") {
                    content_length = value.trim().parse().unwrap();
                }
            }
            let mut request_body = vec![0; content_length];
            reader.read_exact(&mut request_body).unwrap();
            const EXPANDED_INPUT: &[u8] = b"Return a synthetic a synthetic fixture response.";
            if connection == 0 {
                assert!(request_body
                    .windows(EXPANDED_INPUT.len())
                    .any(|bytes| bytes == EXPANDED_INPUT));
            }
            assert!(!request_body
                .windows(14)
                .any(|bytes| bytes == b"input_template"));
            drop(reader);
            let body = "event: response.reasoning_summary_text.delta\ndata: {\"delta\":\"SECRET_REASONING\"}\n\nevent: response.output_text.delta\ndata: {\"delta\":\"SECRET_TEXT\"}\n\nevent: response.function_call_arguments.delta\ndata: {\"delta\":\"SECRET_ARGS\"}\n\nevent: response.output_item.done\ndata: {\"item\":{\"type\":\"function_call\",\"arguments\":\"SECRET_ARGS\"}}\n\nevent: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":4,\"total_tokens\":7}}}\n\n";
            if write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).is_err() {
            continue;
        }
            let split = body.len() / 2;
            if connection == 0 {
                stream.write_all(&body.as_bytes()[..split]).unwrap();
                stream.flush().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(20));
                let _ = stream.write_all(&body.as_bytes()[split..]);
            } else {
                let _ = stream.write_all(body.as_bytes());
            }
        }
    });
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("token"), "fixture-token").unwrap();
    std::fs::write(temp.path().join("metadata.json"), r#"{"model_fingerprint":"sha256:model","engine_fingerprint":"sha256:engine","image_digest":"sha256:image","profile_fingerprint":"sha256:profile","served_model":"fixture","responses_path":"/v1/responses"}"#).unwrap();
    std::fs::write(temp.path().join("observations.json"), r#"{"schema":"sy.spark.benchmark-observations/v1","native":{"mtp_acceptance_rate":0.75},"resources":{"mem_available_bytes":10000000000},"lifecycle":{"healthy":true}}"#).unwrap();
    let output = Command::new("python3")
        .args([
            "scripts/benchmark-spark-engine.py",
            "--fixture",
            "tests/fixtures/spark-benchmark/events.json",
            "--base-url",
            &format!("http://{address}"),
            "--bearer-file",
            temp.path().join("token").to_str().unwrap(),
            "--metadata",
            temp.path().join("metadata.json").to_str().unwrap(),
            "--observations",
            temp.path().join("observations.json").to_str().unwrap(),
            "--allow-http-loopback",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        result["samples"][0]["terminal_event"] == "response.completed"
            && result["summary"][0]["samples"] == 1
            && result["samples"][0]["reasoning_events"] == 1
            && result["samples"][0]["tool_events"] == 2
            && result["samples"][0]["event_counts"]["response.completed"] == 1
            && result["samples"][0]["generated_sha256"].as_str().is_some()
            && result["samples"][0]["request_sha256"].as_str().is_some()
            && result["samples"][0]["request_sha256"] != result["samples"][1]["request_sha256"]
            && result["samples"][0]["client_input_tokens_per_second_estimate"].is_number()
            && result["samples"][0]["client_prefill_tokens_per_second"].is_null()
            && result["samples"][1]["terminal_event"] == "client.cancelled"
            && result["samples"][1]["total_tokens"].is_null()
            && result["image_digest"] == "sha256:image"
            && result["profile_fingerprint"] == "sha256:profile"
            && result["sampling_sha256"].as_str().is_some()
            && result["external_observations"]["native"]["mtp_acceptance_rate"] == 0.75
            && result["mtp_acceptance_rate"].is_null()
            && !result.to_string().contains("SECRET_TEXT"),
        "{result}"
    );
}

#[test]
fn comparison_rejects_missing_immutable_identity() {
    let temp = tempfile::tempdir().unwrap();
    let incomplete = r#"{"schema":"sy.spark.benchmark-result/v1","mode":"live","samples":[]}"#;
    let left = temp.path().join("left.json");
    let right = temp.path().join("right.json");
    std::fs::write(&left, incomplete).unwrap();
    std::fs::write(&right, incomplete).unwrap();
    let output = Command::new("python3")
        .arg("scripts/benchmark-spark-engine.py")
        .arg("--compare")
        .arg(left)
        .arg(right)
        .output()
        .unwrap();
    assert!(
        !output.status.success() && String::from_utf8_lossy(&output.stderr).contains("identity")
    );
}

#[test]
fn comparison_rejects_mixed_sampling() {
    let first = synthetic_result("sha256:engine-a");
    let mut second = synthetic_result("sha256:engine-b");
    second["sampling_sha256"] = "sha256:other-sampling".into();
    let output = compare_output(&first, &second);
    assert!(
        !output.status.success() && String::from_utf8_lossy(&output.stderr).contains("sampling")
    );
}

#[test]
fn comparison_rejects_same_engine_identity() {
    let first = synthetic_result("sha256:engine");
    let output = compare_output(&first, &first);
    assert!(
        !output.status.success()
            && String::from_utf8_lossy(&output.stderr).contains("different engine")
    );
}

#[test]
fn comparison_rejects_unpaired_sample_indices() {
    let first = synthetic_result("sha256:engine-a");
    let mut second = synthetic_result("sha256:engine-b");
    second["samples"][0]["workload_id"] = "other".into();
    second["plan_shape"]["workloads"][0]["id"] = "other".into();
    let output = compare_output(&first, &second);
    assert!(
        !output.status.success() && String::from_utf8_lossy(&output.stderr).contains("unpaired")
    );
}

#[test]
fn comparison_rejects_shared_missing_samples() {
    let mut first = synthetic_result("sha256:engine-a");
    let mut second = synthetic_result("sha256:engine-b");
    first["plan_shape"]["measured_samples"] = 2.into();
    second["plan_shape"]["measured_samples"] = 2.into();
    let output = compare_output(&first, &second);
    assert!(
        !output.status.success() && String::from_utf8_lossy(&output.stderr).contains("missing")
    );
}

#[test]
fn comparison_rejects_incomparable_workload_kinds() {
    let first = synthetic_result("sha256:engine-a");
    let mut second = synthetic_result("sha256:engine-b");
    second["samples"][0]["kind"] = "different-kind".into();
    second["plan_shape"]["workloads"][0]["kind"] = "different-kind".into();
    let output = compare_output(&first, &second);
    assert!(!output.status.success() && String::from_utf8_lossy(&output.stderr).contains("kind"));
}

#[test]
fn comparison_rejects_non_monotonic_timing_and_inconsistent_usage() {
    let first = synthetic_result("sha256:engine-a");
    let mut invalid = synthetic_result("sha256:engine-b");
    invalid["samples"][0]["total_ms"] = 0.5.into();
    let timing = compare_output(&first, &invalid);
    invalid["samples"][0]["total_ms"] = 2.into();
    invalid["samples"][0]["total_tokens"] = 3.into();
    let usage = compare_output(&first, &invalid);
    assert!(!timing.status.success() && !usage.status.success());
}

#[test]
fn live_run_rejects_incomplete_immutable_metadata_before_connecting() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("token"), "synthetic").unwrap();
    std::fs::write(temp.path().join("metadata"), r#"{"model_fingerprint":"m","engine_fingerprint":"e","served_model":"synthetic","responses_path":"/v1/responses"}"#).unwrap();
    std::fs::write(temp.path().join("observations"), r#"{"schema":"sy.spark.benchmark-observations/v1","native":{},"resources":{},"lifecycle":{}}"#).unwrap();
    let output = Command::new("python3")
        .args([
            "scripts/benchmark-spark-engine.py",
            "--fixture",
            "tests/fixtures/spark-benchmark/smoke.json",
            "--base-url",
            "http://127.0.0.1:1",
            "--allow-http-loopback",
            "--bearer-file",
        ])
        .arg(temp.path().join("token"))
        .arg("--metadata")
        .arg(temp.path().join("metadata"))
        .arg("--observations")
        .arg(temp.path().join("observations"))
        .output()
        .unwrap();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success()
            && error.contains("benchmark metadata is incomplete")
            && !error.contains("Traceback")
    );
}
