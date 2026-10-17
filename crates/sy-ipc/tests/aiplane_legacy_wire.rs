use serde_json::{Value, json};
use sy_core::Priority;
use sy_ipc::aiplane::{Op, Req, Resp, SearchFilter};

#[test]
fn workload_request_keeps_legacy_json_shape() {
    let request: Req = serde_json::from_value(json!({
        "req": "run", "workload": "embed", "input": {"kind": "text", "text": "hello"}
    }))
    .unwrap();
    let encoded = serde_json::to_value(request).unwrap();
    assert_eq!(encoded["req"], "run");
    assert_eq!(encoded["workload"], "embed");
    assert_eq!(encoded["input"], json!({"kind": "text", "text": "hello"}));
}

#[test]
fn old_search_request_defaults_priority_and_optional_filter() {
    let request: Req = serde_json::from_value(json!({
        "req": "search", "query": "test", "limit": 4
    }))
    .unwrap();
    let Req::Search {
        priority, filter, ..
    } = request
    else {
        panic!("expected search request");
    };
    assert_eq!(priority, Priority::Interactive);
    assert!(filter.is_none());
}

#[test]
fn old_search_response_defaults_confidence() {
    let response: Resp = serde_json::from_value(json!({"resp": "search", "hits": []})).unwrap();
    let Resp::Search {
        confidence,
        abstained,
        ..
    } = response
    else {
        panic!("expected search response");
    };
    assert_eq!(confidence, 1.0);
    assert!(!abstained);
}

#[test]
fn control_operation_and_empty_filter_keep_wire_names() {
    assert_eq!(
        serde_json::to_value(Op::IndexNow).unwrap(),
        json!({"op": "index-now"})
    );
    let filter = serde_json::to_value(SearchFilter::default()).unwrap();
    assert_eq!(filter, Value::Object(Default::default()));
}
