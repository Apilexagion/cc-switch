//! Codex 原生 Responses 的会话身份轮换。只改身份元数据，不遍历消息或工具历史。

use http::{HeaderMap, HeaderValue};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const CAPACITY: usize = 4096;
const IDLE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const TURN_STATE_CAPACITY: usize = 8;
const STATE_TTL: Duration = Duration::from_secs(60 * 60);
const METADATA_DEPTH: usize = 8;
const METADATA_BYTES: usize = 16 * 1024;
const ENCODED_TURN_METADATA: &str = "x-codex-turn-metadata";
const SESSION_HEADERS: &[&str] = &[
    "session_id",
    "session-id",
    "x-session-id",
    "thread-id",
    "thread_id",
];
const SESSION_FIELDS: &[&str] = &["session_id", "session-id", "thread_id", "thread-id"];
const WINDOW_FIELDS: &[&str] = &["window_id", "window-id", "x-codex-window-id"];
const STATE_FIELDS: &[&str] = &["turn_state", "turn-state", "x-codex-turn-state"];
const TURN_FIELDS: &[&str] = &["turn_id", "turn-id"];
const TURN_HEADERS: &[&str] = &["turn_id", "turn-id", "x-codex-turn-id"];

#[derive(Default)]
struct Store {
    entries: HashMap<String, Arc<Entry>>,
}

struct Entry {
    state: Mutex<EntryState>,
}

struct EntryState {
    generation: u64,
    identity: Option<String>,
    last_used: Instant,
    // 只缓存响应头，不保存请求正文；回合和身份代都是缓存边界。
    turns: BTreeMap<String, (HeaderValue, Instant)>,
}

#[derive(Clone)]
pub(crate) struct Snapshot {
    generation: u64,
    identity: Option<String>,
}

impl Snapshot {
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

#[derive(Clone)]
pub(crate) struct Lease {
    entry: Arc<Entry>,
    key_hash: String,
    aliases: BTreeSet<String>,
    original_headers: HeaderMap,
    turn_metadata: Option<Value>,
    turn: Option<String>,
}

#[derive(Default)]
pub(crate) struct ApplyReport {
    headers: usize,
    fields: usize,
    states_removed: usize,
    state_reused: bool,
    identity_hashes: BTreeSet<String>,
    identity_values: usize,
}

fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))[..16].to_string()
}

fn text(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 4096)
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)?
        .to_str()
        .ok()
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 4096)
}

fn decode_turn_metadata(raw: &str) -> Option<Value> {
    if raw.len() > METADATA_BYTES {
        return None;
    }
    serde_json::from_str::<Value>(raw)
        .ok()
        .filter(Value::is_object)
}

/// Codex 0.145 还会把回合元数据编码为 client_metadata 中的 JSON 字符串。
/// 所有身份读取和审计共享同一容器边界，绝不进入 input/tools/content/arguments。
fn visit_metadata(value: &Value, depth: usize, visitor: &mut impl FnMut(&Value) -> bool) -> bool {
    if !visitor(value) {
        return false;
    }
    for field in ["metadata", "client_metadata"] {
        if let Some(child) = value.get(field).filter(|v| v.is_object()) {
            if depth >= METADATA_DEPTH || !visit_metadata(child, depth + 1, visitor) {
                return false;
            }
        }
    }
    if let Some(encoded) = value.get(ENCODED_TURN_METADATA) {
        let Some(metadata) = encoded.as_str().and_then(decode_turn_metadata) else {
            return false;
        };
        if depth >= METADATA_DEPTH || !visit_metadata(&metadata, depth + 1, visitor) {
            return false;
        }
    }
    true
}

fn metadata_depth_supported(value: &Value, depth: usize) -> bool {
    visit_metadata(value, depth, &mut |_| true)
}

fn container_id(value: &Value, fields: &[&str]) -> Option<String> {
    let mut identity = None;
    visit_metadata(value, 0, &mut |container| {
        if identity.is_none() {
            identity = fields
                .iter()
                .find_map(|field| container.get(*field).and_then(text))
                .map(ToString::to_string);
        }
        true
    });
    identity
}

fn collect_aliases(value: &Value, aliases: &mut BTreeSet<String>) {
    visit_metadata(value, 0, &mut |container| {
        for field in SESSION_FIELDS {
            if let Some(id) = container.get(*field).and_then(text) {
                aliases.insert(id.to_string());
            }
        }
        true
    });
}

fn collect_turns(value: &Value, turns: &mut BTreeSet<String>, depth: usize) -> bool {
    visit_metadata(value, depth, &mut |container| {
        for field in TURN_FIELDS {
            if let Some(value) = container.get(*field) {
                let Some(turn) = text(value) else {
                    return false;
                };
                turns.insert(turn.to_string());
            }
        }
        ["metadata", "client_metadata"].iter().all(|field| {
            container
                .get(*field)
                .is_none_or(|child| child.is_null() || child.is_object())
        })
    })
}

fn consistent_turn(
    headers: &HeaderMap,
    body: &Value,
    restored_metadata: Option<&Value>,
) -> Option<String> {
    let mut turns = BTreeSet::new();
    if !collect_turns(body, &mut turns, 0) {
        return None;
    }
    for name in TURN_HEADERS {
        // 重复的同名头也必须一致，不能只选第一项。
        for value in headers.get_all(*name).iter() {
            let turn = value.to_str().ok()?.trim();
            if turn.is_empty() || turn.len() > 4096 {
                return None;
            }
            turns.insert(turn.to_string());
        }
    }
    let mut has_metadata = false;
    for value in headers.get_all("x-codex-turn-metadata").iter() {
        has_metadata = true;
        let raw = value.to_str().ok()?;
        let metadata = decode_turn_metadata(raw)?;
        if !collect_turns(&metadata, &mut turns, 0) {
            return None;
        }
    }
    // Stack 指纹过滤掉的原始元数据会在 apply 中恢复，因此也属于真正出站的来源。
    if !has_metadata {
        if let Some(metadata) = restored_metadata {
            if !collect_turns(metadata, &mut turns, 0) {
                return None;
            }
        }
    }
    if turns.len() != 1 {
        return None;
    }
    let turn = turns.into_iter().next()?;
    Some(format!("{:x}", Sha256::digest(turn.as_bytes())))
}

fn audit_container(value: &Value, report: &mut ApplyReport, depth: usize) {
    visit_metadata(value, depth, &mut |container| {
        for field in SESSION_FIELDS {
            if let Some(id) = container.get(*field).and_then(text) {
                report.identity_values += 1;
                report.identity_hashes.insert(hash(id));
            }
        }
        true
    });
}

fn audit(headers: &HeaderMap, body: &Value, report: &mut ApplyReport) {
    for name in SESSION_HEADERS {
        for value in headers.get_all(*name).iter() {
            if let Ok(id) = value.to_str() {
                report.identity_values += 1;
                report.identity_hashes.insert(hash(id));
            }
        }
    }
    audit_container(body, report, 0);
    if let Some(metadata) = headers
        .get("x-codex-turn-metadata")
        .and_then(|v| v.to_str().ok())
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
    {
        audit_container(&metadata, report, 0);
    }
}

static STORE: OnceLock<Mutex<Store>> = OnceLock::new();

/// 没有明确客户端会话标识、元数据格式未知或所有容量槽都在使用时安全跳过。
pub(crate) fn acquire(provider_id: &str, headers: &HeaderMap, body: &Value) -> Option<Lease> {
    acquire_from(
        STORE.get_or_init(|| Mutex::new(Store::default())),
        provider_id,
        headers,
        body,
    )
}

fn acquire_from(
    store: &Mutex<Store>,
    provider_id: &str,
    headers: &HeaderMap,
    body: &Value,
) -> Option<Lease> {
    acquire_from_at(store, provider_id, headers, body, Instant::now())
}

fn acquire_from_at(
    store: &Mutex<Store>,
    provider_id: &str,
    headers: &HeaderMap,
    body: &Value,
    now: Instant,
) -> Option<Lease> {
    if !metadata_depth_supported(body, 0) {
        return None;
    }
    let turn_metadata = match headers.get("x-codex-turn-metadata") {
        Some(value) => {
            let raw = value.to_str().ok()?;
            let value = decode_turn_metadata(raw)?;
            if !metadata_depth_supported(&value, 0) {
                return None;
            }
            Some(value)
        }
        None => None,
    };
    let thread = header_text(headers, "thread-id")
        .or_else(|| header_text(headers, "thread_id"))
        .map(ToString::to_string)
        .or_else(|| container_id(body, &["thread_id", "thread-id"]))
        .or_else(|| {
            turn_metadata
                .as_ref()
                .and_then(|v| container_id(v, &["thread_id", "thread-id"]))
        });
    let session = SESSION_HEADERS
        .iter()
        .find_map(|name| header_text(headers, name))
        .map(ToString::to_string)
        .or_else(|| container_id(body, SESSION_FIELDS))
        .or_else(|| {
            turn_metadata
                .as_ref()
                .and_then(|v| container_id(v, SESSION_FIELDS))
        });
    // thread 是聊天边界；一个客户端进程共享 session_id 时，不能把多个线程合并。
    let identity = thread.as_ref().or(session.as_ref())?;
    let key_material = serde_json::to_string(&(provider_id, thread.is_some(), identity)).ok()?;
    let key = format!("{:x}", Sha256::digest(key_material.as_bytes()));
    let mut aliases = BTreeSet::new();
    let mut original_headers = HeaderMap::new();
    for name in SESSION_HEADERS
        .iter()
        .copied()
        .chain(["x-codex-window-id", "x-codex-turn-metadata"])
    {
        if let Some(value) = headers.get(name) {
            original_headers.insert(
                http::HeaderName::from_bytes(name.as_bytes()).ok()?,
                value.clone(),
            );
        }
        if SESSION_HEADERS.contains(&name) {
            if let Some(id) = header_text(headers, name) {
                aliases.insert(id.to_string());
            }
        }
    }
    collect_aliases(body, &mut aliases);
    if let Some(metadata) = &turn_metadata {
        collect_aliases(metadata, &mut aliases);
    }
    let turn = consistent_turn(headers, body, None);
    let mut store = store.lock().unwrap_or_else(|e| e.into_inner());
    // 活跃 Lease 固定该条目，避免并发请求遇到驱逐后产生两条同名映射。
    store.entries.retain(|_, entry| {
        Arc::strong_count(entry) > 1
            || now.duration_since(
                entry
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .last_used,
            ) < IDLE_TTL
    });
    if !store.entries.contains_key(&key) && store.entries.len() >= CAPACITY {
        let oldest = store
            .entries
            .iter()
            .filter(|(_, e)| Arc::strong_count(e) == 1)
            .min_by_key(|(_, e)| e.state.lock().unwrap_or_else(|e| e.into_inner()).last_used)
            .map(|(key, _)| key.clone());
        match oldest {
            Some(key) => {
                store.entries.remove(&key);
            }
            None => return None,
        }
    }
    let entry = store
        .entries
        .entry(key.clone())
        .or_insert_with(|| {
            Arc::new(Entry {
                state: Mutex::new(EntryState {
                    generation: 0,
                    identity: None,
                    last_used: now,
                    turns: BTreeMap::new(),
                }),
            })
        })
        .clone();
    entry
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .last_used = now;
    Some(Lease {
        entry,
        key_hash: key[..16].to_string(),
        aliases,
        original_headers,
        turn_metadata,
        turn,
    })
}

impl Lease {
    /// 在全部中间件与本地头覆盖完成后重定当前回合。冲突或未知时不缓存/复用路由状态。
    pub(crate) fn for_outbound(&self, headers: &HeaderMap, body: &Value) -> Self {
        let mut lease = self.clone();
        lease.turn = consistent_turn(headers, body, self.turn_metadata.as_ref());
        lease
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        let mut state = self.entry.state.lock().unwrap_or_else(|e| e.into_inner());
        state.last_used = Instant::now();
        Snapshot {
            generation: state.generation,
            identity: state.identity.clone(),
        }
    }

    /// 只允许失败请求观察到的那一代推进；并发失败共享同一新代。
    pub(crate) fn rotate(&self, failed: &Snapshot) -> Snapshot {
        let mut state = self.entry.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.generation == failed.generation {
            state.generation = state.generation.saturating_add(1);
            state.identity = Some(uuid::Uuid::new_v4().to_string());
            state.turns.clear();
        }
        state.last_used = Instant::now();
        Snapshot {
            generation: state.generation,
            identity: state.identity.clone(),
        }
    }

    pub(crate) fn apply(
        &self,
        snapshot: &Snapshot,
        headers: &mut HeaderMap,
        body: &mut Value,
    ) -> ApplyReport {
        let mut report = ApplyReport::default();
        let Some(identity) = snapshot.identity.as_deref() else {
            audit(headers, body, &mut report);
            return report;
        };
        // 最终正文也可能被本地覆盖器改成未知格式；整体跳过，不能只换头而留下旧串。
        if !metadata_depth_supported(body, 0) {
            audit(headers, body, &mut report);
            return report;
        }
        let value = HeaderValue::from_str(identity).expect("UUID 是合法请求头值");
        // 始终补齐三种协议常用拼写，避免 Stack 的指纹过滤或本地覆盖留下旧值。
        for name in ["session_id", "session-id", "thread-id"] {
            headers.insert(name, value.clone());
            report.headers += 1;
        }
        for name in SESSION_HEADERS {
            if headers.contains_key(*name) || self.original_headers.contains_key(*name) {
                headers.insert(
                    http::HeaderName::from_bytes(name.as_bytes()).expect("静态头名"),
                    value.clone(),
                );
            }
        }
        let window = headers
            .get("x-codex-window-id")
            .or_else(|| self.original_headers.get("x-codex-window-id"))
            .and_then(|v| v.to_str().ok())
            .map(|v| self.window(identity, v));
        if let Some(window) = window {
            headers.insert(
                "x-codex-window-id",
                HeaderValue::from_str(&window).expect("UUID 窗口值"),
            );
            report.headers += 1;
        }
        let mut metadata = headers
            .get("x-codex-turn-metadata")
            .and_then(|v| v.to_str().ok())
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .filter(Value::is_object)
            .or_else(|| self.turn_metadata.clone());
        if let Some(metadata) = metadata.as_mut() {
            self.rewrite_container(metadata, identity, &mut report);
            if let Ok(value) = HeaderValue::from_str(&metadata.to_string()) {
                headers.insert("x-codex-turn-metadata", value);
            }
        }
        self.rewrite_container(body, identity, &mut report);
        if body
            .get("prompt_cache_key")
            .and_then(Value::as_str)
            .is_some_and(|key| self.aliases.contains(key))
        {
            body["prompt_cache_key"] = Value::String(identity.to_string());
            report.fields += 1;
        }
        report.states_removed += usize::from(headers.remove("x-codex-turn-state").is_some());
        if let Some(turn) = &self.turn {
            let mut state = self.entry.state.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            state
                .turns
                .retain(|_, (_, at)| now.duration_since(*at) < STATE_TTL);
            if state.generation == snapshot.generation {
                if let Some((state, _)) = state.turns.get(turn) {
                    headers.insert("x-codex-turn-state", state.clone());
                    report.state_reused = true;
                }
            }
        }
        audit(headers, body, &mut report);
        report
    }

    fn window(&self, identity: &str, old: &str) -> String {
        for alias in &self.aliases {
            if let Some(suffix) = old
                .strip_prefix(alias)
                .filter(|s| s.starts_with(':') && s[1..].chars().all(|c| c.is_ascii_digit()))
            {
                return format!("{identity}{suffix}");
            }
        }
        // 已轮换请求的二次应用仍保留窗口序号；UUID 前缀由本模块产生。
        if let Some((prefix, suffix)) = old.rsplit_once(':') {
            if uuid::Uuid::parse_str(prefix).is_ok()
                && !suffix.is_empty()
                && suffix.chars().all(|c| c.is_ascii_digit())
            {
                return format!("{identity}:{suffix}");
            }
        }
        format!("{identity}:0")
    }

    fn rewrite_container(&self, value: &mut Value, identity: &str, report: &mut ApplyReport) {
        self.rewrite_container_at(value, identity, report, 0);
    }

    fn rewrite_container_at(
        &self,
        value: &mut Value,
        identity: &str,
        report: &mut ApplyReport,
        depth: usize,
    ) {
        let Some(map) = value.as_object_mut() else {
            return;
        };
        for name in SESSION_FIELDS {
            if map.contains_key(*name) {
                map.insert((*name).to_string(), Value::String(identity.to_string()));
                report.fields += 1;
            }
        }
        for name in WINDOW_FIELDS {
            if let Some(old) = map.get(*name).and_then(Value::as_str) {
                let window = self.window(identity, old);
                map.insert((*name).to_string(), Value::String(window));
                report.fields += 1;
            }
        }
        for name in STATE_FIELDS {
            report.states_removed += usize::from(map.remove(*name).is_some());
        }
        // 递归只跨元数据边界，消息/工具/引用/密文不属于元数据容器。
        if depth < METADATA_DEPTH {
            for name in ["metadata", "client_metadata"] {
                if let Some(child) = map.get_mut(name) {
                    self.rewrite_container_at(child, identity, report, depth + 1);
                }
            }
            if let Some(mut metadata) = map
                .get(ENCODED_TURN_METADATA)
                .and_then(Value::as_str)
                .and_then(decode_turn_metadata)
            {
                self.rewrite_container_at(&mut metadata, identity, report, depth + 1);
                map.insert(
                    ENCODED_TURN_METADATA.to_string(),
                    Value::String(metadata.to_string()),
                );
            }
        }
    }

    /// 仅成功响应调用。未知回合不缓存；迟到的旧代响应不能覆盖新身份状态。
    pub(crate) fn observe(&self, snapshot: &Snapshot, headers: &HeaderMap) {
        if snapshot.generation == 0 {
            return;
        }
        let (Some(turn), Some(value)) = (&self.turn, headers.get("x-codex-turn-state")) else {
            return;
        };
        // 路由凭据仅进内存，限制每条大小与每身份数量。
        if value.as_bytes().len() > 4096 {
            return;
        }
        let mut state = self.entry.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.generation != snapshot.generation {
            return;
        }
        if !state.turns.contains_key(turn) && state.turns.len() >= TURN_STATE_CAPACITY {
            if let Some(oldest) = state
                .turns
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(key, _)| key.clone())
            {
                state.turns.remove(&oldest);
            }
        }
        state
            .turns
            .insert(turn.clone(), (value.clone(), Instant::now()));
    }

    /// 由供应商诊断开关控制调用；不输出原标识、正文或路由凭据。
    pub(crate) fn log(
        &self,
        snapshot: &Snapshot,
        report: &ApplyReport,
        phase: &'static str,
        status: Option<u16>,
    ) {
        log::info!("[CodexSessionIdentity] phase={phase} key_hash={} generation={} identity_hash={} identity_hash_count={} identity_values={} headers={} fields={} states_removed={} state_reused={} status={}",
            self.key_hash,
            snapshot.generation,
            snapshot.identity.as_deref().map(hash).unwrap_or_else(|| "original".to_string()),
            report.identity_hashes.len(),
            report.identity_values,
            report.headers,
            report.fields,
            report.states_removed,
            report.state_reused,
            status.unwrap_or_default(),
        );
    }
}

/// HTTP 400 之外不触发。其他明确错误码优先否决，不能用提示文本覆盖其含义。
pub(crate) fn is_turn_limit(status: u16, body: &str) -> bool {
    if status != 400 {
        return false;
    }
    let parsed = serde_json::from_str::<Value>(body).ok();
    let error = parsed
        .as_ref()
        .and_then(|v| v.get("error"))
        .or(parsed.as_ref());
    if let Some(code) = error
        .and_then(|v| v.get("code"))
        .filter(|code| !code.is_null() && code.as_str() != Some(""))
    {
        return code.as_str() == Some("session_turn_limit_exceeded");
    }
    let message = error
        .and_then(|v| v.get("message"))
        .and_then(Value::as_str)
        .or_else(|| error.and_then(Value::as_str))
        .unwrap_or(body)
        .trim();
    static KNOWN: OnceLock<regex::Regex> = OnceLock::new();
    KNOWN.get_or_init(|| regex::Regex::new(concat!(
        r"^(?:当前会话模型已回复\s*[1-9][0-9]*\s*轮[，,]\s*达到上限[。.]\s*Codex\s*请输入\s*/compact\s*压缩上下文后继续[，,]\s*或输入\s*/new\s*新开会话[。.]?",
        r"|This session has reached the limit of [1-9][0-9]* model turns\. Please use /compact to compact the context and continue, or /new to start a new session\.)$"
    )).expect("固定提示正则合法")).is_match(message)
}

#[cfg(test)]
mod tests;
