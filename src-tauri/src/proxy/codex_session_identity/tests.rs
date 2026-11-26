use super::*;
use serde_json::json;

fn request(thread: &str, turn: &str) -> (HeaderMap, Value) {
    let mut headers = HeaderMap::new();
    headers.insert(
        "session_id",
        HeaderValue::from_static("shared-process-session"),
    );
    headers.insert("thread-id", HeaderValue::from_str(thread).unwrap());
    headers.insert(
        "x-codex-turn-state",
        HeaderValue::from_static("old-route-credential"),
    );
    headers.insert(
        "x-codex-window-id",
        HeaderValue::from_str(&format!("{thread}:2")).unwrap(),
    );
    headers.insert(
        "x-codex-turn-metadata",
        HeaderValue::from_str(
            &json!({"thread_id":thread,"session_id":"shared-process-session","turn_id":turn})
                .to_string(),
        )
        .unwrap(),
    );
    (
        headers,
        json!({
            "model":"test-model", "stream":true,
            "metadata":{"session_id":"shared-process-session","thread_id":thread,"turn_id":turn,"turn_state":"old-body-state"},
            "client_metadata":{"session-id":"shared-process-session","thread-id":thread,"window_id":format!("{thread}:2")},
            "prompt_cache_key":thread,
            "previous_response_id":"resp-original", "conversation":{"id":"conversation-original"},
            "input":[
                {"role":"user","content":[{"type":"input_text","text":"完整消息"}]},
                {"type":"reasoning","encrypted_content":"opaque-encrypted-reasoning"},
                {"type":"compaction","encrypted_content":"opaque-encrypted-compaction"},
                {"type":"function_call","call_id":"call-original","turn_id":"tool-turn-original","name":"run","arguments":"{\"session_id\":\"tool-session\",\"thread_id\":\"tool-thread\"}"},
                {"type":"function_call_output","call_id":"call-original","output":"原始工具输出"}
            ],
            "tools":[{"type":"function","name":"run","parameters":{"type":"object","properties":{"session_id":{"type":"string"}}}}]
        }),
    )
}

#[test]
fn original_generation_is_exactly_unchanged() {
    let store = Mutex::new(Store::default());
    let (mut headers, mut body) = request("thread-a", "turn-a");
    let old = (headers.clone(), body.clone());
    let lease = acquire_from(&store, "provider-a", &headers, &body).unwrap();
    assert_eq!(lease.snapshot().generation(), 0);
    let report = lease.apply(&lease.snapshot(), &mut headers, &mut body);
    assert_eq!((headers, body), old);
    assert_eq!(report.identity_hashes.len(), 2);
    assert!(report.identity_values >= 8);
}

fn encoded_metadata_request(thread: &str, turn: &str) -> Value {
    json!({
        "model":"test-model",
        "stream":true,
        "client_metadata":{
            "x-codex-turn-metadata":format!(" {{ \"session_id\":\"process-session\", \"thread_id\":\"{thread}\", \"turn_id\":\"{turn}\", \"window_id\":\"{thread}:5\", \"turn_state\":\"old-body-state\", \"custom\":\"保留原字段\", \"root_turn_id\":\"root-turn-original\", \"context_window_id\":\"context-window-original\", \"installation_id\":\"installation-original\" }} "),
            "other":"保留外层字段",
            "root_turn_id":"root-turn-original",
            "x-codex-installation-id":"installation-original"
        },
        "prompt_cache_key":thread,
        "previous_response_id":"resp-original",
        "conversation":{"id":"conversation-original"},
        "input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"完整历史"}]},
            {"type":"reasoning","id":"rs-original","encrypted_content":"opaque-reasoning"},
            {"type":"function_call","call_id":"call-original","name":"run","arguments":"{\"client_metadata\":{\"x-codex-turn-metadata\":\"not-json\"}}"},
            {"type":"function_call_output","call_id":"call-original","output":"完整工具输出"}
        ],
        "tools":[{"type":"function","name":"run","parameters":{"type":"object"}}]
    })
}

#[test]
fn encoded_body_metadata_is_acquired_audited_and_original_generation_is_unchanged() {
    let store = Mutex::new(Store::default());
    let mut headers = HeaderMap::new();
    let mut body = encoded_metadata_request("body-thread-a", "turn-a");
    let original = body.clone();
    let lease = acquire_from(&store, "provider-a", &headers, &body).unwrap();
    assert!(lease.turn.is_some());
    let report = lease.apply(&lease.snapshot(), &mut headers, &mut body);
    assert_eq!(body, original, "第 0 代连 JSON 字符串空白也原样保留");
    assert!(headers.is_empty());
    assert_eq!(report.identity_hashes.len(), 2);
    assert_eq!(report.identity_values, 2);
    assert!(lease.aliases.contains("body-thread-a"));
    assert!(lease.aliases.contains("process-session"));

    // 同一进程的不同线程不能仅因编码在字符串中就合并映射。
    let peer = acquire_from(
        &store,
        "provider-a",
        &HeaderMap::new(),
        &encoded_metadata_request("body-thread-b", "turn-a"),
    )
    .unwrap();
    assert_ne!(lease.key_hash, peer.key_hash);
}

#[test]
fn encoded_body_metadata_rotates_with_headers_cache_and_complete_history() {
    let store = Mutex::new(Store::default());
    let mut headers = HeaderMap::new();
    headers.insert("session-id", HeaderValue::from_static("process-session"));
    headers.insert("thread-id", HeaderValue::from_static("body-thread-a"));
    headers.insert("x-codex-turn-state", HeaderValue::from_static("old-route"));
    let mut body = encoded_metadata_request("body-thread-a", "turn-a");
    let original = body.clone();
    let lease = acquire_from(&store, "provider-a", &headers, &body).unwrap();
    let snapshot = lease.rotate(&lease.snapshot());
    let identity = snapshot.identity.as_deref().unwrap();
    let report = lease.apply(&snapshot, &mut headers, &mut body);
    let metadata: Value = serde_json::from_str(
        body["client_metadata"][ENCODED_TURN_METADATA]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    for name in ["session_id", "session-id", "thread-id"] {
        assert_eq!(headers[name], identity);
    }
    assert_eq!(metadata["session_id"], identity);
    assert_eq!(metadata["thread_id"], identity);
    assert_eq!(metadata["window_id"], format!("{identity}:5"));
    assert_eq!(metadata["turn_id"], "turn-a");
    assert_eq!(metadata["custom"], "保留原字段");
    assert_eq!(metadata["root_turn_id"], "root-turn-original");
    assert_eq!(metadata["context_window_id"], "context-window-original");
    assert_eq!(metadata["installation_id"], "installation-original");
    assert_eq!(body["client_metadata"]["other"], "保留外层字段");
    assert_eq!(
        body["client_metadata"]["root_turn_id"],
        "root-turn-original"
    );
    assert_eq!(
        body["client_metadata"]["x-codex-installation-id"],
        "installation-original"
    );
    assert_eq!(body["prompt_cache_key"], identity);
    assert!(metadata.get("turn_state").is_none());
    assert!(!headers.contains_key("x-codex-turn-state"));
    assert_eq!(report.identity_hashes.len(), 1);
    assert_eq!(report.identity_values, 5);
    assert_eq!(report.states_removed, 2);
    for field in [
        "input",
        "tools",
        "previous_response_id",
        "conversation",
        "model",
        "stream",
    ] {
        assert_eq!(body[field], original[field], "{field}");
    }
}

#[test]
fn encoded_body_turn_metadata_participates_in_route_state_isolation() {
    let store = Mutex::new(Store::default());
    let headers = HeaderMap::new();
    let body = encoded_metadata_request("body-thread-a", "turn-a");
    let lease = acquire_from(&store, "provider-a", &headers, &body).unwrap();
    let snapshot = lease.rotate(&lease.snapshot());
    let mut response = HeaderMap::new();
    response.insert(
        "x-codex-turn-state",
        HeaderValue::from_static("new-route-a"),
    );
    lease.observe(&snapshot, &response);
    let mut same_headers = HeaderMap::new();
    let mut same_body = body.clone();
    lease.apply(&snapshot, &mut same_headers, &mut same_body);
    assert_eq!(same_headers["x-codex-turn-state"], "new-route-a");

    let peer_body = encoded_metadata_request("body-thread-a", "turn-b");
    let peer = acquire_from(&store, "provider-a", &headers, &peer_body).unwrap();
    let mut peer_headers = HeaderMap::new();
    let mut peer_body = peer_body;
    peer.apply(&snapshot, &mut peer_headers, &mut peer_body);
    assert!(!peer_headers.contains_key("x-codex-turn-state"));

    let mut conflicting = HeaderMap::new();
    conflicting.insert("turn-id", HeaderValue::from_static("turn-b"));
    assert!(lease.for_outbound(&conflicting, &body).turn.is_none());
}

#[test]
fn invalid_or_oversized_encoded_body_metadata_is_skipped_without_changing_history() {
    let store = Mutex::new(Store::default());
    let mut headers = HeaderMap::new();
    headers.insert("thread-id", HeaderValue::from_static("body-thread-a"));
    for encoded in [
        json!("not-json"),
        json!("[]"),
        json!("null"),
        json!(null),
        json!({"thread_id":"object-instead-of-string"}),
        json!(format!(
            "{{\"thread_id\":\"{}\"}}",
            "a".repeat(METADATA_BYTES)
        )),
    ] {
        let mut body = encoded_metadata_request("body-thread-a", "turn-a");
        body["client_metadata"][ENCODED_TURN_METADATA] = encoded;
        let original = body.clone();
        assert!(acquire_from(&store, "provider-a", &headers, &body).is_none());
        assert_eq!(body, original);
    }
}

#[test]
fn encoded_body_metadata_obeys_the_shared_depth_bound() {
    let store = Mutex::new(Store::default());
    let mut body =
        json!({"x-codex-turn-metadata":json!({"thread_id":"body-thread-a"}).to_string()});
    for _ in 0..METADATA_DEPTH - 1 {
        body = json!({"metadata":body});
    }
    assert!(acquire_from(&store, "provider-a", &HeaderMap::new(), &body).is_some());
    body = json!({"metadata":body});
    assert!(acquire_from(&store, "provider-a", &HeaderMap::new(), &body).is_none());
}

#[test]
fn invalid_final_encoded_metadata_does_not_partially_rotate_headers() {
    let store = Mutex::new(Store::default());
    let mut headers = HeaderMap::new();
    headers.insert("thread-id", HeaderValue::from_static("body-thread-a"));
    let mut body = encoded_metadata_request("body-thread-a", "turn-a");
    let lease = acquire_from(&store, "provider-a", &headers, &body).unwrap();
    let snapshot = lease.rotate(&lease.snapshot());
    body["client_metadata"][ENCODED_TURN_METADATA] = json!("unknown-final-format");
    let original = (headers.clone(), body.clone());
    let report = lease.apply(&snapshot, &mut headers, &mut body);
    assert_eq!((headers, body), original);
    assert_eq!(
        (report.headers, report.fields, report.states_removed),
        (0, 0, 0)
    );
}

#[test]
fn rotation_rewrites_final_identity_and_preserves_all_context_and_refs() {
    let store = Mutex::new(Store::default());
    let (mut headers, mut body) = request("thread-a", "turn-a");
    let original = body.clone();
    let lease = acquire_from(&store, "provider-a", &headers, &body).unwrap();
    // 模拟后续本地头覆盖及 Stack 过滤：最终阶段必须修正并恢复必要字段。
    headers.insert(
        "session-id",
        HeaderValue::from_static("configured-stale-session"),
    );
    headers.remove("x-codex-window-id");
    headers.remove("x-codex-turn-metadata");
    let snapshot = lease.rotate(&lease.snapshot());
    let report = lease.apply(&snapshot, &mut headers, &mut body);
    let identity = snapshot.identity.as_deref().unwrap();
    for name in ["session_id", "session-id", "thread-id"] {
        assert_eq!(headers[name], identity);
    }
    assert_eq!(headers["x-codex-window-id"], format!("{identity}:2"));
    let metadata: Value =
        serde_json::from_str(headers["x-codex-turn-metadata"].to_str().unwrap()).unwrap();
    assert_eq!(metadata["session_id"], identity);
    assert_eq!(metadata["thread_id"], identity);
    assert_eq!(metadata["turn_id"], "turn-a");
    assert_eq!(body["metadata"]["session_id"], identity);
    assert_eq!(body["client_metadata"]["thread-id"], identity);
    assert_eq!(body["prompt_cache_key"], identity);
    assert!(!headers.contains_key("x-codex-turn-state"));
    assert!(body["metadata"].get("turn_state").is_none());
    assert!(report.states_removed >= 2);
    assert_eq!(report.identity_hashes.len(), 1);
    assert!(report.identity_values >= 8);
    for field in [
        "input",
        "tools",
        "previous_response_id",
        "conversation",
        "model",
        "stream",
    ] {
        assert_eq!(body[field], original[field], "{field}");
    }
}

#[test]
fn custom_cache_key_is_retained() {
    let store = Mutex::new(Store::default());
    let (mut headers, mut body) = request("thread-a", "turn-a");
    body["prompt_cache_key"] = json!("user-defined-cache-key");
    let lease = acquire_from(&store, "provider-a", &headers, &body).unwrap();
    lease.apply(&lease.rotate(&lease.snapshot()), &mut headers, &mut body);
    assert_eq!(body["prompt_cache_key"], "user-defined-cache-key");
}

#[test]
fn nested_metadata_cache_alias_rotates_without_touching_tool_arguments() {
    let store = Mutex::new(Store::default());
    let (mut headers, mut body) = request("thread-a", "turn-a");
    body["metadata"]["client_metadata"] = json!({"session_id":"nested-original-session"});
    body["prompt_cache_key"] = json!("nested-original-session");
    let input = body["input"].clone();
    let lease = acquire_from(&store, "provider-a", &headers, &body).unwrap();
    let snapshot = lease.rotate(&lease.snapshot());
    lease.apply(&snapshot, &mut headers, &mut body);
    assert_eq!(
        body["prompt_cache_key"],
        snapshot.identity.as_deref().unwrap()
    );
    assert_eq!(
        body["metadata"]["client_metadata"]["session_id"],
        body["prompt_cache_key"]
    );
    assert_eq!(body["input"], input);
}

#[test]
fn provider_thread_isolation_and_subsequent_reuse() {
    let store = Mutex::new(Store::default());
    let (a_headers, a_body) = request("thread-a", "turn-a");
    let (b_headers, b_body) = request("thread-b", "turn-a");
    let a = acquire_from(&store, "provider-a", &a_headers, &a_body).unwrap();
    let first = a.rotate(&a.snapshot());
    let next = acquire_from(&store, "provider-a", &a_headers, &a_body).unwrap();
    assert_eq!(next.snapshot().identity, first.identity);
    assert_eq!(
        acquire_from(&store, "provider-b", &a_headers, &a_body)
            .unwrap()
            .snapshot()
            .generation(),
        0
    );
    assert_eq!(
        acquire_from(&store, "provider-a", &b_headers, &b_body)
            .unwrap()
            .snapshot()
            .generation(),
        0
    );
    let second = next.rotate(&first);
    assert_eq!(second.generation(), 2);
    assert_ne!(second.identity, first.identity);
}

#[test]
fn simultaneous_failures_share_one_generation() {
    let store = Mutex::new(Store::default());
    let (headers, body) = request("thread-a", "turn-a");
    let lease = acquire_from(&store, "provider-a", &headers, &body).unwrap();
    let original = lease.snapshot();
    let barrier = Arc::new(std::sync::Barrier::new(16));
    let workers: Vec<_> = (0..16)
        .map(|_| {
            let lease = lease.clone();
            let original = original.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                lease.rotate(&original)
            })
        })
        .collect();
    let snapshots: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert!(snapshots
        .iter()
        .all(|s| s.generation() == 1 && s.identity == snapshots[0].identity));
}

#[test]
fn route_state_is_scoped_to_generation_and_explicit_turn() {
    let store = Mutex::new(Store::default());
    let (headers, body) = request("thread-a", "turn-a");
    let lease = acquire_from(&store, "provider-a", &headers, &body).unwrap();
    let first = lease.rotate(&lease.snapshot());
    let mut response = HeaderMap::new();
    response.insert(
        "x-codex-turn-state",
        HeaderValue::from_static("new-state-a"),
    );
    lease.observe(&first, &response);
    let (mut same_headers, mut same_body) = request("thread-a", "turn-a");
    let same = acquire_from(&store, "provider-a", &same_headers, &same_body).unwrap();
    same.apply(&same.snapshot(), &mut same_headers, &mut same_body);
    assert_eq!(same_headers["x-codex-turn-state"], "new-state-a");
    let (mut other_headers, mut other_body) = request("thread-a", "turn-b");
    let other = acquire_from(&store, "provider-a", &other_headers, &other_body).unwrap();
    other.apply(&other.snapshot(), &mut other_headers, &mut other_body);
    assert!(!other_headers.contains_key("x-codex-turn-state"));
    let second = lease.rotate(&first);
    lease.observe(&first, &response); // 迟到的旧代响应不能污染新代。
    let mut retry_headers = headers.clone();
    let mut retry_body = body.clone();
    lease.apply(&second, &mut retry_headers, &mut retry_body);
    assert!(!retry_headers.contains_key("x-codex-turn-state"));
    lease.observe(&second, &response);
    let mut unknown = body.clone();
    unknown["metadata"]
        .as_object_mut()
        .unwrap()
        .remove("turn_id");
    let mut unknown_headers = headers.clone();
    unknown_headers.remove("x-codex-turn-metadata");
    let unknown_lease = acquire_from(&store, "provider-a", &unknown_headers, &unknown).unwrap();
    unknown_lease.observe(&second, &response);
    assert!(unknown_lease.turn.is_none());
    unknown_lease.apply(&second, &mut unknown_headers, &mut unknown);
    assert!(!unknown_headers.contains_key("x-codex-turn-state"));
    let peer = acquire_from(&store, "provider-b", &headers, &body).unwrap();
    let peer_snapshot = peer.rotate(&peer.snapshot());
    let mut peer_headers = headers.clone();
    let mut peer_body = body.clone();
    peer.apply(&peer_snapshot, &mut peer_headers, &mut peer_body);
    assert!(!peer_headers.contains_key("x-codex-turn-state"));
}

#[test]
fn final_outbound_turn_overrides_do_not_reuse_original_turn_state() {
    let store = Mutex::new(Store::default());
    let (headers, body) = request("thread-a", "turn-a");
    let lease = acquire_from(&store, "provider-a", &headers, &body).unwrap();
    let snapshot = lease.rotate(&lease.snapshot());
    let mut response = HeaderMap::new();
    response.insert(
        "x-codex-turn-state",
        HeaderValue::from_static("state-for-turn-a"),
    );
    lease.observe(&snapshot, &response);

    // 模拟中间件统一修改最终回合：只能缓存/复用 B，不能沿用 acquire 时的 A。
    let mut outbound_headers = headers.clone();
    outbound_headers.insert(
        "x-codex-turn-metadata",
        HeaderValue::from_static("{\"turn_id\":\"turn-b\"}"),
    );
    let mut outbound_body = body.clone();
    outbound_body["metadata"]["turn_id"] = json!("turn-b");
    let outbound = lease.for_outbound(&outbound_headers, &outbound_body);
    assert_ne!(outbound.turn, lease.turn);
    outbound.apply(&snapshot, &mut outbound_headers, &mut outbound_body);
    assert!(!outbound_headers.contains_key("x-codex-turn-state"));
    response.insert(
        "x-codex-turn-state",
        HeaderValue::from_static("state-for-turn-b"),
    );
    outbound.observe(&snapshot, &response);
    let mut next_headers = outbound_headers.clone();
    let mut next_body = outbound_body.clone();
    outbound.apply(&snapshot, &mut next_headers, &mut next_body);
    assert_eq!(next_headers["x-codex-turn-state"], "state-for-turn-b");
    let mut original_headers = headers.clone();
    let mut original_body = body.clone();
    lease.apply(&snapshot, &mut original_headers, &mut original_body);
    assert_eq!(original_headers["x-codex-turn-state"], "state-for-turn-a");
}

#[test]
fn conflicting_or_unknown_final_turn_sources_disable_state_reuse() {
    let store = Mutex::new(Store::default());
    let (headers, body) = request("thread-a", "turn-a");
    let lease = acquire_from(&store, "provider-a", &headers, &body).unwrap();
    let snapshot = lease.rotate(&lease.snapshot());
    let mut response = HeaderMap::new();
    response.insert(
        "x-codex-turn-state",
        HeaderValue::from_static("state-for-turn-a"),
    );
    lease.observe(&snapshot, &response);

    let mut conflicting = headers.clone();
    conflicting.insert("turn-id", HeaderValue::from_static("turn-b"));
    let mut final_body = body.clone();
    let outbound = lease.for_outbound(&conflicting, &final_body);
    assert!(outbound.turn.is_none());
    outbound.apply(&snapshot, &mut conflicting, &mut final_body);
    assert!(!conflicting.contains_key("x-codex-turn-state"));
    outbound.observe(&snapshot, &response);
    assert!(
        acquire_from(&store, "provider-a", &conflicting, &final_body)
            .unwrap()
            .turn
            .is_none()
    );

    let mut duplicates = HeaderMap::new();
    duplicates.append("turn-id", HeaderValue::from_static("turn-a"));
    duplicates.append("turn-id", HeaderValue::from_static("turn-b"));
    assert!(lease.for_outbound(&duplicates, &json!({})).turn.is_none());
    let mut unknown = headers.clone();
    unknown.insert(
        "x-codex-turn-metadata",
        HeaderValue::from_static("opaque-metadata"),
    );
    assert!(lease.for_outbound(&unknown, &body).turn.is_none());
    let mut unknown_container = body.clone();
    unknown_container["client_metadata"] = json!("unknown-metadata-format");
    assert!(lease
        .for_outbound(&headers, &unknown_container)
        .turn
        .is_none());
    // 将恢复的原元数据 A 与最终正文 B 冲突，同样不能复用状态。
    let mut stripped = headers.clone();
    stripped.remove("x-codex-turn-metadata");
    let mut changed = body.clone();
    changed["metadata"]["turn_id"] = json!("turn-b");
    assert!(lease.for_outbound(&stripped, &changed).turn.is_none());
    // 历史/工具参数里不同的 turn_id 不是当前回合来源。
    assert!(lease.for_outbound(&headers, &body).turn.is_some());
}

#[test]
fn missing_identity_and_opaque_turn_metadata_are_skipped() {
    let store = Mutex::new(Store::default());
    assert!(acquire_from(
        &store,
        "provider-a",
        &HeaderMap::new(),
        &json!({"model":"x"})
    )
    .is_none());
    let (mut headers, body) = request("thread-a", "turn-a");
    headers.insert(
        "x-codex-turn-metadata",
        HeaderValue::from_static("unrecognized-format"),
    );
    assert!(acquire_from(&store, "provider-a", &headers, &body).is_none());
}

#[test]
fn active_entries_are_pinned_and_capacity_fails_open() {
    let store = Mutex::new(Store::default());
    let leases: Vec<_> = (0..CAPACITY)
        .map(|i| {
            let (headers, body) = request(&format!("thread-{i}"), "turn-a");
            acquire_from(&store, "provider-a", &headers, &body).unwrap()
        })
        .collect();
    let first = leases[0].rotate(&leases[0].snapshot());
    let (headers, body) = request("overflow-thread", "turn-a");
    assert!(acquire_from(&store, "provider-a", &headers, &body).is_none());
    assert_eq!(leases[0].snapshot().identity, first.identity);
    drop(leases);
    assert!(acquire_from(&store, "provider-a", &headers, &body).is_some());
    assert_eq!(store.lock().unwrap().entries.len(), CAPACITY);
}

#[test]
fn idle_expiry_never_evicts_an_active_lease() {
    let store = Mutex::new(Store::default());
    let (headers, body) = request("thread-a", "turn-a");
    let baseline = Instant::now();
    let expired_now = baseline + IDLE_TTL + Duration::from_secs(1);
    let lease = acquire_from_at(&store, "provider-a", &headers, &body, baseline).unwrap();
    let rotated = lease.rotate(&lease.snapshot());
    lease.entry.state.lock().unwrap().last_used = baseline;
    let same = acquire_from_at(&store, "provider-a", &headers, &body, expired_now).unwrap();
    assert_eq!(same.snapshot().identity, rotated.identity);
    same.entry.state.lock().unwrap().last_used = baseline;
    drop(same);
    drop(lease);
    let fresh = acquire_from_at(&store, "provider-a", &headers, &body, expired_now).unwrap();
    assert_eq!(fresh.snapshot().generation(), 0);
}

#[test]
fn only_exact_known_limit_errors_trigger() {
    let coded =
        json!({"error":{"code":"session_turn_limit_exceeded","message":"limit"}}).to_string();
    assert!(is_turn_limit(400, &coded));
    for status in [200, 401, 403, 409, 429, 500] {
        assert!(!is_turn_limit(status, &coded));
    }
    for count in [100, 105] {
        let message = format!("当前会话模型已回复 {count} 轮，达到上限。Codex 请输入 /compact 压缩上下文后继续，或输入 /new 新开会话。");
        assert!(is_turn_limit(
            400,
            &json!({"error":{"message":message}}).to_string()
        ));
        assert!(!is_turn_limit(
            400,
            &json!({"error":{"code":"context_length_exceeded","message":message}}).to_string()
        ));
    }
    for message in [
        "Bad Request",
        "context length exceeded",
        "insufficient quota",
        "insufficient balance",
        "rate limit exceeded",
        "105 model turns",
        "session_turn_limit_exceeded",
    ] {
        assert!(!is_turn_limit(
            400,
            &json!({"error":{"message":message}}).to_string()
        ));
    }
    let message = "当前会话模型已回复 105 轮，达到上限。Codex 请输入 /compact 压缩上下文后继续，或输入 /new 新开会话。";
    assert!(!is_turn_limit(
        400,
        &json!({"error":{"code":400,"message":message}}).to_string()
    ));
}
