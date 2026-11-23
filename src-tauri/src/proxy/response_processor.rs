//! 响应处理器模块
//!
//! 统一处理流式和非流式 API 响应

use super::{
    content_encoding::{decompress_body_with_limit, get_content_encoding, DecompressError},
    forwarder::ActiveConnectionGuard,
    handler_config::{StreamUsageEventFilter, UsageParserConfig},
    handler_context::{RequestContext, StreamingTimeoutConfig},
    hyper_client::{ProxyResponse, MAX_RESPONSE_BODY_BYTES},
    server::ProxyState,
    sse::{bounded_sse_chunks, strip_sse_field, SseBuffer},
    usage::parser::TokenUsage,
    ProxyError,
};
use crate::database::PRICING_SOURCE_REQUEST;
use axum::http::{header::HeaderMap, HeaderName};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use serde_json::Value;
use std::{
    borrow::Cow,
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
    time::Duration,
};
use tokio::sync::Mutex;

// ============================================================================
// 响应头处理
// ============================================================================

/// RFC 2616 / RFC 7230 中定义的不应被代理继续转发的响应头。
const HOP_BY_HOP_RESPONSE_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// 移除响应侧 hop-by-hop 头，以及 `Connection` 中点名的扩展头。
pub(crate) fn strip_hop_by_hop_response_headers(headers: &mut HeaderMap) {
    let connection_listed_headers: Vec<HeaderName> = headers
        .get_all(axum::http::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .filter_map(|name| HeaderName::from_bytes(name.as_bytes()).ok())
        .collect();

    for name in HOP_BY_HOP_RESPONSE_HEADERS {
        headers.remove(*name);
    }

    for name in connection_listed_headers {
        headers.remove(name);
    }
}

/// 移除在重建响应体后会失真的实体头。
pub(crate) fn strip_entity_headers_for_rebuilt_body(headers: &mut HeaderMap) {
    headers.remove(axum::http::header::CONTENT_ENCODING);
    headers.remove(axum::http::header::CONTENT_LENGTH);
    headers.remove(axum::http::header::TRANSFER_ENCODING);
}

/// 读取响应体并在需要时解压，确保 headers 与返回 body 一致。
///
/// `body_timeout`: 整包超时。当非零时用 `tokio::time::timeout` 包住 `.bytes()` 调用，
/// 防止上游发完响应头后卡住 body 导致请求永远挂住。
/// 传入 `Duration::ZERO` 表示不启用超时（故障转移关闭时）。
pub(crate) async fn read_decoded_body(
    response: ProxyResponse,
    tag: &str,
    body_timeout: Duration,
) -> Result<(HeaderMap, http::StatusCode, Bytes), ProxyError> {
    let mut headers = response.headers().clone();
    let status = response.status();
    let bytes_future = response.bytes_with_limit(MAX_RESPONSE_BODY_BYTES);
    let raw_bytes = if body_timeout.is_zero() {
        bytes_future.await?
    } else {
        tokio::time::timeout(body_timeout, bytes_future)
            .await
            .map_err(|_| {
                ProxyError::Timeout(format!(
                    "响应体读取超时: {}s（上游发完响应头后 body 未到达）",
                    body_timeout.as_secs()
                ))
            })??
    };

    log::debug!(
        "[{tag}] 已接收上游响应体: status={}, bytes={}, headers={}",
        status.as_u16(),
        raw_bytes.len(),
        format_headers(&headers)
    );

    let mut body_bytes = raw_bytes.clone();
    let mut decoded = false;

    if let Some(encoding) = get_content_encoding(&headers) {
        log::debug!("[{tag}] 解压非流式响应: content-encoding={encoding}");
        match decompress_body_with_limit(&encoding, &raw_bytes, MAX_RESPONSE_BODY_BYTES) {
            Ok(Some(decompressed)) => {
                // 解码器在预算耗尽处即截停，此处必然 ≤ MAX_RESPONSE_BODY_BYTES
                body_bytes = Bytes::from(decompressed);
                decoded = true;
            }
            // 不支持的编码：原样透传且保留 content-encoding 头，
            // 让下游诊断/客户端知道这仍是压缩字节
            Ok(None) => {}
            Err(DecompressError::TooLarge { .. }) => {
                return Err(ProxyError::ResponseBodyTooLarge(MAX_RESPONSE_BODY_BYTES));
            }
            Err(DecompressError::Io(e)) => {
                log::warn!("[{tag}] 解压失败 ({encoding}): {e}，使用原始数据");
            }
        }
    }

    if decoded {
        strip_entity_headers_for_rebuilt_body(&mut headers);
    }

    Ok((headers, status, body_bytes))
}

// ============================================================================
// 公共接口
// ============================================================================

/// 检测响应是否为 SSE 流式响应
///
/// 上游标了 `text/event-stream` 就是流；客户端要的是流（`request_is_stream`）、
/// 上游 2xx 却完全不带 Content-Type 时也按流处理。chatgpt.com 的 Codex
/// Responses 回包就不带这个头：当成整包读会把实时输出攒到回合结束才一次性
/// 交给客户端，SSE 文本按 JSON 解析失败，用量记成 0（会话日志导入随之去重
/// 不上，同一回合出现两行）。只认“缺头”，不认任意非 JSON：网关忽略
/// `stream: true`、回一个标成 `text/plain` 的 JSON 时，仍按整包解析用量。
#[inline]
pub fn is_sse_response(response: &ProxyResponse, request_is_stream: bool) -> bool {
    response.is_sse()
        || (request_is_stream
            && response.status().is_success()
            && response.content_type().is_none())
}

fn prepare_streaming_headers(
    headers: &HeaderMap,
    timeout_config: &mut StreamingTimeoutConfig,
) -> HeaderMap {
    let mut headers = headers.clone();
    strip_hop_by_hop_response_headers(&mut headers);
    // A compressed passthrough is an opaque byte stream. Injecting plain SSE comments
    // would corrupt it; preserve its encoding and length until streaming decoding exists.
    if get_content_encoding(&headers).is_some() {
        timeout_config.keepalive_secs = 0;
        timeout_config.loop_guard = false;
    }
    if timeout_config.keepalive_secs > 0 {
        headers.remove(axum::http::header::CONTENT_LENGTH);
    }
    headers
}

/// 处理流式响应
pub async fn handle_streaming(
    response: ProxyResponse,
    ctx: &RequestContext,
    state: &ProxyState,
    parser_config: &UsageParserConfig,
    connection_guard: Option<ActiveConnectionGuard>,
) -> Response {
    let status = response.status();
    log::debug!(
        "[{}] 已接收上游流式响应: status={}, headers={}",
        ctx.tag,
        status.as_u16(),
        format_headers(response.headers())
    );
    // 检查流式响应是否被压缩（SSE 通常不压缩，如果压缩则 SSE 解析会失败）
    if let Some(encoding) = get_content_encoding(response.headers()) {
        log::warn!(
            "[{}] 流式响应含 content-encoding={encoding}，SSE 解析可能失败。\
             上游在 accept-encoding 透传后压缩了 SSE 流。",
            ctx.tag
        );
    }

    let mut timeout_config = ctx.streaming_timeout_config();
    let response_headers = prepare_streaming_headers(response.headers(), &mut timeout_config);

    let mut builder = axum::response::Response::builder().status(status);

    // 复制响应头
    for (key, value) in &response_headers {
        builder = builder.header(key, value);
    }

    // 创建字节流
    let stream = response.bytes_stream();

    // 创建使用量收集器；关闭 usage logging 时不要在流式热路径上解析每个 SSE event。
    let usage_collector = create_usage_collector(ctx, state, status.as_u16(), parser_config);

    // 创建带日志和超时的透传流
    let logged_stream = create_logged_passthrough_stream(
        stream,
        ctx.tag,
        usage_collector,
        timeout_config,
        connection_guard,
    );

    let body = axum::body::Body::from_stream(logged_stream);
    match builder.body(body) {
        Ok(resp) => resp,
        Err(e) => {
            log::error!("[{}] 构建流式响应失败: {e}", ctx.tag);
            ProxyError::Internal(format!("Failed to build streaming response: {e}")).into_response()
        }
    }
}

/// 处理非流式响应
pub async fn handle_non_streaming(
    response: ProxyResponse,
    ctx: &RequestContext,
    state: &ProxyState,
    parser_config: &UsageParserConfig,
    // guard 在函数 scope 内持有，整包响应读取完成后随函数返回一并 drop
    _connection_guard: Option<ActiveConnectionGuard>,
) -> Result<Response, ProxyError> {
    // 整包超时：仅在故障转移开启且配置值非零时生效
    let body_timeout =
        if ctx.app_config.auto_failover_enabled && ctx.app_config.non_streaming_timeout > 0 {
            Duration::from_secs(ctx.app_config.non_streaming_timeout as u64)
        } else {
            Duration::ZERO
        };
    let (mut response_headers, status, body_bytes) =
        read_decoded_body(response, ctx.tag, body_timeout).await?;
    strip_hop_by_hop_response_headers(&mut response_headers);

    log::debug!(
        "[{}] 上游响应体已接收: bytes={} (content omitted)",
        ctx.tag,
        body_bytes.len()
    );

    // Gateway quotas still need usage when optional diagnostic logging is off.
    if usage_collection_enabled(state, ctx.gateway_key_id.as_deref()) {
        if let Ok(json_value) = serde_json::from_slice::<Value>(&body_bytes) {
            // 解析使用量
            if let Some(usage) = (parser_config.response_parser)(&json_value) {
                // 归因优先级：usage 解析出的模型 → 响应 model 字段 → 映射后的出站
                // 模型（路由接管真值）→ 客户端请求模型。空字符串视为缺失。
                let model = usage
                    .model
                    .clone()
                    .filter(|m| !m.is_empty())
                    .or_else(|| {
                        json_value
                            .get("model")
                            .and_then(|m| m.as_str())
                            .filter(|m| !m.is_empty())
                            .map(str::to_string)
                    })
                    .or_else(|| ctx.outbound_model.clone())
                    .unwrap_or_else(|| ctx.request_model.clone());

                spawn_log_usage(
                    state,
                    ctx,
                    usage,
                    &model,
                    &ctx.request_model,
                    status.as_u16(),
                    false,
                );
            } else {
                let model = json_value
                    .get("model")
                    .and_then(|m| m.as_str())
                    .filter(|m| !m.is_empty())
                    .map(str::to_string)
                    .or_else(|| ctx.outbound_model.clone())
                    .unwrap_or_else(|| ctx.request_model.clone());
                spawn_log_usage(
                    state,
                    ctx,
                    TokenUsage::default(),
                    &model,
                    &ctx.request_model,
                    status.as_u16(),
                    false,
                );
                log::debug!(
                    "[{}] 未能解析 usage 信息，跳过记录",
                    parser_config.app_type_str
                );
            }
        } else {
            log::debug!(
                "[{}] <<< 响应 (非 JSON): {} bytes",
                ctx.tag,
                body_bytes.len()
            );
            spawn_log_usage(
                state,
                ctx,
                TokenUsage::default(),
                ctx.outbound_model.as_deref().unwrap_or(&ctx.request_model),
                &ctx.request_model,
                status.as_u16(),
                false,
            );
        }
    } else {
        log::debug!("[{}] usage logging 已关闭，跳过非流式 usage 解析", ctx.tag);
    }

    // 构建响应
    let mut builder = axum::response::Response::builder().status(status);
    for (key, value) in response_headers.iter() {
        builder = builder.header(key, value);
    }

    let body = axum::body::Body::from(body_bytes);
    builder.body(body).map_err(|e| {
        log::error!("[{}] 构建响应失败: {e}", ctx.tag);
        ProxyError::Internal(format!("Failed to build response: {e}"))
    })
}

/// 通用响应处理入口
///
/// 根据响应类型自动选择流式或非流式处理
pub async fn process_response(
    response: ProxyResponse,
    ctx: &RequestContext,
    state: &ProxyState,
    parser_config: &UsageParserConfig,
    request_is_stream: bool,
    connection_guard: Option<ActiveConnectionGuard>,
) -> Result<Response, ProxyError> {
    if is_sse_response(&response, request_is_stream) {
        Ok(handle_streaming(response, ctx, state, parser_config, connection_guard).await)
    } else {
        handle_non_streaming(response, ctx, state, parser_config, connection_guard).await
    }
}

// ============================================================================
// SSE 使用量收集器
// ============================================================================

const MAX_HEAD_USAGE_EVENTS: usize = 32;
const MAX_TAIL_USAGE_EVENTS: usize = 256;
const MAX_PENDING_SSE_BYTES: usize = 64 * 1024 * 1024;

type UsageCallbackWithTiming = Arc<dyn Fn(Vec<Value>, Option<u64>) + Send + Sync + 'static>;

#[derive(Default)]
struct CollectedSseEvents {
    head: Vec<Value>,
    tail: VecDeque<Value>,
    dropped: usize,
}

impl CollectedSseEvents {
    fn push(&mut self, event: Value) {
        if self.head.len() < MAX_HEAD_USAGE_EVENTS {
            self.head.push(event);
            return;
        }
        if self.tail.len() == MAX_TAIL_USAGE_EVENTS {
            self.tail.pop_front();
            self.dropped += 1;
        }
        self.tail.push_back(event);
    }

    fn into_vec(self) -> Vec<Value> {
        let mut events = self.head;
        events.extend(self.tail);
        events
    }
}

/// SSE 使用量收集器
#[derive(Clone)]
pub struct SseUsageCollector {
    inner: Arc<SseUsageCollectorInner>,
}

struct SseUsageCollectorInner {
    events: Mutex<CollectedSseEvents>,
    first_output_time: OnceLock<std::time::Instant>,
    start_time: std::time::Instant,
    on_complete: UsageCallbackWithTiming,
    should_collect: Option<StreamUsageEventFilter>,
    finished: AtomicBool,
}

impl SseUsageCollector {
    /// 创建使用量收集器；`should_collect` 用来在 hot path 跳过与 usage 无关的事件。
    pub fn new(
        start_time: std::time::Instant,
        should_collect: Option<StreamUsageEventFilter>,
        callback: impl Fn(Vec<Value>, Option<u64>) + Send + Sync + 'static,
    ) -> Self {
        let on_complete: UsageCallbackWithTiming = Arc::new(callback);
        Self {
            inner: Arc::new(SseUsageCollectorInner {
                events: Mutex::new(CollectedSseEvents::default()),
                first_output_time: OnceLock::new(),
                start_time,
                on_complete,
                should_collect,
                finished: AtomicBool::new(false),
            }),
        }
    }

    pub fn should_collect(&self, data: &str) -> bool {
        self.inner
            .should_collect
            .map(|filter| filter(data))
            .unwrap_or(true)
    }

    /// 观察一行 SSE data，记下首个输出事件的时间（`first_token_ms`）。
    ///
    /// 必须对每一行 data 调用，不能只看 usage 过滤器收下的事件：过滤器只挑带
    /// usage 的事件，上游的开头事件不带 usage 时，首个被收下的就是结尾的
    /// completed，生成时长被压到几毫秒，TPS 随之虚高。
    pub fn observe_data(&self, event_name: Option<&str>, data: &str) {
        if self.inner.first_output_time.get().is_some() {
            return;
        }
        if sse_data_starts_output(event_name, data) {
            let _ = self.inner.first_output_time.set(std::time::Instant::now());
        }
    }

    /// 推送 SSE 事件
    pub async fn push(&self, event: Value) {
        let mut events = self.inner.events.lock().await;
        events.push(event);
    }

    /// 完成收集并触发回调
    pub async fn finish(&self) {
        if self.inner.finished.swap(true, Ordering::SeqCst) {
            return;
        }

        let collected = {
            let mut guard = self.inner.events.lock().await;
            std::mem::take(&mut *guard)
        };
        if collected.dropped > 0 {
            log::debug!(
                "SSE usage collector retained {} head + {} tail events (dropped {} middle events)",
                collected.head.len(),
                collected.tail.len(),
                collected.dropped
            );
        }
        let events = collected.into_vec();

        let first_token_ms = self
            .inner
            .first_output_time
            .get()
            .map(|t| (*t - self.inner.start_time).as_millis() as u64);

        (self.inner.on_complete)(events, first_token_ms);
    }
}

/// 判断一行 SSE data 是否标志着模型开始输出（口径同 Sub2API 的 semantic TTFT）：
/// 跳过只宣告响应开始或保活的元数据事件，其余非空事件都算。Chat Completions /
/// Gemini 的分块没有事件类型，一律算。
pub(crate) fn sse_data_starts_output(event_name: Option<&str>, data: &str) -> bool {
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return false;
    }
    let event_type = match event_name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => name.to_string(),
        None => serde_json::from_str::<Value>(data)
            .ok()
            .and_then(|value| value.get("type")?.as_str().map(str::to_string))
            .unwrap_or_default(),
    };
    !matches!(
        event_type.as_str(),
        "response.created" | "response.in_progress" | "ping" | "keepalive"
    )
}

struct SseUsageFinishGuard {
    collector: Option<SseUsageCollector>,
}

impl SseUsageFinishGuard {
    fn new(collector: SseUsageCollector) -> Self {
        Self {
            collector: Some(collector),
        }
    }

    fn disarm(&mut self) {
        self.collector = None;
    }
}

impl Drop for SseUsageFinishGuard {
    fn drop(&mut self) {
        if let Some(collector) = self.collector.take() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    collector.finish().await;
                });
            } else {
                log::warn!("SSE 用量收尾保护触发时 Tokio runtime 不可用，跳过异步 finish");
            }
        }
    }
}

// ============================================================================
// 内部辅助函数
// ============================================================================

/// 创建使用量收集器
pub(crate) fn create_usage_collector(
    ctx: &RequestContext,
    state: &ProxyState,
    status_code: u16,
    parser_config: &UsageParserConfig,
) -> Option<SseUsageCollector> {
    if !usage_collection_enabled(state, ctx.gateway_key_id.as_deref()) {
        return None;
    }

    let state = state.clone();
    let provider_id = ctx.provider.id.clone();
    let multiplier = crate::provider::billing_multiplier(&ctx.provider);
    let request_model = ctx.request_model.clone();
    // 流式事件缺失模型名时的归因兜底：映射后的出站模型（路由接管真值）优先，
    // 其次才是客户端请求别名
    let fallback_model = ctx
        .outbound_model
        .clone()
        .unwrap_or_else(|| ctx.request_model.clone());
    // 用 ctx 的 app_type 而不是 parser_config 的：Claude Desktop 流式透传复用
    // CLAUDE_PARSER_CONFIG（app_type_str="claude"），按 parser_config 记账会把
    // claude-desktop 的行错记到 claude 名下，导致供应商计价覆盖解析不到。
    let app_type_str = ctx.app_type_str;
    let tag = ctx.tag;
    let start_time = ctx.start_time;
    let stream_parser = parser_config.stream_parser;
    let model_extractor = parser_config.model_extractor;
    let session_id = ctx.session_id.clone();
    // Fork：把本次请求归因到发起它的局域网网关密钥，供 per-key 配额统计。
    let gateway_key_id = ctx.gateway_key_id.clone();
    let diagnostics = ctx.diagnostics.clone();

    Some(SseUsageCollector::new(
        start_time,
        parser_config.stream_event_filter,
        move |events, first_token_ms| {
            if let Some(usage) = stream_parser(&events) {
                let model = model_extractor(&events, &fallback_model);
                let latency_ms = start_time.elapsed().as_millis() as u64;

                let state = state.clone();
                let provider_id = provider_id.clone();
                let session_id = session_id.clone();
                let request_model = request_model.clone();
                let outbound_model = fallback_model.clone();
                let gateway_key_id = gateway_key_id.clone();
                let diagnostics = diagnostics.clone();

                tokio::spawn(async move {
                    log_usage_internal(
                        &state,
                        &provider_id,
                        multiplier,
                        app_type_str,
                        &model,
                        &request_model,
                        &outbound_model,
                        usage,
                        latency_ms,
                        first_token_ms,
                        true, // is_streaming
                        status_code,
                        Some(session_id),
                        gateway_key_id,
                        diagnostics,
                    )
                    .await;
                });
            } else {
                let model = model_extractor(&events, &fallback_model);
                let latency_ms = start_time.elapsed().as_millis() as u64;
                let state = state.clone();
                let provider_id = provider_id.clone();
                let session_id = session_id.clone();
                let request_model = request_model.clone();
                let outbound_model = fallback_model.clone();
                let gateway_key_id = gateway_key_id.clone();
                let diagnostics = diagnostics.clone();

                tokio::spawn(async move {
                    log_usage_internal(
                        &state,
                        &provider_id,
                        multiplier,
                        app_type_str,
                        &model,
                        &request_model,
                        &outbound_model,
                        TokenUsage::default(),
                        latency_ms,
                        first_token_ms,
                        true, // is_streaming
                        status_code,
                        Some(session_id),
                        gateway_key_id,
                        diagnostics,
                    )
                    .await;
                });
                log::debug!("[{tag}] 流式响应缺少 usage 统计，跳过消费记录");
            }
        },
    ))
}

/// 异步记录使用量
fn spawn_log_usage(
    state: &ProxyState,
    ctx: &RequestContext,
    usage: TokenUsage,
    model: &str,
    request_model: &str,
    status_code: u16,
    is_streaming: bool,
) {
    if !usage_collection_enabled(state, ctx.gateway_key_id.as_deref()) {
        return;
    }

    let state = state.clone();
    let provider_id = ctx.provider.id.clone();
    let multiplier = crate::provider::billing_multiplier(&ctx.provider);
    let app_type_str = ctx.app_type_str.to_string();
    let model = model.to_string();
    let request_model = request_model.to_string();
    // 「按请求计价」模式的锚点：映射后的出站模型，无映射时等于 request_model
    let outbound_model = ctx
        .outbound_model
        .clone()
        .unwrap_or_else(|| ctx.request_model.clone());
    let latency_ms = ctx.latency_ms();
    let session_id = ctx.session_id.clone();
    let gateway_key_id = ctx.gateway_key_id.clone();
    let diagnostics = ctx.diagnostics.clone();

    tokio::spawn(async move {
        log_usage_internal(
            &state,
            &provider_id,
            multiplier,
            &app_type_str,
            &model,
            &request_model,
            &outbound_model,
            usage,
            latency_ms,
            None,
            is_streaming,
            status_code,
            Some(session_id),
            gateway_key_id,
            diagnostics,
        )
        .await;
    });
}

pub(crate) fn usage_logging_enabled(state: &ProxyState) -> bool {
    state
        .config
        .try_read()
        .map(|config| config.enable_logging)
        .unwrap_or(true)
}

/// Usage parsing is also the input to the durable gateway quota ledger. A
/// gateway request therefore remains collectible while diagnostic request
/// logging is disabled.
pub(crate) fn usage_collection_enabled(state: &ProxyState, gateway_key_id: Option<&str>) -> bool {
    gateway_key_id.is_some() || usage_logging_enabled(state)
}

/// 内部使用量记录函数
///
/// `outbound_model` 是「按请求计价」模式的锚点：实际发往上游的模型
/// （路由接管映射后的真值，无映射时等于 request_model）。该模式的语义是
/// 「按代理发出的请求计价、不信任上游回显」，接管场景下发出的请求模型是
/// 映射后的 Y 而非客户端别名 X，按 X 计价会用错定价表行。
#[allow(clippy::too_many_arguments)]
async fn log_usage_internal(
    state: &ProxyState,
    provider_id: &str,
    multiplier: rust_decimal::Decimal,
    app_type: &str,
    model: &str,
    request_model: &str,
    outbound_model: &str,
    usage: TokenUsage,
    latency_ms: u64,
    first_token_ms: Option<u64>,
    is_streaming: bool,
    status_code: u16,
    session_id: Option<String>,
    gateway_key_id: Option<String>,
    diagnostics: Option<super::diagnostics::DiagnosticHandle>,
) {
    use super::usage::logger::UsageLogger;

    let diagnostic_logging = usage_logging_enabled(state);
    if !diagnostic_logging && gateway_key_id.is_none() {
        return;
    }
    let dedup_scope = super::usage::parser::dedup_scope_for_app(app_type, provider_id);
    let request_id = usage.dedup_request_id(dedup_scope);
    if let Some(diagnostics) = diagnostics {
        diagnostics.bind(&request_id);
    }

    log::debug!(
        "[{app_type}] 记录请求日志: id={request_id}, provider={provider_id}, model={model}, streaming={is_streaming}, status={status_code}, latency_ms={latency_ms}, first_token_ms={first_token_ms:?}, session={}, input={}, output={}, cache_read={}, cache_creation={}",
        session_id.as_deref().unwrap_or("none"),
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_tokens,
        usage.cache_creation_tokens
    );

    // #7818：使用量写入要拿 Database 的单把 std::sync::Mutex<Connection> 并做
    // 磁盘 IO，同步执行会卡住 tokio worker，移到阻塞线程池执行。计费模式
    // 读取走同一把锁，一并移入。
    let db = state.db.clone();
    let provider_id = provider_id.to_string();
    let app_type = app_type.to_string();
    let model = model.to_string();
    let request_model = request_model.to_string();
    let outbound_model = outbound_model.to_string();
    let write = tokio::task::spawn_blocking(move || {
        let logger = UsageLogger::new(&db).with_diagnostic_logging(diagnostic_logging);
        // 计费模式读取的 DAO 是伪 async（无真实挂起点、直接取阻塞锁），
        // 在阻塞线程上 block_on 不会停转运行时
        let pricing_model_source = tokio::runtime::Handle::current()
            .block_on(logger.resolve_pricing_model_source(&app_type));
        let pricing_model = if pricing_model_source == PRICING_SOURCE_REQUEST {
            outbound_model
        } else {
            model.clone()
        };
        logger.log_with_calculation(
            request_id,
            provider_id,
            multiplier,
            app_type,
            model,
            request_model,
            pricing_model,
            usage,
            latency_ms,
            first_token_ms,
            status_code,
            session_id,
            None, // provider_type
            is_streaming,
            gateway_key_id,
        )
    });
    if let Err(e) = write.await.unwrap_or_else(|e| {
        Err(crate::error::AppError::Database(format!(
            "usage 记录任务失败: {e}"
        )))
    }) {
        log::warn!("[USG-001] 记录使用量失败: {e}");
    }
}

/// Keep an incomplete event private so comments can only be emitted between events.
/// Bytes are preserved exactly, including UTF-8 characters and mixed CRLF boundaries
/// split across transport chunks. Only scan newly appended bytes.
fn buffer_complete_sse_frames(pending: &mut bytes::BytesMut, bytes: &[u8]) -> Option<Bytes> {
    let start = pending.len();
    pending.extend_from_slice(bytes);
    let end = (start..pending.len()).rev().find_map(|i| {
        let prefix = &pending[..=i];
        (prefix.ends_with(b"\n\n") || prefix.ends_with(b"\n\r\n") || prefix.ends_with(b"\r\n\r\n"))
            .then_some(i + 1)
    })?;
    Some(pending.split_to(end).freeze())
}

/// SSE joins all data fields in one event with a newline. Keep the common
/// single-line case borrowed, and parse a multiline JSON event only once.
fn sse_event_data(block: &str) -> Option<Cow<'_, str>> {
    let mut lines = block
        .lines()
        .filter_map(|line| strip_sse_field(line, "data"));
    let first = lines.next()?;
    let Some(second) = lines.next() else {
        return Some(Cow::Borrowed(first));
    };
    let mut data = String::with_capacity(first.len() + second.len() + 1);
    data.push_str(first);
    data.push('\n');
    data.push_str(second);
    for line in lines {
        data.push('\n');
        data.push_str(line);
    }
    Some(Cow::Owned(data))
}

async fn observe_sse_event(
    collector: Option<&SseUsageCollector>,
    block: &str,
    loop_guard: bool,
    loop_text: &mut String,
    tag: &str,
) -> Result<(), std::io::Error> {
    let event_name = block
        .lines()
        .filter_map(|line| strip_sse_field(line, "event"))
        .next_back();
    let Some(data) = sse_event_data(block) else {
        return Ok(());
    };
    if let Some(collector) = collector {
        collector.observe_data(event_name, &data);
    }
    if data.trim() == "[DONE]" {
        return Ok(());
    }
    let collect_usage = collector.is_some_and(|collector| collector.should_collect(&data));
    let parsed = (collect_usage || loop_guard)
        .then(|| serde_json::from_str::<Value>(&data).ok())
        .flatten();
    let mut collected = false;
    if let Some(value) = parsed {
        if loop_guard {
            let delta = crate::proxy::reliability::extract_text_delta(event_name, &value);
            if !delta.is_empty() {
                loop_text.push_str(&delta);
                if loop_text.len() > 8192 {
                    let mut cut = loop_text.len() - 8192;
                    while cut < loop_text.len() && !loop_text.is_char_boundary(cut) {
                        cut += 1;
                    }
                    loop_text.drain(..cut);
                }
                if crate::proxy::reliability::tail_is_repeating(loop_text, 128, 30) {
                    return Err(std::io::Error::other("检测到流式死循环，已切断"));
                }
            }
        }
        if let Some(collector) = collector.filter(|_| collect_usage) {
            collector.push(value).await;
            collected = true;
        }
    }
    log::trace!(
        "[{tag}] <<< SSE data: bytes={}, usage_collected={collected} (content omitted)",
        data.len()
    );
    Ok(())
}

/// 创建带日志记录和超时控制的透传流
pub fn create_logged_passthrough_stream(
    stream: impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    tag: &'static str,
    usage_collector: Option<SseUsageCollector>,
    timeout_config: StreamingTimeoutConfig,
    connection_guard: Option<ActiveConnectionGuard>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    create_logged_passthrough_stream_with_max_pending_bytes(
        stream,
        tag,
        usage_collector,
        timeout_config,
        connection_guard,
        MAX_PENDING_SSE_BYTES,
    )
}

fn create_logged_passthrough_stream_with_max_pending_bytes(
    stream: impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    tag: &'static str,
    usage_collector: Option<SseUsageCollector>,
    timeout_config: StreamingTimeoutConfig,
    connection_guard: Option<ActiveConnectionGuard>,
    max_pending_sse_bytes: usize,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        let _conn_guard = connection_guard;
        let mut sse = SseBuffer::with_max_pending_bytes(max_pending_sse_bytes);
        let mut collector = usage_collector;
        let mut finish_guard = collector.clone().map(SseUsageFinishGuard::new);
        // Fork：loop_guard 开启时也需要解析 SSE（累计文本做死循环检测）。
        let loop_guard = timeout_config.loop_guard;
        // Keepalive also retains incomplete output events, so apply the shared
        // event budget even if diagnostics and usage collection are disabled.
        let inspect_sse_events = collector.is_some()
            || log::log_enabled!(log::Level::Debug)
            || loop_guard
            || timeout_config.keepalive_secs > 0;
        let mut sse_overflow_warned = false;
        let mut is_first_chunk = true;
        // Fork：死循环检测的滚动文本窗口（只保留尾部若干 KB 以限成本）。
        let mut loop_text = String::new();
        let mut pending_output = bytes::BytesMut::new();

        // 超时配置
        let first_byte_timeout = if timeout_config.first_byte_timeout > 0 {
            Some(Duration::from_secs(timeout_config.first_byte_timeout))
        } else {
            None
        };
        let idle_timeout = if timeout_config.idle_timeout > 0 {
            Some(Duration::from_secs(timeout_config.idle_timeout))
        } else {
            None
        };
        // Fork：上游静默时的 keep-alive 间隔（0=关闭）。
        let keepalive = if timeout_config.keepalive_secs > 0 {
            Some(Duration::from_secs(timeout_config.keepalive_secs))
        } else {
            None
        };

        let stream = bounded_sse_chunks(stream);
        tokio::pin!(stream);

        'outer: loop {
            // 硬超时：首字节超时或静默期超时（None = 无上限）
            let phase_timeout = if is_first_chunk {
                first_byte_timeout
            } else {
                idle_timeout
            };

            // 等待下一包时穿插 keep-alive 注释（上游静默），同时保持 phase_timeout 为硬上限。
            let mut waited = Duration::ZERO;
            let chunk_result = loop {
                let tick = match (keepalive, phase_timeout) {
                    (Some(k), Some(t)) => Some(k.min(t.saturating_sub(waited))),
                    (Some(k), None) => Some(k),
                    (None, Some(t)) => Some(t.saturating_sub(waited)),
                    (None, None) => None,
                };
                match tick {
                    Some(duration) => match tokio::time::timeout(duration, stream.next()).await {
                        Ok(got) => break got,
                        Err(_) => {
                            waited += duration;
                            if let Some(t) = phase_timeout {
                                if waited >= t {
                                    let timeout_type =
                                        if is_first_chunk { "首字节" } else { "静默期" };
                                    log::error!(
                                        "[{tag}] 流式响应{}超时 ({}秒)",
                                        timeout_type,
                                        t.as_secs()
                                    );
                                    yield Err(std::io::Error::other(format!(
                                        "流式响应{timeout_type}超时"
                                    )));
                                    break 'outer;
                                }
                            }
                            if keepalive.is_some() {
                                // Any incomplete trailing event is still private in
                                // pending_output, so this comment is emitted before it.
                                yield Ok(Bytes::from_static(
                                    crate::proxy::reliability::KEEPALIVE_COMMENT,
                                ));
                            }
                        }
                    },
                    None => break stream.next().await, // 无超时且无 keep-alive：直接等
                }
            };

            match chunk_result {
                Some(Ok(bytes)) => {
                    if is_first_chunk {
                        log::debug!(
                            "[{tag}] 已接收上游流式首包: bytes={}",
                            bytes.len()
                        );
                    }
                    is_first_chunk = false;
                    if inspect_sse_events {
                        if let Err(error) = sse.push_bytes(&bytes) {
                            yield Err(error);
                            break 'outer;
                        }
                        if sse.dropped_oversized_events() > 0 && !sse_overflow_warned {
                            sse_overflow_warned = true;
                            log::warn!(
                                "[{tag}] SSE inspection buffer exceeded {max_pending_sse_bytes} bytes; skipping oversized event(s) and resuming at next delimiter"
                            );
                        }

                        // 尝试解析并记录完整的 SSE 事件
                        while let Some(event_text) = sse.next_block() {
                            if let Err(error) = observe_sse_event(
                                collector.as_ref(), event_text, loop_guard, &mut loop_text, tag,
                            ).await {
                                yield Err(error);
                                break 'outer;
                            }
                        }
                    }

                    if keepalive.is_some() {
                        if let Some(complete) = buffer_complete_sse_frames(&mut pending_output, &bytes) {
                            yield Ok(complete);
                        }
                        if pending_output.len() > MAX_RESPONSE_BODY_BYTES {
                            yield Err(std::io::Error::other("SSE event exceeds response size limit"));
                            break;
                        }
                    } else {
                        yield Ok(bytes);
                    }
                }
                Some(Err(e)) => {
                    log::error!("[{tag}] 流错误: {e}");
                    yield Err(std::io::Error::other(e.to_string()));
                    break;
                }
                None => {
                    if inspect_sse_events && !sse.pending().trim().is_empty() {
                        let tail = sse.take_tail();
                        if let Err(error) = observe_sse_event(
                            collector.as_ref(), &tail, loop_guard, &mut loop_text, tag,
                        ).await {
                            yield Err(error);
                            break 'outer;
                        }
                    }
                    // Preserve an upstream tail without a final delimiter at EOF.
                    if !pending_output.is_empty() {
                        yield Ok(pending_output.split().freeze());
                    }
                    break;
                }
            }
        }

        if let Some(c) = collector.take() {
            c.finish().await;
        }
        if let Some(guard) = &mut finish_guard {
            guard.disarm();
        }
    }
}

fn is_safe_diagnostic_header(name: &str) -> bool {
    matches!(
        name,
        "content-type"
            | "content-encoding"
            | "content-length"
            | "retry-after"
            | "cf-ray"
            | "x-request-id"
            | "request-id"
            | "x-correlation-id"
    ) || name.starts_with("x-ratelimit-")
        || name.starts_with("ratelimit-")
}

fn bounded_header_value(value: &axum::http::HeaderValue) -> Option<String> {
    let value = value.to_str().ok()?;
    let mut bounded = value.chars().take(160).collect::<String>();
    if value.chars().count() > 160 {
        bounded.push('…');
    }
    Some(bounded)
}

fn format_headers(headers: &HeaderMap) -> String {
    let mut entries = headers
        .keys()
        .map(|key| {
            let name = key.as_str();
            if !is_safe_diagnostic_header(name) {
                return name.to_string();
            }

            let values = headers
                .get_all(key)
                .iter()
                .filter_map(bounded_header_value)
                .collect::<Vec<_>>();
            if values.is_empty() {
                name.to_string()
            } else {
                format!("{name}={}", values.join("|"))
            }
        })
        .collect::<Vec<_>>();
    entries.sort();
    format!("[{}]", entries.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_config::AppType;
    use crate::database::Database;
    use crate::error::AppError;
    use crate::provider::ProviderMeta;
    use rust_decimal::Decimal;
    use std::str::FromStr;
    use std::sync::Arc;

    #[tokio::test]
    async fn gateway_usage_collection_stays_enabled_when_diagnostics_are_disabled() {
        let state = crate::proxy::server::ProxyState::for_test(Arc::new(
            crate::database::Database::memory().unwrap(),
        ));
        state.config.write().await.enable_logging = false;

        assert!(super::usage_collection_enabled(&state, Some("gateway-key")));
        assert!(!super::usage_collection_enabled(&state, None));
    }

    fn reliability_timeouts(keepalive_secs: u64, loop_guard: bool) -> StreamingTimeoutConfig {
        StreamingTimeoutConfig {
            first_byte_timeout: 0,
            idle_timeout: 0,
            keepalive_secs,
            loop_guard,
        }
    }

    #[tokio::test]
    async fn native_multiline_sse_preserves_bytes_and_records_usage_for_every_protocol() {
        use crate::proxy::handler_config::{
            CLAUDE_PARSER_CONFIG, CODEX_PARSER_CONFIG, GEMINI_PARSER_CONFIG, OPENAI_PARSER_CONFIG,
        };

        for (parser, source, expected_input, expected_output) in [
            (
                CLAUDE_PARSER_CONFIG,
                concat!(
                    "event: message_start\r\ndata: {\"type\":\"message_start\",\r\n",
                    "data: \"message\":{\"id\":\"msg_multiline\",\"model\":\"claude\",\"usage\":{\"input_tokens\":100,\"output_tokens\":0}}}\r\n\r\n",
                    "event: message_delta\r\ndata: {\"type\":\"message_delta\",\r\n",
                    "data: \"usage\":{\"output_tokens\":25}}\r\n\r\n"
                ),
                100,
                25,
            ),
            (
                CODEX_PARSER_CONFIG,
                concat!(
                    "event: response.completed\ndata: {\"type\":\"response.completed\",\n",
                    "data: \"response\":{\"id\":\"resp_multiline\",\"model\":\"gpt-test\",\"usage\":{\"input_tokens\":101,\"output_tokens\":26}}}\n\n"
                ),
                101,
                26,
            ),
            (
                OPENAI_PARSER_CONFIG,
                concat!(
                    "data: {\"id\":\"chat_multiline\",\"model\":\"chat-test\",\n",
                    "data: \"usage\":{\"prompt_tokens\":102,\"completion_tokens\":27},\"choices\":[]}\n\n"
                ),
                102,
                27,
            ),
            (
                GEMINI_PARSER_CONFIG,
                concat!(
                    "data: {\"responseId\":\"gemini_multiline\",\"modelVersion\":\"gemini-test\",\n",
                    "data: \"usageMetadata\":{\"promptTokenCount\":103,\"candidatesTokenCount\":28,\"totalTokenCount\":131}}\n\n"
                ),
                103,
                28,
            ),
        ] {
            // Exercise a split inside the data field itself, not just event
            // boundaries, on the real passthrough/collector path.
            for split in [0, source.len() / 2, source.len()] {
                let captured = Arc::new(std::sync::Mutex::new(None));
                let collector = SseUsageCollector::new(
                    std::time::Instant::now(),
                    parser.stream_event_filter,
                    {
                        let captured = captured.clone();
                        move |events, _| {
                            *captured.lock().unwrap() = (parser.stream_parser)(&events);
                        }
                    },
                );
                let upstream = futures::stream::iter(vec![
                    Ok::<_, std::io::Error>(Bytes::copy_from_slice(&source.as_bytes()[..split])),
                    Ok(Bytes::copy_from_slice(&source.as_bytes()[split..])),
                ]);
                let results: Vec<_> = create_logged_passthrough_stream(
                    upstream,
                    "multiline-test",
                    Some(collector),
                    reliability_timeouts(0, false),
                    None,
                )
                .collect()
                .await;
                let bytes: Vec<_> = results
                    .into_iter()
                    .flat_map(|chunk| chunk.expect("passthrough").to_vec())
                    .collect();
                assert_eq!(bytes, source.as_bytes());
                let usage = captured.lock().unwrap().clone().expect("parsed multiline usage");
                assert_eq!(usage.input_tokens, expected_input, "{}", parser.app_type_str);
                assert_eq!(usage.output_tokens, expected_output, "{}", parser.app_type_str);
            }
        }
    }

    #[tokio::test]
    async fn native_multiline_usage_at_eof_is_recorded_once() {
        use crate::proxy::handler_config::OPENAI_PARSER_CONFIG;

        let source = concat!(
            "data: {\"id\":\"chat_eof\",\"model\":\"chat-test\",\n",
            "data: \"usage\":{\"prompt_tokens\":12,\"completion_tokens\":7},\"choices\":[]}"
        );
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let collector = SseUsageCollector::new(
            std::time::Instant::now(),
            OPENAI_PARSER_CONFIG.stream_event_filter,
            {
                let captured = captured.clone();
                move |events, _| *captured.lock().unwrap() = events
            },
        );
        let output: Vec<_> = create_logged_passthrough_stream(
            futures::stream::once(async move { Ok(Bytes::from_static(source.as_bytes())) }),
            "multiline-eof-test",
            Some(collector),
            reliability_timeouts(0, false),
            None,
        )
        .collect()
        .await;
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].as_ref().unwrap().as_ref(), source.as_bytes());
        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        let usage = (OPENAI_PARSER_CONFIG.stream_parser)(&events).expect("EOF usage");
        assert_eq!((usage.input_tokens, usage.output_tokens), (12, 7));
    }

    #[test]
    fn keepalive_framing_preserves_every_transport_split() {
        for delimiter in ["\n\n", "\r\n\r\n", "\r\n\n", "\n\r\n"] {
            let source =
                format!("data: {{\"text\":\"你好\"}}{delimiter}data: {{\"n\":2}}{delimiter}");
            for split in 0..=source.len() {
                let mut pending = bytes::BytesMut::new();
                let mut output = Vec::new();
                for chunk in [&source.as_bytes()[..split], &source.as_bytes()[split..]] {
                    if let Some(complete) = buffer_complete_sse_frames(&mut pending, chunk) {
                        output.extend_from_slice(&complete);
                    }
                }
                assert!(pending.is_empty(), "split={split}, delimiter={delimiter:?}");
                assert_eq!(output, source.as_bytes());
            }
        }
    }

    #[tokio::test]
    async fn keepalive_before_partial_utf8_event_does_not_corrupt_data() {
        let original = "data: {\"delta\":{\"text\":\"你好\"}}\r\n\r\n";
        let split = original.find('你').unwrap() + 1;
        let upstream = async_stream::stream! {
            yield Ok::<_, std::io::Error>(Bytes::copy_from_slice(&original.as_bytes()[..split]));
            tokio::time::sleep(Duration::from_millis(1100)).await;
            yield Ok(Bytes::copy_from_slice(&original.as_bytes()[split..]));
        };
        let mut stream = Box::pin(create_logged_passthrough_stream(
            upstream,
            "test",
            None,
            reliability_timeouts(1, false),
            None,
        ));
        let heartbeat = tokio::time::timeout(Duration::from_secs(3), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            heartbeat.as_ref(),
            crate::proxy::reliability::KEEPALIVE_COMMENT
        );
        let event = stream.next().await.unwrap().unwrap();
        assert_eq!(event.as_ref(), original.as_bytes());
        let event = std::str::from_utf8(&event).unwrap();
        let data = strip_sse_field(event.lines().next().unwrap(), "data").unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(data).unwrap()["delta"]["text"],
            "你好"
        );
    }

    #[tokio::test]
    async fn disabled_keepalive_keeps_transport_chunks_unchanged() {
        let chunks = vec![
            Bytes::from_static(b"data: {\"delta\":"),
            Bytes::from_static(b"\"text\"}\n\n"),
        ];
        let upstream = futures::stream::iter(chunks.clone().into_iter().map(Ok));
        let output: Vec<_> = create_logged_passthrough_stream(
            upstream,
            "test",
            None,
            reliability_timeouts(0, false),
            None,
        )
        .collect()
        .await;
        assert_eq!(
            output.into_iter().map(Result::unwrap).collect::<Vec<_>>(),
            chunks
        );
    }

    #[tokio::test]
    async fn keepalive_http_response_delivers_events_past_original_content_length() {
        use http_body_util::BodyExt;
        let (client, server) = tokio::io::duplex(4096);
        let server_task = tokio::spawn(async move {
            hyper::server::conn::http1::Builder::new()
                .serve_connection(
                    hyper_util::rt::TokioIo::new(server),
                    hyper::service::service_fn(|_| async {
                        let upstream = async_stream::stream! {
                            yield Ok(Bytes::from_static(b"data: {}\n\n"));
                            tokio::time::sleep(Duration::from_millis(1100)).await;
                            yield Ok(Bytes::from_static(b"data: {}\n\n"));
                        };
                        let mut headers = HeaderMap::new();
                        headers.insert("content-type", "text/event-stream".parse().unwrap());
                        headers.insert("content-length", "20".parse().unwrap());
                        let mut config = reliability_timeouts(1, false);
                        let headers = prepare_streaming_headers(&headers, &mut config);
                        let stream =
                            create_logged_passthrough_stream(upstream, "test", None, config, None);
                        let mut response = Response::new(axum::body::Body::from_stream(stream));
                        *response.headers_mut() = headers;
                        Ok::<_, std::convert::Infallible>(response)
                    }),
                )
                .await
                .unwrap();
        });
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(client))
                .await
                .unwrap();
        let client_task = tokio::spawn(connection);
        let response = sender
            .send_request(
                http::Request::builder()
                    .uri("http://localhost/")
                    .body(http_body_util::Empty::<Bytes>::new())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(!response.headers().contains_key("content-length"));
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(text.matches("data: {}\n\n").count(), 2, "{text:?}");
        assert!(text.contains(": keep-alive\n\n"), "{text:?}");
        drop(sender);
        client_task.await.unwrap().unwrap();
        server_task.await.unwrap();
    }

    #[test]
    fn compressed_passthrough_disables_byte_rewriting_and_keeps_entity_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", "gzip".parse().unwrap());
        headers.insert("content-length", "20".parse().unwrap());
        let mut config = reliability_timeouts(1, true);
        let prepared = prepare_streaming_headers(&headers, &mut config);
        assert_eq!(prepared["content-encoding"], "gzip");
        assert_eq!(prepared["content-length"], "20");
        assert_eq!(config.keepalive_secs, 0);
        assert!(!config.loop_guard);
    }

    async fn loop_guard_result(event: Value) -> (usize, bool) {
        let bytes = Bytes::from(format!("data: {event}\n\n"));
        let upstream = futures::stream::iter((0..40).map(move |_| Ok(bytes.clone())));
        let results: Vec<_> = create_logged_passthrough_stream(
            upstream,
            "test",
            None,
            reliability_timeouts(0, true),
            None,
        )
        .collect()
        .await;
        (
            results.iter().filter(|item| item.is_ok()).count(),
            results.iter().any(|item| item.is_err()),
        )
    }

    #[tokio::test]
    async fn loop_guard_preserves_tool_arguments_but_stops_text_loops_across_protocols() {
        let (events, failed) = loop_guard_result(
            serde_json::json!({"type":"response.function_call_arguments.delta", "delta":"0,"}),
        )
        .await;
        assert_eq!(events, 40);
        assert!(!failed, "valid repeated tool arguments must not be cut off");
        for event in [
            serde_json::json!({"type":"response.output_text.delta", "delta":"ha "}),
            serde_json::json!({"delta":{"type":"text_delta","text":"ha "}}),
            serde_json::json!({"choices":[{"delta":{"content":"ha "}}]}),
            serde_json::json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"tool"}},{"text":"ha "}]}}]}),
        ] {
            let (events, failed) = loop_guard_result(event.clone()).await;
            assert!(failed && events < 40, "text loop was not stopped: {event}");
        }
    }

    #[tokio::test]
    async fn billing_multiplier_request_snapshot_survives_provider_change_and_delete(
    ) -> Result<(), AppError> {
        let db = Arc::new(Database::memory()?);
        seed_pricing(&db)?;
        insert_provider(
            &db,
            "snapshot",
            "claude",
            ProviderMeta {
                billing_multiplier: Some("0.5".into()),
                ..Default::default()
            },
        )?;
        let selected = db
            .get_provider_by_id("snapshot", &crate::app_config::AppType::Claude)?
            .unwrap();
        let multiplier = crate::provider::billing_multiplier(&selected);
        let state = build_state(db.clone());
        for (index, mutation) in [
            "UPDATE providers SET meta='{}' WHERE id='snapshot'",
            "DELETE FROM providers WHERE id='snapshot'",
        ]
        .iter()
        .enumerate()
        {
            {
                let conn = crate::database::lock_conn!(db.conn);
                conn.execute(mutation, [])?;
            }
            log_usage_internal(
                &state,
                "snapshot",
                multiplier,
                "claude",
                "resp-model",
                "resp-model",
                "resp-model",
                TokenUsage {
                    input_tokens: 1000000,
                    message_id: Some(format!("snapshot-{index}")),
                    ..Default::default()
                },
                1,
                None,
                index == 1,
                200,
                None,
                None,
                None,
            )
            .await;
        }
        let conn = crate::database::lock_conn!(db.conn);
        let rows: i64 = conn.query_row("SELECT COUNT(*) FROM proxy_request_logs WHERE provider_id='snapshot' AND cost_multiplier='0.5' AND CAST(total_cost_usd AS REAL)=0.5", [], |r| r.get(0))?;
        assert_eq!(rows, 2);
        Ok(())
    }

    #[test]
    fn sse_data_starts_output_skips_metadata_events() {
        assert!(!sse_data_starts_output(
            Some("response.created"),
            r#"{"type":"response.created"}"#
        ));
        assert!(!sse_data_starts_output(
            None,
            r#"{"type":"response.in_progress","response":{}}"#
        ));
        assert!(!sse_data_starts_output(Some("ping"), r#"{"type":"ping"}"#));
        assert!(!sse_data_starts_output(None, "[DONE]"));
        assert!(!sse_data_starts_output(None, "  "));

        assert!(sse_data_starts_output(
            Some("response.output_text.delta"),
            r#"{"type":"response.output_text.delta","delta":"hi"}"#
        ));
        assert!(sse_data_starts_output(
            Some("message_start"),
            r#"{"type":"message_start"}"#
        ));
        // Chat Completions / Gemini 分块没有事件类型
        assert!(sse_data_starts_output(
            None,
            r#"{"choices":[{"delta":{"content":"hi"}}]}"#
        ));
    }

    /// 回归：上游 response.created 不带 usage 时，首个被 usage 过滤器收下的是
    /// 结尾的 completed；首 token 时间必须仍落在首个输出事件上。
    #[tokio::test]
    async fn first_token_ms_tracks_first_output_not_first_usage_event() {
        let start = std::time::Instant::now();
        let recorded = Arc::new(std::sync::Mutex::new(None));
        let recorded_in_cb = recorded.clone();
        let collector = SseUsageCollector::new(
            start,
            Some(super::super::handler_config::codex_stream_usage_event_filter),
            move |events, first_token_ms| {
                *recorded_in_cb.lock().unwrap() = Some((
                    events.len(),
                    first_token_ms,
                    start.elapsed().as_millis() as u64,
                ));
            },
        );

        let upstream = async_stream::stream! {
            yield Ok::<_, std::io::Error>(Bytes::from(
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{}}\n\n",
            ));
            tokio::time::sleep(Duration::from_millis(60)).await;
            yield Ok(Bytes::from(
                "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            ));
            tokio::time::sleep(Duration::from_millis(60)).await;
            yield Ok(Bytes::from(
                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n",
            ));
        };
        let stream = create_logged_passthrough_stream(
            upstream,
            "test",
            Some(collector),
            StreamingTimeoutConfig {
                first_byte_timeout: 0,
                idle_timeout: 0,
                keepalive_secs: 0,
                loop_guard: false,
            },
            None,
        );
        futures::pin_mut!(stream);
        while stream.next().await.is_some() {}

        let (collected, first_token_ms, latency_ms) =
            recorded.lock().unwrap().expect("collector finished");
        assert_eq!(collected, 1, "only the completed event carries usage");
        let first_token_ms = first_token_ms.expect("first output recorded");
        assert!(first_token_ms >= 50, "first_token_ms={first_token_ms}");
        assert!(
            latency_ms >= first_token_ms + 50,
            "first_token_ms={first_token_ms}, latency_ms={latency_ms}"
        );
    }

    #[test]
    fn format_headers_keeps_only_allowlisted_diagnostic_values() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer super-secret".parse().unwrap());
        headers.insert("set-cookie", "session=cookie-secret".parse().unwrap());
        headers.insert("retry-after", "30".parse().unwrap());
        headers.insert("x-ratelimit-remaining", "2".parse().unwrap());
        headers.insert("cf-ray", "abc123-SJC".parse().unwrap());

        let formatted = format_headers(&headers);
        assert!(formatted.contains("authorization"), "{formatted}");
        assert!(formatted.contains("set-cookie"), "{formatted}");
        assert!(formatted.contains("retry-after=30"), "{formatted}");
        assert!(formatted.contains("x-ratelimit-remaining=2"), "{formatted}");
        assert!(formatted.contains("cf-ray=abc123-SJC"), "{formatted}");
        assert!(!formatted.contains("super-secret"), "{formatted}");
        assert!(!formatted.contains("cookie-secret"), "{formatted}");
    }

    #[tokio::test]
    async fn read_decoded_body_rejects_compressed_bomb_without_full_expansion() {
        // 128 MiB+1 全零 payload 的 gzip 只有 ~130 KiB：原始读取上限拦不住它，
        // 只有解压侧的有界解码能拒绝。若解码退化为"先完整展开再比较"，
        // 展开后长度 > MAX_RESPONSE_BODY_BYTES 的 payload 会成功返回（测试失败）。
        let payload = vec![0u8; MAX_RESPONSE_BODY_BYTES + 1];
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &payload).unwrap();
        let compressed = encoder.finish().unwrap();
        assert!(compressed.len() < MAX_RESPONSE_BODY_BYTES);

        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", "gzip".parse().unwrap());
        let response =
            ProxyResponse::buffered(http::StatusCode::OK, headers, Bytes::from(compressed));

        let result = read_decoded_body(response, "test", Duration::ZERO).await;
        assert!(
            matches!(result, Err(ProxyError::ResponseBodyTooLarge(_))),
            "压缩炸弹应被拒绝而不是完整展开: {:?}",
            result.map(|(_, _, body)| body.len())
        );
    }

    fn response_with_content_type(
        status: http::StatusCode,
        content_type: Option<&'static str>,
    ) -> ProxyResponse {
        let mut headers = HeaderMap::new();
        if let Some(content_type) = content_type {
            headers.insert("content-type", content_type.parse().unwrap());
        }
        ProxyResponse::buffered(status, headers, Bytes::new())
    }

    #[test]
    fn stream_request_without_content_type_is_treated_as_sse() {
        // chatgpt.com 的 Codex Responses 回包不带 Content-Type。
        let response = response_with_content_type(http::StatusCode::OK, None);
        assert!(is_sse_response(&response, true));
        assert!(!is_sse_response(&response, false));
    }

    #[test]
    fn declared_content_type_wins_over_stream_flag() {
        let sse = response_with_content_type(
            http::StatusCode::OK,
            Some("text/event-stream;charset=utf-8"),
        );
        assert!(is_sse_response(&sse, false));

        // 网关忽略 stream: true 时回的 JSON（无论标成什么）仍按整包解析用量。
        for content_type in ["application/json", "text/plain"] {
            let response = response_with_content_type(http::StatusCode::OK, Some(content_type));
            assert!(!is_sse_response(&response, true), "{content_type}");
        }
    }

    #[test]
    fn stream_request_error_without_content_type_stays_buffered() {
        let response = response_with_content_type(http::StatusCode::BAD_REQUEST, None);
        assert!(!is_sse_response(&response, true));
    }

    #[test]
    fn test_strip_sse_field_accepts_optional_space() {
        assert_eq!(
            super::strip_sse_field("data: {\"ok\":true}", "data"),
            Some("{\"ok\":true}")
        );
        assert_eq!(
            super::strip_sse_field("data:{\"ok\":true}", "data"),
            Some("{\"ok\":true}")
        );
        assert_eq!(
            super::strip_sse_field("event: message_start", "event"),
            Some("message_start")
        );
        assert_eq!(
            super::strip_sse_field("event:message_start", "event"),
            Some("message_start")
        );
        assert_eq!(super::strip_sse_field("id:1", "data"), None);
    }

    #[test]
    fn test_strip_hop_by_hop_response_headers_removes_standard_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONNECTION,
            axum::http::HeaderValue::from_static("keep-alive"),
        );
        headers.insert(
            axum::http::header::HeaderName::from_static("keep-alive"),
            axum::http::HeaderValue::from_static("timeout=5"),
        );
        headers.insert(
            axum::http::header::TRANSFER_ENCODING,
            axum::http::HeaderValue::from_static("chunked"),
        );
        headers.insert(
            axum::http::header::HeaderName::from_static("proxy-connection"),
            axum::http::HeaderValue::from_static("keep-alive"),
        );
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            axum::http::header::CONTENT_LENGTH,
            axum::http::HeaderValue::from_static("12"),
        );

        strip_hop_by_hop_response_headers(&mut headers);

        assert!(!headers.contains_key(axum::http::header::CONNECTION));
        assert!(!headers.contains_key("keep-alive"));
        assert!(!headers.contains_key(axum::http::header::TRANSFER_ENCODING));
        assert!(!headers.contains_key("proxy-connection"));
        assert_eq!(
            headers.get(axum::http::header::CONTENT_TYPE),
            Some(&axum::http::HeaderValue::from_static("application/json"))
        );
        assert_eq!(
            headers.get(axum::http::header::CONTENT_LENGTH),
            Some(&axum::http::HeaderValue::from_static("12"))
        );
    }

    #[test]
    fn test_strip_hop_by_hop_response_headers_removes_connection_listed_extensions() {
        let mut headers = HeaderMap::new();
        headers.append(
            axum::http::header::CONNECTION,
            axum::http::HeaderValue::from_static("x-trace-hop, x-debug-hop"),
        );
        headers.append(
            axum::http::header::CONNECTION,
            axum::http::HeaderValue::from_static("upgrade"),
        );
        headers.insert(
            axum::http::header::HeaderName::from_static("x-trace-hop"),
            axum::http::HeaderValue::from_static("trace"),
        );
        headers.insert(
            axum::http::header::HeaderName::from_static("x-debug-hop"),
            axum::http::HeaderValue::from_static("debug"),
        );
        headers.insert(
            axum::http::header::UPGRADE,
            axum::http::HeaderValue::from_static("websocket"),
        );
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/event-stream"),
        );

        strip_hop_by_hop_response_headers(&mut headers);

        assert!(!headers.contains_key(axum::http::header::CONNECTION));
        assert!(!headers.contains_key("x-trace-hop"));
        assert!(!headers.contains_key("x-debug-hop"));
        assert!(!headers.contains_key(axum::http::header::UPGRADE));
        assert_eq!(
            headers.get(axum::http::header::CONTENT_TYPE),
            Some(&axum::http::HeaderValue::from_static("text/event-stream"))
        );
    }

    fn build_state(db: Arc<Database>) -> ProxyState {
        ProxyState::for_test(db)
    }

    #[tokio::test]
    async fn diagnostics_bind_the_existing_deduplication_id() {
        let db = Arc::new(Database::memory().unwrap());
        let state = build_state(db);
        let diagnostics = super::super::diagnostics::DiagnosticHandle::isolated(false);
        let usage = TokenUsage {
            message_id: Some("msg-diagnostics".into()),
            input_tokens: 1,
            ..Default::default()
        };
        log_usage_internal(
            &state,
            "provider",
            rust_decimal::Decimal::ONE,
            "claude",
            "m",
            "m",
            "m",
            usage,
            1,
            None,
            false,
            200,
            None,
            None,
            Some(diagnostics.clone()),
        )
        .await;
        assert!(diagnostics.snapshot("session:msg-diagnostics").is_some());
    }

    fn seed_pricing(db: &Database) -> Result<(), AppError> {
        let conn = crate::database::lock_conn!(db.conn);
        conn.execute(
            "INSERT OR REPLACE INTO model_pricing (model_id, display_name, input_cost_per_million, output_cost_per_million)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["resp-model", "Resp Model", "1.0", "0"],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        conn.execute(
            "INSERT OR REPLACE INTO model_pricing (model_id, display_name, input_cost_per_million, output_cost_per_million)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["req-model", "Req Model", "2.0", "0"],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    fn insert_provider(
        db: &Database,
        id: &str,
        app_type: &str,
        meta: ProviderMeta,
    ) -> Result<(), AppError> {
        let meta_json =
            serde_json::to_string(&meta).map_err(|e| AppError::Database(e.to_string()))?;
        let conn = crate::database::lock_conn!(db.conn);
        conn.execute(
            "INSERT INTO providers (id, app_type, name, settings_config, meta)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, app_type, "Test Provider", "{}", meta_json],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    #[tokio::test]
    async fn test_log_usage_ignores_legacy_multiplier_and_provider_overrides(
    ) -> Result<(), AppError> {
        let db = Arc::new(Database::memory()?);
        let app_type = "claude";

        db.set_pricing_model_source(&AppType::Claude, "response")
            .await?;
        seed_pricing(&db)?;
        {
            // 旧版写下的全局倍率仍留在列里，新版不再读取
            let conn = crate::database::lock_conn!(db.conn);
            conn.execute(
                "UPDATE proxy_config SET default_cost_multiplier = '1.5' WHERE app_type = ?1",
                [app_type],
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        }

        // 旧版的供应商级覆盖仍在 meta 里（给旧设备往返保留），新版不再读取
        let meta = ProviderMeta {
            cost_multiplier: Some("2".to_string()),
            pricing_model_source: Some("request".to_string()),
            ..ProviderMeta::default()
        };
        insert_provider(&db, "provider-1", app_type, meta)?;

        let state = build_state(db.clone());
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cache_creation_1h_tokens: 0,
            model: None,
            message_id: None,
        };

        log_usage_internal(
            &state,
            "provider-1",
            rust_decimal::Decimal::ONE,
            app_type,
            "resp-model",
            "req-model",
            "req-model",
            usage,
            10,
            None,
            false,
            200,
            None,
            None,
            None,
        )
        .await;

        let conn = crate::database::lock_conn!(db.conn);
        let (total_cost, cost_multiplier): (String, String) = conn
            .query_row(
                "SELECT total_cost_usd, cost_multiplier
                 FROM proxy_request_logs WHERE provider_id = ?1",
                ["provider-1"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| AppError::Database(e.to_string()))?;

        assert_eq!(Decimal::from_str(&cost_multiplier).unwrap(), Decimal::ONE);
        // 按全局的「返回模型」计价：resp-model $1/M，不乘倍率
        assert_eq!(
            Decimal::from_str(&total_cost).unwrap(),
            Decimal::from_str("1").unwrap()
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_request_pricing_mode_anchors_to_outbound_model() -> Result<(), AppError> {
        let db = Arc::new(Database::memory()?);
        let app_type = "claude";

        db.set_pricing_model_source(&AppType::Claude, "request")
            .await?;
        seed_pricing(&db)?;
        {
            let conn = crate::database::lock_conn!(db.conn);
            conn.execute(
                "INSERT OR REPLACE INTO model_pricing (model_id, display_name, input_cost_per_million, output_cost_per_million)
                 VALUES ('outbound-model', 'Outbound Model', '4.0', '0')",
                [],
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        }

        insert_provider(&db, "provider-3", app_type, ProviderMeta::default())?;

        let state = build_state(db.clone());
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cache_creation_1h_tokens: 0,
            model: None,
            message_id: None,
        };

        // 路由接管场景：客户端请求 req-model（$2/M），代理实际发出 outbound-model
        // （$4/M），上游回显 resp-model。「按请求计价」必须锚定实际发出的模型。
        log_usage_internal(
            &state,
            "provider-3",
            rust_decimal::Decimal::ONE,
            app_type,
            "resp-model",
            "req-model",
            "outbound-model",
            usage,
            10,
            None,
            false,
            200,
            None,
            None,
            None,
        )
        .await;

        let conn = crate::database::lock_conn!(db.conn);
        let (model, request_model, total_cost): (String, String, String) = conn
            .query_row(
                "SELECT model, request_model, total_cost_usd
                 FROM proxy_request_logs WHERE provider_id = ?1",
                ["provider-3"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|e| AppError::Database(e.to_string()))?;

        // model / request_model 列不受计价锚点影响
        assert_eq!(model, "resp-model");
        assert_eq!(request_model, "req-model");
        // 按 outbound-model（$4/M）计价，而不是 req-model（$2/M）或 resp-model（$1/M）
        assert_eq!(
            Decimal::from_str(&total_cost).unwrap(),
            Decimal::from_str("4").unwrap()
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_claude_desktop_inherits_claude_pricing_source() -> Result<(), AppError> {
        use crate::proxy::usage::logger::UsageLogger;

        let db = Arc::new(Database::memory()?);

        // proxy_config 没有 claude-desktop 行；它的计费模式必须继承 claude，
        // 而不是静默落回工厂默认（response）
        db.set_pricing_model_source(&AppType::Claude, "request")
            .await?;

        let logger = UsageLogger::new(&db);
        let source = logger.resolve_pricing_model_source("claude-desktop").await;

        assert_eq!(source, "request");
        Ok(())
    }

    // #7818 回归闸门：使用量写入必须离开异步执行器线程。
    // Database 全局只有单把 std::sync::Mutex<Connection>，先让专用线程占住
    // 这把锁模拟慢盘/竞争。修复前，log_usage_internal 会在当前 tokio worker
    // 上同步等锁，单线程（current_thread）运行时被整个卡死，下方的心跳
    // sleep 只能在锁释放（约 500ms）后才会醒来；修复后写入走阻塞线程池，
    // sleep 照常在 100ms 触发。
    #[tokio::test]
    async fn log_usage_write_does_not_block_the_async_runtime() -> Result<(), AppError> {
        use std::thread;
        use std::time::{Duration, Instant};

        let db = Arc::new(Database::memory()?);
        let app_type = "claude";

        db.set_pricing_model_source(&AppType::Claude, "response")
            .await?;
        seed_pricing(&db)?;
        insert_provider(&db, "provider-busy", app_type, ProviderMeta::default())?;

        let state = build_state(db.clone());
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cache_creation_1h_tokens: 0,
            model: None,
            message_id: None,
        };

        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let blocker_db = db.clone();
        let blocker = thread::spawn(move || {
            let _guard = blocker_db.conn.lock().expect("占锁失败");
            locked_tx.send(()).expect("通知占锁失败");
            // 无论被测代码走哪条路径都定时释放，测试不会挂死
            thread::sleep(Duration::from_millis(500));
        });
        locked_rx.recv().expect("接收占锁通知失败");

        let writer = tokio::spawn(async move {
            log_usage_internal(
                &state,
                "provider-busy",
                rust_decimal::Decimal::ONE,
                app_type,
                "resp-model",
                "req-model",
                "req-model",
                usage,
                10,
                None,
                false,
                200,
                None,
                None,
                None,
            )
            .await;
        });

        let start = Instant::now();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let heartbeat = start.elapsed();
        blocker.join().expect("占锁线程 panic");
        writer.await.expect("使用量写入任务 panic");

        assert!(
            heartbeat < Duration::from_millis(300),
            "使用量写入阻塞了异步执行器 {heartbeat:?}（应移到阻塞线程池执行）"
        );

        // 锁释放后记录最终仍要落库
        let conn = crate::database::lock_conn!(db.conn);
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM proxy_request_logs WHERE provider_id = ?1",
                ["provider-busy"],
                |row| row.get(0),
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        assert_eq!(count, 1);
        Ok(())
    }

    /// Gemini `alt=sse` 流式响应（Antigravity 等 Gemini 客户端）经透传流后，
    /// 尾 chunk 的 usageMetadata 必须能被收集并解析成 token usage。
    #[tokio::test]
    async fn gemini_sse_stream_feeds_collector_and_parses_usage() {
        use crate::proxy::handler_config::GEMINI_PARSER_CONFIG;
        use crate::proxy::usage::parser::TokenUsage;
        use futures::StreamExt;

        let sse_body = concat!(
            "data: {\"responseId\":\"resp_1\",\"modelVersion\":\"gemini-3.8-flash\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"he\"}]}}]}\n\n",
            "data: {\"responseId\":\"resp_1\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"llo\"}]}}]}\n\n",
            "data: {\"responseId\":\"resp_1\",\"candidates\":[],\"usageMetadata\":{\"promptTokenCount\":258,\"totalTokenCount\":277,\"cachedContentTokenCount\":160}}\n\n",
        );

        let collected = Arc::new(std::sync::Mutex::new(Vec::new()));
        let collector = SseUsageCollector::new(
            std::time::Instant::now(),
            GEMINI_PARSER_CONFIG.stream_event_filter,
            {
                let collected = collected.clone();
                move |events, _| {
                    *collected.lock().unwrap() = events;
                }
            },
        );

        let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(
            sse_body.as_bytes().to_vec().into(),
        )]);
        let passthrough = create_logged_passthrough_stream(
            stream,
            "test",
            Some(collector),
            StreamingTimeoutConfig {
                first_byte_timeout: 0,
                idle_timeout: 0,
                keepalive_secs: 0,
                loop_guard: false,
            },
            None,
        );
        futures::pin_mut!(passthrough);
        while let Some(chunk) = passthrough.next().await {
            chunk.expect("passthrough chunk");
        }

        let events = collected.lock().unwrap().clone();
        assert_eq!(
            events.len(),
            2,
            "含 modelVersion 的首 chunk 与含 usageMetadata 的尾 chunk 都应被收集: {events:?}"
        );
        let usage =
            TokenUsage::from_gemini_stream_chunks(&events).expect("Gemini 流式 usage 应可解析");
        assert_eq!(usage.input_tokens, 258);
        assert_eq!(usage.output_tokens, 19);
        assert_eq!(usage.cache_read_tokens, 160);
        assert_eq!(usage.model.as_deref(), Some("gemini-3.8-flash"));
        assert_eq!(usage.message_id.as_deref(), Some("resp_1"));
    }

    #[tokio::test]
    async fn sse_usage_collector_bounds_events_and_preserves_head_and_tail() {
        let collected = Arc::new(std::sync::Mutex::new(Vec::new()));
        let collector = SseUsageCollector::new(std::time::Instant::now(), None, {
            let collected = collected.clone();
            move |events, _| {
                *collected.lock().unwrap() = events;
            }
        });

        let total = 1200usize;
        collector
            .push(serde_json::json!({
                "type": "message_start",
                "index": 0,
                "message": {
                    "id": "msg_1",
                    "model": "claude-sonnet-4-5",
                    "usage": { "input_tokens": 123 }
                }
            }))
            .await;
        for i in 1..(total - 1) {
            collector
                .push(serde_json::json!({
                    "type": "content_block_delta",
                    "index": i
                }))
                .await;
        }
        collector
            .push(serde_json::json!({
                "type": "message_delta",
                "index": total - 1,
                "usage": { "output_tokens": 45 }
            }))
            .await;

        collector.finish().await;

        let events = collected.lock().unwrap().clone();
        assert_eq!(events.len(), MAX_HEAD_USAGE_EVENTS + MAX_TAIL_USAGE_EVENTS);
        let retained_indices: Vec<usize> = events
            .iter()
            .map(|e| e["index"].as_u64().unwrap() as usize)
            .collect();
        let expected_head: Vec<usize> = (0..MAX_HEAD_USAGE_EVENTS).collect();
        let expected_tail: Vec<usize> = ((total - MAX_TAIL_USAGE_EVENTS)..total).collect();
        assert_eq!(&retained_indices[..MAX_HEAD_USAGE_EVENTS], expected_head);
        assert_eq!(&retained_indices[MAX_HEAD_USAGE_EVENTS..], expected_tail);

        let usage = TokenUsage::from_claude_stream_events(&events).expect("usage parsed");
        assert_eq!(usage.input_tokens, 123);
        assert_eq!(usage.output_tokens, 45);
        assert_eq!(usage.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(usage.message_id.as_deref(), Some("msg_1"));
    }

    #[tokio::test]
    async fn logged_passthrough_stream_keeps_client_bytes_and_resumes_after_overflow() {
        let head = Bytes::from("data: {\"type\":\"message_start\"}\n\n");
        let blob = "a".repeat(2048);
        let giant_event = Bytes::from(format!("data: {{\"blob\":\"{blob}\"}}\n\n"));
        let tail = Bytes::from("data: {\"type\":\"message_delta\"}\n\n");
        let done = Bytes::from("data: [DONE]\n\n");
        let expected_len = head.len() + giant_event.len() + tail.len() + done.len();

        let collected = Arc::new(std::sync::Mutex::new(Vec::new()));
        let collector = SseUsageCollector::new(std::time::Instant::now(), None, {
            let collected = collected.clone();
            move |events, _| {
                *collected.lock().unwrap() = events;
            }
        });

        let upstream = futures::stream::iter(vec![
            Ok::<_, std::io::Error>(head),
            Ok(giant_event),
            Ok(tail),
            Ok(done),
        ]);
        let passthrough = create_logged_passthrough_stream_with_max_pending_bytes(
            upstream,
            "test",
            Some(collector),
            StreamingTimeoutConfig {
                first_byte_timeout: 0,
                idle_timeout: 0,
                ..Default::default()
            },
            None,
            1024,
        );
        futures::pin_mut!(passthrough);
        let mut client_bytes = 0usize;
        while let Some(chunk) = passthrough.next().await {
            client_bytes += chunk.expect("client passthrough chunk").len();
        }

        assert_eq!(client_bytes, expected_len);
        let events = collected.lock().unwrap().clone();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "message_start");
        assert_eq!(events[1]["type"], "message_delta");
    }

    #[tokio::test]
    async fn logged_passthrough_stream_collects_large_image_generation_completed_usage() {
        use crate::proxy::handler_config::codex_stream_usage_event_filter;

        let b64_image = "A".repeat(5 * 1024 * 1024);
        let mut payload = String::with_capacity(5 * 1024 * 1024 + 256);
        payload.push_str("event: image_generation.completed\n");
        payload.push_str("data: {\"type\":\"image_generation.completed\",\"b64_json\":\"");
        payload.push_str(&b64_image);
        payload.push_str("\",\"usage\":{\"input_tokens\":120,\"output_tokens\":450}}\n\n");
        let expected_len = payload.len();
        let chunks: Vec<Result<Bytes, std::io::Error>> = payload
            .as_bytes()
            .chunks(64 * 1024)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();

        let collected = Arc::new(std::sync::Mutex::new(Vec::new()));
        let collector = SseUsageCollector::new(
            std::time::Instant::now(),
            Some(codex_stream_usage_event_filter),
            {
                let collected = collected.clone();
                move |events, _| {
                    *collected.lock().unwrap() = events;
                }
            },
        );

        let passthrough = create_logged_passthrough_stream(
            futures::stream::iter(chunks),
            "test",
            Some(collector),
            StreamingTimeoutConfig {
                first_byte_timeout: 0,
                idle_timeout: 0,
                ..Default::default()
            },
            None,
        );
        futures::pin_mut!(passthrough);
        let mut client_bytes = 0usize;
        while let Some(chunk) = passthrough.next().await {
            client_bytes += chunk.expect("client passthrough chunk").len();
        }

        assert_eq!(client_bytes, expected_len);
        let events = collected.lock().unwrap().clone();
        assert_eq!(events.len(), 1);
        let usage = TokenUsage::from_codex_stream_events_auto(&events).expect("usage parsed");
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 450);
    }
}
