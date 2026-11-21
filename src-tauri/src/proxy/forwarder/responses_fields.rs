//! Final per-provider edits and opt-in, in-memory field summaries for Codex Responses.

use super::{
    headers::is_protected_local_proxy_override_header, request_body::prepare_upstream_request_body,
};
use crate::provider::LocalProxyResponsesRules;
use http::{HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::VecDeque,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn credential_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["api-key", "api_key", "api.key", "apikey"]
        .iter()
        .any(|part| name.contains(*part))
        || name.split(['-', '_', '.']).any(|segment| {
            [
                "auth",
                "authorization",
                "authentication",
                "token",
                "secret",
                "password",
                "cookie",
                "credential",
            ]
            .iter()
            .any(|word| segment.eq_ignore_ascii_case(word))
        })
}

fn can_edit_header(name: &HeaderName) -> bool {
    !is_protected_local_proxy_override_header(name)
        && !matches!(name.as_str(), "accept" | "content-encoding")
        && !credential_name(name.as_str())
}

fn can_edit_body_field(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('_')
        && !name.chars().any(char::is_control)
        && !matches!(name, "model" | "input" | "stream" | "session_id")
        && !credential_name(name)
}

/// Only change fields actually present on this request. In particular, a rule
/// chosen from a normal turn must not add unsupported parameters to /compact.
/// An explicit removal wins over a replacement in externally imported rules.
pub(super) fn apply_rules(
    rules: &LocalProxyResponsesRules,
    headers: &mut HeaderMap,
    body: &mut Value,
    redact: impl FnOnce(&mut Value),
) {
    for (name, value) in &rules.set_headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.trim().as_bytes()),
            HeaderValue::from_str(value),
        ) {
            if can_edit_header(&name) && headers.contains_key(&name) {
                headers.insert(name, value);
            }
        }
    }
    for name in &rules.remove_headers {
        if let Ok(name) = HeaderName::from_bytes(name.trim().as_bytes()) {
            if can_edit_header(&name) {
                headers.remove(name);
            }
        }
    }
    let Some(body) = body.as_object_mut() else {
        return;
    };
    if !rules.set_body_fields.is_empty() {
        let mut replacements = prepare_upstream_request_body(Value::Object(
            rules
                .set_body_fields
                .iter()
                .map(|(name, value)| (name.trim(), value))
                .filter(|(name, _)| can_edit_body_field(name) && body.contains_key(*name))
                .map(|(name, value)| (name.to_owned(), value.clone()))
                .collect(),
        ));
        redact(&mut replacements);
        if let Value::Object(replacements) = replacements {
            body.extend(replacements);
        }
    }
    for name in &rules.remove_body_fields {
        let name = name.trim();
        if can_edit_body_field(name) {
            body.remove(name);
        }
    }
}

const LEASE: Duration = Duration::from_secs(15);
const MAX_WATCHES: usize = 16;
const MAX_FIELDS: usize = 96;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FieldSummary {
    name: String,
    kind: &'static str,
    editable: bool,
    /// Closed allowlist of previews; no free-form strings, headers or JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    preview: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResponsesFieldsSnapshot {
    observed_at_ms: u64,
    headers: Vec<FieldSummary>,
    body_fields: Vec<FieldSummary>,
    truncated: bool,
}

impl ResponsesFieldsSnapshot {
    fn new(headers: &HeaderMap, body: &Value) -> Self {
        let body = body.as_object();
        Self {
            observed_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            truncated: headers.keys().count() > MAX_FIELDS
                || body.is_some_and(|body| body.len() > MAX_FIELDS),
            headers: headers
                .keys()
                .take(MAX_FIELDS)
                .filter(|name| name.as_str().len() <= 128)
                .map(|name| FieldSummary {
                    name: name.to_string(),
                    kind: "header",
                    editable: can_edit_header(name),
                    preview: safe_header_preview(name, headers),
                })
                .collect(),
            body_fields: body
                .into_iter()
                .flatten()
                .take(MAX_FIELDS)
                .filter(|(name, _)| {
                    !name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control)
                })
                .map(|(name, value)| FieldSummary {
                    name: name.clone(),
                    kind: match value {
                        Value::Null => "null",
                        Value::Bool(_) => "boolean",
                        Value::Number(_) => "number",
                        Value::String(_) => "string",
                        Value::Array(_) => "array",
                        Value::Object(_) => "object",
                    },
                    editable: can_edit_body_field(name),
                    preview: safe_body_preview(name, value),
                })
                .collect(),
        }
    }
}

fn safe_header_preview(name: &HeaderName, headers: &HeaderMap) -> Option<Value> {
    let safe_values: &[&str] = match name.as_str() {
        "content-type" => &["application/json", "application/json; charset=utf-8"],
        "accept" => &["application/json", "text/event-stream", "*/*"],
        "accept-encoding" | "content-encoding" => &["identity", "gzip", "br", "zstd", "deflate"],
        _ => return None,
    };
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    (values.next().is_none() && safe_values.contains(&value)).then(|| Value::from(value))
}

fn safe_body_preview(name: &str, value: &Value) -> Option<Value> {
    let safe = match name {
        "stream" | "store" | "parallel_tool_calls" => value.is_boolean(),
        "temperature" => value.as_f64().is_some_and(|n| (0.0..=2.0).contains(&n)),
        "top_p" => value.as_f64().is_some_and(|n| (0.0..=1.0).contains(&n)),
        "max_output_tokens" => value.as_u64().is_some_and(|n| n <= 10_000_000),
        _ => false,
    };
    safe.then(|| value.clone())
}

struct Watch {
    provider_id: String,
    deadline: Instant,
    sample: Option<ResponsesFieldsSnapshot>,
}

#[derive(Default)]
struct Watches(VecDeque<Watch>);

impl Watches {
    fn poll(&mut self, provider_id: &str, now: Instant) -> Option<ResponsesFieldsSnapshot> {
        self.0.retain(|watch| watch.deadline > now);
        let previous = self
            .0
            .iter()
            .position(|watch| watch.provider_id == provider_id)
            .and_then(|index| self.0.remove(index));
        let sample = previous.and_then(|watch| watch.sample);
        if self.0.len() >= MAX_WATCHES {
            self.0.pop_front();
        }
        self.0.push_back(Watch {
            provider_id: provider_id.to_owned(),
            deadline: now + LEASE,
            sample: sample.clone(),
        });
        sample
    }

    fn record(&mut self, provider_id: &str, headers: &HeaderMap, body: &Value, now: Instant) {
        self.0.retain(|watch| watch.deadline > now);
        if let Some(watch) = self
            .0
            .iter_mut()
            .find(|watch| watch.provider_id == provider_id)
        {
            watch.sample = Some(ResponsesFieldsSnapshot::new(headers, body));
        }
    }
}

static WATCHES: LazyLock<Mutex<Watches>> = LazyLock::new(Mutex::default);

/// A short observation lease, renewed by the open editor. A stopped viewer does
/// not keep collecting. Neither the sample nor the lease is persisted.
pub(crate) fn observe(provider_id: &str) -> Option<ResponsesFieldsSnapshot> {
    if provider_id.is_empty() || provider_id.len() > 1024 {
        return None;
    }
    WATCHES.lock().ok()?.poll(provider_id, Instant::now())
}

pub(super) fn record(provider_id: &str, headers: &HeaderMap, body: &Value) {
    if let Ok(mut watches) = WATCHES.lock() {
        watches.record(provider_id, headers, body, Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn observation_is_opt_in_expiring_and_provider_scoped_without_raw_values() {
        let now = Instant::now();
        let mut watches = Watches::default();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("private-auth"));
        headers.insert("x-session-id", HeaderValue::from_static("private-session"));
        headers.insert("accept", HeaderValue::from_static("text/event-stream"));
        let body = json!({
            "input": ["private-prompt"],
            "prompt_cache_key": "private-cache-key",
            "temperature": 0.2,
            "stream": true
        });
        watches.record("a", &headers, &body, now);
        assert!(watches.poll("a", now).is_none());
        watches.record("b", &headers, &body, now);
        assert!(watches.poll("a", now).is_none());
        watches.record("a", &headers, &body, now);
        let sample = watches.poll("a", now).unwrap();
        let serialized = serde_json::to_string(&sample).unwrap();
        assert!(!serialized.contains("private-"));
        assert!(sample.headers.iter().any(|field| {
            field.name == "accept"
                && !field.editable
                && field.preview == Some(json!("text/event-stream"))
        }));
        assert!(sample
            .headers
            .iter()
            .any(|field| field.name == "authorization" && !field.editable));
        assert!(sample
            .headers
            .iter()
            .any(|field| field.name == "x-session-id"
                && field.editable
                && field.preview.is_none()));
        assert!(sample
            .body_fields
            .iter()
            .any(|field| field.name == "temperature" && field.preview == Some(json!(0.2))));
        assert!(watches.poll("b", now).is_none());
        assert!(watches.poll("a", now + LEASE).is_none());
    }

    #[test]
    fn final_rules_replace_exactly_and_cannot_change_transport_fields() {
        let rules: LocalProxyResponsesRules = serde_json::from_value(json!({
            "setHeaders": {
                "X-Example": "changed",
                "X-Not-Present": "not-added",
                "X-Session-ID": "intermediate",
                "Authorization": "blocked",
                "x-custom-api-key": "blocked",
                "Accept": "blocked"
            },
            "setBodyFields": {
                "instructions": "REDACT",
                "metadata": {"replacement": true},
                "max_output_tokens": null,
                "top_p": 0.2,
                "stream": false,
                "_private": "blocked"
            },
            "removeHeaders": ["x-session-id", "Session_Id"],
            "removeBodyFields": ["prompt_cache_key", "input", "session_id"]
        }))
        .unwrap();
        let mut headers = HeaderMap::new();
        for name in [
            "x-example",
            "x-session-id",
            "x-session-id",
            "session_id",
            "authorization",
            "accept",
        ] {
            headers.append(name, HeaderValue::from_static("original"));
        }
        let mut body = json!({
            "instructions": "keep",
            "metadata": {"original": true},
            "prompt_cache_key": "old",
            "max_output_tokens": 1024,
            "input": [],
            "stream": true,
            "session_id": "keep"
        });
        apply_rules(&rules, &mut headers, &mut body, |patch| {
            assert_eq!(patch["instructions"], "REDACT");
            patch["instructions"] = json!("redacted");
        });
        assert!(!headers.contains_key("x-session-id"));
        assert!(!headers.contains_key("x-custom-api-key"));
        assert!(!headers.contains_key("x-not-present"));
        assert_eq!(headers["x-example"], "changed");
        for name in ["session_id", "authorization", "accept"] {
            assert_eq!(headers[name], "original");
        }
        assert_eq!(
            body,
            json!({
                "instructions": "redacted",
                "metadata": {"replacement": true},
                "max_output_tokens": null,
                "input": [],
                "stream": true,
                "session_id": "keep"
            })
        );
    }
}
