//! 身份重试完成后追加一条可见说明；不模拟客户端工具执行或聊天来源标签。

use super::*;
use crate::proxy::sse::{bounded_sse_chunks, strip_sse_field, SseBuffer};
use serde_json::json;

fn notice_item(text: &str) -> Value {
    json!({
        "type":"message", "role":"assistant", "phase":"commentary", "status":"completed",
        "id":format!("msg_cc_switch_notice_{}", uuid::Uuid::new_v4().simple()),
        "content":[{"type":"output_text","text":text,"annotations":[]}]
    })
}

/// 只接受明确完成的正常响应，失败、未完成、断流均不得宣称续接成功。
fn append_notice(response: &mut Value, text: &str) -> Option<(usize, Value)> {
    if response.get("status").and_then(Value::as_str) != Some("completed")
        || response.get("error").is_some_and(|value| !value.is_null())
    {
        return None;
    }
    let output = response.get_mut("output")?.as_array_mut()?;
    let index = output.len();
    let item = notice_item(text);
    output.push(item.clone());
    Some((index, item))
}

fn sse_bytes(value: &Value) -> Bytes {
    Bytes::from(format!(
        "event: {}\ndata: {}\n\n",
        value["type"].as_str().unwrap_or("message"),
        value
    ))
}

/// 只替换 JSON data，保留原事件的 id、retry、注释及未知字段。
fn replace_sse_data(block: &str, value: &Value) -> Bytes {
    let mut lines = Vec::new();
    let mut replaced = false;
    for line in block.lines() {
        if strip_sse_field(line, "data").is_some() {
            if !replaced {
                lines.push(format!("data: {value}"));
                replaced = true;
            }
        } else {
            lines.push(line.to_string());
        }
    }
    Bytes::from(format!("{}\n\n", lines.join("\n")))
}

#[derive(Default)]
struct NoticeStream {
    completed: bool,
    max_sequence: Option<u64>,
    sequence_offset: u64,
}

impl NoticeStream {
    fn block(&mut self, block: &str, text: &str) -> Vec<Bytes> {
        let raw = || vec![Bytes::from(format!("{block}\n\n"))];
        let data = block
            .lines()
            .filter_map(|line| strip_sse_field(line, "data"))
            .collect::<Vec<_>>()
            .join("\n");
        let Ok(mut event) = serde_json::from_str::<Value>(&data) else {
            return raw();
        };
        let sequence = event.get("sequence_number").and_then(Value::as_u64);
        if let Some(value) = sequence {
            self.max_sequence = Some(self.max_sequence.map_or(value, |old| old.max(value)));
        }
        if self.completed || event.get("type").and_then(Value::as_str) != Some("response.completed")
        {
            if self.sequence_offset > 0 {
                if let Some(value) = sequence {
                    event["sequence_number"] = json!(value.saturating_add(self.sequence_offset));
                    return vec![replace_sse_data(block, &event)];
                }
            }
            return raw();
        }
        self.completed = true;
        let Some((index, item)) = event
            .get_mut("response")
            .and_then(|response| append_notice(response, text))
        else {
            return raw();
        };
        let start_sequence = sequence
            .or_else(|| self.max_sequence.map(|value| value.saturating_add(1)))
            .unwrap_or(0);
        let id = item["id"].clone();
        let part = item["content"][0].clone();
        let mut started = item.clone();
        started["status"] = json!("in_progress");
        started["content"] = json!([]);
        let mut events = vec![
            json!({"type":"response.output_item.added","output_index":index,"item":started}),
            json!({"type":"response.content_part.added","item_id":id,"output_index":index,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
            json!({"type":"response.output_text.delta","item_id":id,"output_index":index,"content_index":0,"delta":text}),
            json!({"type":"response.output_text.done","item_id":id,"output_index":index,"content_index":0,"text":text}),
            json!({"type":"response.content_part.done","item_id":id,"output_index":index,"content_index":0,"part":part}),
            json!({"type":"response.output_item.done","output_index":index,"item":item}),
        ];
        self.sequence_offset = events.len() as u64;
        for (offset, value) in events.iter_mut().enumerate() {
            value["sequence_number"] = json!(start_sequence.saturating_add(offset as u64));
        }
        event["sequence_number"] = json!(start_sequence.saturating_add(self.sequence_offset));
        let mut bytes: Vec<_> = events.iter().map(sse_bytes).collect();
        bytes.push(replace_sse_data(block, &event));
        bytes
    }
}

pub(super) async fn annotate(
    response: ProxyResponse,
    streaming: bool,
    text: String,
) -> Result<ProxyResponse, ProxyError> {
    if !response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let mut headers = response.headers().clone();
    if !streaming || response.is_json() {
        let encoding = get_content_encoding(&headers);
        let bytes = response.bytes_with_limit(MAX_RESPONSE_BODY_BYTES).await?;
        let decoded = match encoding {
            Some(encoding) => {
                decompress_body_with_limit(&encoding, &bytes, MAX_RESPONSE_BODY_BYTES)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| bytes.to_vec())
            }
            None => bytes.to_vec(),
        };
        let Ok(mut value) = serde_json::from_slice::<Value>(&decoded) else {
            return Ok(ProxyResponse::buffered(status, headers, bytes));
        };
        if append_notice(&mut value, &text).is_none() {
            return Ok(ProxyResponse::buffered(status, headers, bytes));
        }
        let output = serde_json::to_vec(&value)
            .map_err(|error| ProxyError::Internal(format!("无法编码续聊提示：{error}")))?;
        headers.remove(http::header::CONTENT_LENGTH);
        headers.remove(http::header::CONTENT_ENCODING);
        return Ok(ProxyResponse::buffered(
            status,
            headers,
            Bytes::from(output),
        ));
    }
    // 不把压缩字节当 SSE 解析；上游通常遵守已有 identity 编码请求。
    if get_content_encoding(&headers).is_some() {
        return Ok(response);
    }
    headers.remove(http::header::CONTENT_LENGTH);
    let stream = async_stream::try_stream! {
        let chunks = bounded_sse_chunks(response.bytes_stream());
        futures::pin_mut!(chunks);
        let mut buffer = SseBuffer::new();
        let mut state = NoticeStream::default();
        while let Some(chunk) = chunks.next().await {
            buffer.push_bytes(&chunk?)?;
            while let Some(block) = buffer.next_block() {
                for bytes in state.block(block, &text) { yield bytes; }
            }
        }
        if !buffer.pending().is_empty() {
            for bytes in state.block(buffer.pending(), &text) { yield bytes; }
        }
    };
    Ok(ProxyResponse::streamed(status, headers, stream))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[Bytes]) -> Vec<Value> {
        bytes
            .iter()
            .filter_map(|bytes| {
                let block = std::str::from_utf8(bytes).unwrap();
                let data = block
                    .lines()
                    .filter_map(|line| strip_sse_field(line, "data"))
                    .collect::<Vec<_>>()
                    .join("\n");
                serde_json::from_str(&data).ok()
            })
            .collect()
    }

    #[test]
    fn session_identity_notice_preserves_tool_calls_and_adds_one_consistent_message() {
        let tool = json!({"type":"custom_tool_call","call_id":"original-call","name":"exec","input":"text(323);"});
        let event = json!({"type":"response.completed","sequence_number":12,"response":{"id":"original-response","status":"completed","usage":{"output_tokens":5},"output":[tool.clone()]}});
        let mut state = NoticeStream::default();
        let input = format!("event: response.completed\ndata: {event}");
        let events = parse(&state.block(&input, "身份已切换（第100轮回复），聊天记录已保留。"));
        assert_eq!(events.len(), 7);
        for (index, value) in events.iter().enumerate() {
            assert_eq!(value["sequence_number"], 12 + index);
        }
        assert_eq!(events[0]["output_index"], 1);
        assert_eq!(events[0]["item"]["phase"], "commentary");
        assert_eq!(events[6]["response"]["id"], "original-response");
        assert_eq!(events[6]["response"]["output"][0], tool);
        assert_eq!(events[6]["response"]["usage"], json!({"output_tokens":5}));
        assert_eq!(events[5]["item"], events[6]["response"]["output"][1]);
        assert_eq!(state.block(&input, "不得重复").len(), 1);
    }

    #[tokio::test]
    async fn session_identity_notice_handles_fragmented_utf8_and_does_not_wait_for_all_output() {
        let created = Bytes::from_static(b"event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0}\n\n");
        let (tx, rx) = tokio::sync::oneshot::channel();
        let completed = "event: response.completed\ndata: {\"type\":\"response.completed\",\"sequence_number\":1,\"response\":{\"id\":\"原响应\",\"status\":\"completed\",\"output\":[]}}";
        let response = ProxyResponse::streamed(
            http::StatusCode::OK,
            http::HeaderMap::new(),
            async_stream::try_stream! {
                yield created;
                rx.await.map_err(std::io::Error::other)?;
                for byte in completed.as_bytes() { yield Bytes::copy_from_slice(&[*byte]); }
            },
        );
        let response = annotate(response, true, "身份已切换，聊天记录已保留。".to_string())
            .await
            .unwrap();
        let mut stream = Box::pin(response.bytes_stream());
        let first = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(std::str::from_utf8(&first)
            .unwrap()
            .contains("response.created"));
        tx.send(()).unwrap();
        let mut remaining = Vec::new();
        while let Some(bytes) = stream.next().await {
            remaining.push(bytes.unwrap());
        }
        let events = parse(&remaining);
        assert_eq!(events.len(), 7);
        assert_eq!(events[2]["delta"], "身份已切换，聊天记录已保留。");
    }

    #[test]
    fn session_identity_notice_never_claims_success_for_failure_or_incomplete() {
        for status in ["failed", "incomplete", "in_progress"] {
            let mut value = json!({"status":status,"output":[]});
            assert!(append_notice(&mut value, "不得显示").is_none());
            assert_eq!(value["output"], json!([]));
        }
        let mut state = NoticeStream::default();
        for kind in ["response.failed", "error", "response.output_text.delta"] {
            let block = format!("data: {}", json!({"type":kind,"delta":"原文"}));
            assert_eq!(
                state.block(&block, "不得显示"),
                vec![Bytes::from(format!("{block}\n\n"))]
            );
        }
    }

    #[tokio::test]
    async fn session_identity_notice_json_success_keeps_original_output_and_removes_stale_length() {
        let original = json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"原回复"}]}],"usage":{"total_tokens":7}});
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            http::header::CONTENT_LENGTH,
            http::HeaderValue::from_static("1"),
        );
        let response = ProxyResponse::buffered(
            http::StatusCode::OK,
            headers,
            Bytes::from(original.to_string()),
        );
        let response = annotate(response, false, "身份已切换，聊天记录已保留。".to_string())
            .await
            .unwrap();
        assert!(!response
            .headers()
            .contains_key(http::header::CONTENT_LENGTH));
        let value: Value = serde_json::from_slice(
            &response
                .bytes_with_limit(MAX_RESPONSE_BODY_BYTES)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(value["output"][0], original["output"][0]);
        assert_eq!(value["output"].as_array().unwrap().len(), 2);
        assert_eq!(value["usage"], original["usage"]);
    }

    #[test]
    fn session_identity_notice_rewritten_events_keep_sse_metadata() {
        let mut state = NoticeStream::default();
        let completed = "id: original-id\n: upstream-comment\nretry: 1500\nevent: response.completed\nx-extra: preserved\ndata: {\"type\":\"response.completed\",\"sequence_number\":5,\"response\":{\"status\":\"completed\",\"output\":[]}}";
        let bytes = state.block(completed, "身份已切换，聊天记录已保留。");
        let last = std::str::from_utf8(bytes.last().unwrap()).unwrap();
        for line in [
            "id: original-id",
            ": upstream-comment",
            "retry: 1500",
            "x-extra: preserved",
        ] {
            assert!(last.contains(line));
        }
        let trailing = "id: metadata-id\nretry: 2000\n: trailing-comment\nevent: response.metadata\ndata: {\"type\":\"response.metadata\",\"sequence_number\":6,\"metadata\":{\"original\":true}}";
        let bytes = state.block(trailing, "不能重复");
        let text = std::str::from_utf8(&bytes[0]).unwrap();
        for line in ["id: metadata-id", "retry: 2000", ": trailing-comment"] {
            assert!(text.contains(line));
        }
        let parsed = parse(&bytes);
        assert_eq!(parsed[0]["sequence_number"], 12);
        assert_eq!(parsed[0]["metadata"]["original"], true);
    }

    #[tokio::test]
    async fn session_identity_notice_decodes_gzip_json_without_changing_tools() {
        use std::io::Write;
        let original = json!({"status":"completed","output":[{"type":"function_call","call_id":"kept-call","arguments":"{\"保留\":true}"}]});
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(original.to_string().as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            http::header::CONTENT_ENCODING,
            http::HeaderValue::from_static("gzip"),
        );
        let response =
            ProxyResponse::buffered(http::StatusCode::OK, headers, Bytes::from(compressed));
        let response = annotate(response, false, "身份已切换，聊天记录已保留。".to_string())
            .await
            .unwrap();
        assert!(!response
            .headers()
            .contains_key(http::header::CONTENT_ENCODING));
        let parsed: Value = serde_json::from_slice(
            &response
                .bytes_with_limit(MAX_RESPONSE_BODY_BYTES)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(parsed["output"][0], original["output"][0]);
        assert_eq!(parsed["output"].as_array().unwrap().len(), 2);
    }
}
