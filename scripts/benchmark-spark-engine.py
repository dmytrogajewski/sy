#!/usr/bin/env python3
"""Run reproducible, engine-neutral Spark inference benchmarks."""

import argparse
import hashlib
import json
import math
import ssl
import statistics
import sys
import time
import urllib.parse
import urllib.request
from pathlib import Path


def document_sha256(value) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def validate_plan(plan: dict) -> None:
    required = {"schema", "warmup_samples", "measured_samples", "sampling", "workloads"}
    if set(plan) != required or plan["schema"] != "sy.spark.benchmark-plan/v1":
        raise ValueError("benchmark plan has an unsupported shape")
    if type(plan["warmup_samples"]) is not int or type(plan["measured_samples"]) is not int:
        raise ValueError("benchmark plan sample counts are invalid")
    if plan["warmup_samples"] < 0 or plan["measured_samples"] < 1 or not plan["workloads"]:
        raise ValueError("benchmark plan has no measured work")
    sampling = plan["sampling"]
    if not isinstance(sampling, dict) or type(sampling.get("max_output_tokens")) is not int or sampling["max_output_tokens"] < 1:
        raise ValueError("benchmark plan output ceiling is invalid")
    if {"model", "stream"} & set(sampling):
        raise ValueError("benchmark sampling cannot override transport identity")
    ids = [workload.get("id") for workload in plan["workloads"]]
    if any(not value for value in ids) or len(ids) != len(set(ids)):
        raise ValueError("benchmark workload identifiers are invalid")
    for workload in plan["workloads"]:
        if set(workload) - {"id", "kind", "request", "requests", "timeout_seconds", "cancel_after_events"}:
            raise ValueError("benchmark workload has unknown fields")
        if not isinstance(workload.get("kind"), str) or not workload["kind"]:
            raise ValueError("benchmark workload kind is invalid")
        if type(workload.get("timeout_seconds")) is not int or workload["timeout_seconds"] < 1:
            raise ValueError("benchmark workload timeout is invalid")
        if "cancel_after_events" in workload and (type(workload["cancel_after_events"]) is not int or workload["cancel_after_events"] < 1):
            raise ValueError("benchmark workload cancellation is invalid")
        if ("request" in workload) == ("requests" in workload):
            raise ValueError("benchmark workload must declare one request form")
        requests = [workload["request"]] if "request" in workload else workload["requests"]
        if not isinstance(requests, list) or not all(isinstance(request, dict) for request in requests):
            raise ValueError("benchmark workload request is missing")
        if "requests" in workload and len(requests) != plan["warmup_samples"] + plan["measured_samples"]:
            raise ValueError("benchmark workload request sequence length is invalid")
        if any({"model", "stream"} & set(request) for request in requests):
            raise ValueError("benchmark workload cannot override transport identity")
        if any(set(request) & set(plan["sampling"]) for request in requests):
            raise ValueError("benchmark workload cannot override plan sampling")
        for request in requests:
            expand_request(request)


def validate_metadata(metadata: dict) -> None:
    required = {
        "model_fingerprint", "engine_fingerprint", "image_digest",
        "profile_fingerprint", "served_model", "responses_path",
    }
    if set(metadata) != required or any(not metadata[key] for key in required):
        raise ValueError("benchmark metadata is incomplete")


def validate_observations(observations: dict) -> None:
    categories = {"native", "resources", "lifecycle"}
    if set(observations) != {"schema", *categories} or observations.get("schema") != "sy.spark.benchmark-observations/v1":
        raise ValueError("benchmark observations have an unsupported shape")
    for category in categories:
        metrics = observations[category]
        if not isinstance(metrics, dict) or any(not isinstance(key, str) or not key for key in metrics):
            raise ValueError("benchmark observation metric names are invalid")
        if any(type(value) not in {bool, float, int, type(None)} for value in metrics.values()):
            raise ValueError("benchmark observations may contain only scalar metrics")
        if any(type(value) is float and not math.isfinite(value) for value in metrics.values()):
            raise ValueError("benchmark observation metrics must be finite")


def sse_events(response):
    pending = b""
    while chunk := response.read1(65536):
        pending += chunk.replace(b"\r\n", b"\n")
        while b"\n\n" in pending:
            frame, pending = pending.split(b"\n\n", 1)
            event = "message"
            data = []
            for line in frame.decode("utf-8").splitlines():
                if line.startswith("event: "):
                    event = line[7:]
                elif line.startswith("data: "):
                    data.append(line[6:])
            if data and data != ["[DONE]"]:
                yield event, json.loads("\n".join(data)), time.monotonic_ns()


def expand_request(request: dict) -> dict:
    request = dict(request)
    if template := request.pop("input_template", None):
        fields = {"prefix", "unit", "repetitions", "suffix"}
        if not isinstance(template, dict) or set(template) != fields or "input" in request:
            raise ValueError("benchmark input template has an unsupported shape")
        text = (template["prefix"], template["unit"], template["suffix"])
        repetitions = template["repetitions"]
        if not all(isinstance(value, str) for value in text) or not template["unit"]:
            raise ValueError("benchmark input template text is invalid")
        if type(repetitions) is not int or not 1 <= repetitions <= 1_000_000:
            raise ValueError("benchmark input template repetitions are invalid")
        request["input"] = template["prefix"] + template["unit"] * template["repetitions"] + template["suffix"]
    return request


def run_sample(opener, url: str, token: str, metadata: dict, plan: dict, workload: dict, configured_request: dict) -> dict:
    configured_request = expand_request(configured_request)
    payload = {
        **plan["sampling"],
        **configured_request,
        "model": metadata["served_model"],
        "stream": True,
    }
    started = time.monotonic_ns()
    request = urllib.request.Request(
        url,
        data=json.dumps(payload, separators=(",", ":")).encode(),
        headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
    )
    first_delta = None
    usage = None
    terminal_event = None
    event_counts = {}
    reasoning_events = 0
    tool_events = 0
    generated = hashlib.sha256()
    event_count = 0
    cancelled = False
    with opener.open(request, timeout=workload["timeout_seconds"]) as response:
        for event, document, received in sse_events(response):
            event_count += 1
            event_counts[event] = event_counts.get(event, 0) + 1
            delta = document.get("delta")
            if isinstance(delta, str):
                generated.update(event.encode() + b"\0" + delta.encode() + b"\0")
            if first_delta is None and delta not in (None, ""):
                first_delta = received
            reasoning_events += "reasoning" in event
            tool_events += (
                "function_call" in event
                or document.get("item", {}).get("type") == "function_call"
            )
            if event in {"response.completed", "response.incomplete"}:
                usage = document.get("response", {}).get("usage")
                terminal_event = event
            elif event == "response.failed":
                raise ValueError("benchmark generation failed")
            if event_count == workload.get("cancel_after_events"):
                terminal_event = "client.cancelled"
                cancelled = True
                break
    finished = time.monotonic_ns()
    if first_delta is None or terminal_event is None:
        raise ValueError("benchmark stream is missing delta or terminal event")
    if usage is None:
        if not cancelled:
            raise ValueError("benchmark terminal event is missing usage")
        usage = {"input_tokens": None, "output_tokens": None, "total_tokens": None}
    ttft_ms = (first_delta - started) / 1_000_000
    total_ms = (finished - started) / 1_000_000
    decode_ms = total_ms - ttft_ms
    return {
        "workload_id": workload["id"],
        "kind": workload["kind"],
        "request_sha256": document_sha256(configured_request),
        "ttft_ms": round(ttft_ms, 3),
        "total_ms": round(total_ms, 3),
        "client_input_tokens_per_second_estimate": None if cancelled else round(usage["input_tokens"] * 1000 / ttft_ms, 3),
        "client_decode_tokens_per_second": None if cancelled else round(usage["output_tokens"] * 1000 / decode_ms, 3),
        "input_tokens": usage["input_tokens"],
        "output_tokens": usage["output_tokens"],
        "total_tokens": usage["total_tokens"],
        "reasoning_events": reasoning_events,
        "tool_events": tool_events,
        "event_counts": dict(sorted(event_counts.items())),
        "terminal_event": terminal_event,
        "generated_sha256": generated.hexdigest(),
    }


def summarize(samples: list[dict]) -> list[dict]:
    groups = {}
    for sample in samples:
        groups.setdefault(sample["workload_id"], []).append(sample)
    output = []
    for workload_id, group in groups.items():
        metric = lambda name: sorted(sample[name] for sample in group)
        summary = {"workload_id": workload_id, "kind": group[0]["kind"], "samples": len(group)}
        for name in ("ttft_ms", "client_decode_tokens_per_second", "client_input_tokens_per_second_estimate"):
            values = metric(name)
            summary[name] = {"min": values[0], "median": statistics.median(values), "p95": values[math.ceil(len(values) * 0.95) - 1], "max": values[-1]}
        summary["tool_event_samples"] = sum(sample["tool_events"] > 0 for sample in group)
        summary["terminal_events"] = {event: sum(sample["terminal_event"] == event for sample in group) for event in sorted({sample["terminal_event"] for sample in group})}
        output.append(summary)
    return output


def validate_result(result: dict) -> None:
    identity = ("plan_sha256", "sampling_sha256", "model_fingerprint", "engine_fingerprint", "image_digest", "profile_fingerprint")
    if result.get("schema") != "sy.spark.benchmark-result/v1" or result.get("mode") != "live":
        raise ValueError("benchmark result schema is invalid")
    if any(not result.get(key) for key in identity):
        raise ValueError("benchmark identity is incomplete")
    if not result.get("samples"):
        raise ValueError("benchmark result has no samples")
    plan_shape = result.get("plan_shape", {})
    measured = plan_shape.get("measured_samples")
    workloads = plan_shape.get("workloads")
    if type(measured) is not int or measured < 1 or not isinstance(workloads, list):
        raise ValueError("benchmark result plan shape is invalid")
    expected = {(workload.get("id"), index): workload.get("kind") for workload in workloads for index in range(measured)}
    actual = {(sample.get("workload_id"), sample.get("sample_index")): sample.get("kind") for sample in result["samples"]}
    if actual != expected or len(actual) != len(result["samples"]):
        raise ValueError("benchmark result has missing or duplicate samples")
    for sample in result["samples"]:
        if sample["total_ms"] < sample["ttft_ms"] or sample["ttft_ms"] < 0:
            raise ValueError("benchmark timing is non-monotonic")
        if sample["total_tokens"] != sample["input_tokens"] + sample["output_tokens"]:
            raise ValueError("benchmark token usage is inconsistent")


def compare_results(first: dict, second: dict) -> dict:
    validate_result(first)
    validate_result(second)
    if first["plan_sha256"] != second["plan_sha256"] or first["model_fingerprint"] != second["model_fingerprint"]:
        raise ValueError("benchmark pair does not share one plan and model identity")
    if first["sampling_sha256"] != second["sampling_sha256"]:
        raise ValueError("benchmark pair has mixed sampling")
    if first["engine_fingerprint"] == second["engine_fingerprint"]:
        raise ValueError("benchmark pair must use different engine identities")
    sample_identity = lambda sample: (sample.get("workload_id"), sample.get("sample_index"))
    first_samples = {sample_identity(sample) for sample in first["samples"]}
    second_samples = {sample_identity(sample) for sample in second["samples"]}
    if first_samples != second_samples or len(first_samples) != len(first["samples"]) or len(second_samples) != len(second["samples"]):
        raise ValueError("benchmark pair has unpaired samples")
    first_kinds = {sample_identity(sample): sample.get("kind") for sample in first["samples"]}
    second_kinds = {sample_identity(sample): sample.get("kind") for sample in second["samples"]}
    if first_kinds != second_kinds:
        raise ValueError("benchmark pair has incomparable workload kinds")
    first_summary = {entry["workload_id"]: entry for entry in summarize(first["samples"])}
    second_summary = {entry["workload_id"]: entry for entry in summarize(second["samples"])}
    if first_summary.keys() != second_summary.keys() or any(first_summary[key]["samples"] != second_summary[key]["samples"] for key in first_summary):
        raise ValueError("benchmark pair has unpaired workloads")
    workloads = []
    for key in first_summary:
        left = first_summary[key]
        right = second_summary[key]
        workloads.append({"workload_id": key, "first": left, "second": right, "second_over_first_decode": round(right["client_decode_tokens_per_second"]["median"] / left["client_decode_tokens_per_second"]["median"], 6), "second_over_first_ttft": round(right["ttft_ms"]["median"] / left["ttft_ms"]["median"], 6)})
    return {"schema": "sy.spark.benchmark-comparison/v1", "plan_sha256": first["plan_sha256"], "model_fingerprint": first["model_fingerprint"], "first_engine_fingerprint": first["engine_fingerprint"], "second_engine_fingerprint": second["engine_fingerprint"], "workloads": workloads}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixture", type=Path)
    parser.add_argument("--compare", nargs=2, type=Path)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--base-url")
    parser.add_argument("--bearer-file", type=Path)
    parser.add_argument("--metadata", type=Path)
    parser.add_argument("--observations", type=Path)
    parser.add_argument("--ca", type=Path)
    parser.add_argument("--allow-http-loopback", action="store_true")
    args = parser.parse_args()
    if args.compare:
        try:
            result = compare_results(*(json.loads(path.read_bytes()) for path in args.compare))
        except ValueError as error:
            parser.error(str(error))
        print(json.dumps(result, sort_keys=True, separators=(",", ":")))
        return
    if not args.fixture:
        parser.error("a fixture is required outside comparison mode")
    raw = args.fixture.read_bytes()
    plan = json.loads(raw)
    validate_plan(plan)
    plan_hash = hashlib.sha256(raw).hexdigest()
    planned = len(plan["workloads"]) * (plan["warmup_samples"] + plan["measured_samples"])
    if args.dry_run:
        result = {"schema": "sy.spark.benchmark-result/v1", "mode": "dry-run", "plan_sha256": plan_hash, "sampling_sha256": document_sha256(plan["sampling"]), "planned_requests": planned, "workload_ids": [workload["id"] for workload in plan["workloads"]]}
        print(json.dumps(result, sort_keys=True, separators=(",", ":")))
        return
    if not args.base_url or not args.bearer_file or not args.metadata or not args.observations:
        parser.error("live mode requires base URL, bearer file, metadata, and observations")
    metadata = json.loads(args.metadata.read_bytes())
    validate_metadata(metadata)
    observations = json.loads(args.observations.read_bytes())
    validate_observations(observations)
    parsed = urllib.parse.urlparse(args.base_url)
    loopback = parsed.hostname in {"127.0.0.1", "::1", "localhost"}
    if parsed.scheme == "http" and not (loopback and args.allow_http_loopback):
        parser.error("plaintext HTTP is restricted to an explicitly allowed loopback fixture")
    if parsed.scheme not in {"http", "https"}:
        parser.error("base URL must use HTTP or HTTPS")
    context = ssl.create_default_context(cafile=str(args.ca)) if args.ca else None
    opener = urllib.request.build_opener(urllib.request.HTTPSHandler(context=context))
    url = args.base_url.rstrip("/") + metadata["responses_path"]
    token = args.bearer_file.read_text(encoding="utf-8").strip()
    samples = []
    for workload in plan["workloads"]:
        requests = workload.get("requests") or [workload["request"]] * (plan["warmup_samples"] + plan["measured_samples"])
        for configured_request in requests[:plan["warmup_samples"]]:
            run_sample(opener, url, token, metadata, plan, workload, configured_request)
        for sample_index, configured_request in enumerate(requests[plan["warmup_samples"]:]):
            sample = run_sample(opener, url, token, metadata, plan, workload, configured_request)
            sample["sample_index"] = sample_index
            samples.append(sample)
    external_observations = {key: observations[key] for key in ("native", "resources", "lifecycle")}
    plan_shape = {"warmup_samples": plan["warmup_samples"], "measured_samples": plan["measured_samples"], "workloads": [{"id": workload["id"], "kind": workload["kind"]} for workload in plan["workloads"]]}
    result = {"schema": "sy.spark.benchmark-result/v1", "mode": "live", "plan_sha256": plan_hash, "sampling_sha256": document_sha256(plan["sampling"]), "plan_shape": plan_shape, "model_fingerprint": metadata["model_fingerprint"], "engine_fingerprint": metadata["engine_fingerprint"], "image_digest": metadata["image_digest"], "profile_fingerprint": metadata["profile_fingerprint"], "external_observations_sha256": document_sha256(observations), "external_observations": external_observations, "samples": samples, "summary": summarize(samples)}
    print(json.dumps(result, sort_keys=True, separators=(",", ":")))


if __name__ == "__main__":
    try:
        main()
    except (KeyError, OSError, TypeError, ValueError) as error:
        print(f"benchmark: {error}", file=sys.stderr)
        raise SystemExit(1) from None
