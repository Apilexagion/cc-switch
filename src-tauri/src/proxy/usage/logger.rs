//! Usage Logger - 记录 API 请求使用情况

use super::calculator::{CostBreakdown, CostCalculator, ModelPricing};
use super::parser::TokenUsage;
use crate::app_config::AppType;
use crate::database::{Database, PRICING_SOURCE_REQUEST, PRICING_SOURCE_RESPONSE};
use crate::error::AppError;
use crate::services::sql_helpers::{
    is_cache_inclusive_app, INPUT_TOKEN_SEMANTICS_FRESH, INPUT_TOKEN_SEMANTICS_LEGACY,
    INPUT_TOKEN_SEMANTICS_TOTAL,
};
use crate::services::usage_stats::{find_model_pricing_row, is_placeholder_pricing_model};
use rusqlite::OptionalExtension;
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};

/// `proxy_request_logs.error_message` 的字符数上限。上游错误体（中转站整页
/// HTML、长 JSON）原样入库时，高频报错能把库撑到数 GB（#6706）。按字符而非
/// 字节截断，不会切坏 UTF-8；取值与 Pi 会话导入的 errorMessage 上限一致。
pub(crate) const MAX_ERROR_MESSAGE_CHARS: usize = 4096;

/// 保留错误信息的前 `MAX_ERROR_MESSAGE_CHARS` 个字符：状态行、错误码和
/// 错误描述都在开头，定位问题足够。
fn bound_error_message(message: &str) -> &str {
    match message.char_indices().nth(MAX_ERROR_MESSAGE_CHARS) {
        Some((byte_index, _)) => &message[..byte_index],
        None => message,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct UsageSemantic {
    app_type: String,
    provider_id: String,
    model: String,
    input_token_semantics: i64,
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,
    cache_creation_tokens: u32,
    status_code: u16,
}

impl UsageSemantic {
    /// Gateway token quotas count fresh input, output and cache writes, but not
    /// cache reads. Match `fresh_input_sql` for both current and historical rows;
    /// normalized FRESH rows must not be deducted again just because of app type.
    fn gateway_billable_tokens(&self) -> i64 {
        let input = i64::from(self.input_tokens);
        let read = i64::from(self.cache_read_tokens);
        let write = i64::from(self.cache_creation_tokens);
        let fresh = if self.input_token_semantics == INPUT_TOKEN_SEMANTICS_FRESH
            || !is_cache_inclusive_app(&self.app_type)
        {
            input
        } else if self.input_token_semantics == INPUT_TOKEN_SEMANTICS_TOTAL && input >= read + write
        {
            input - read - write
        } else if self.input_token_semantics == INPUT_TOKEN_SEMANTICS_LEGACY && input >= read {
            input - read
        } else {
            // Preserve the existing normalization fallback for malformed or
            // future semantics instead of subtracting untrusted counters.
            input
        };
        fresh + i64::from(self.output_tokens) + write
    }

    fn from_log(log: &RequestLog, input_token_semantics: i64) -> Self {
        Self {
            app_type: log.app_type.clone(),
            provider_id: log.provider_id.clone(),
            model: log.model.clone(),
            input_token_semantics,
            input_tokens: log.usage.input_tokens,
            output_tokens: log.usage.output_tokens,
            cache_read_tokens: log.usage.cache_read_tokens,
            cache_creation_tokens: log.usage.cache_creation_tokens,
            status_code: log.status_code,
        }
    }

    fn sha256(&self) -> String {
        let encoded = serde_json::to_vec(&(
            &self.app_type,
            &self.provider_id,
            &self.model,
            self.input_token_semantics,
            self.input_tokens,
            self.output_tokens,
            self.cache_read_tokens,
            self.cache_creation_tokens,
            self.status_code,
        ))
        .expect("usage semantic tuple is serializable");
        Sha256::digest(encoded)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// 请求日志
#[derive(Debug, Clone)]
pub struct RequestLog {
    pub request_id: String,
    pub provider_id: String,
    pub app_type: String,
    pub model: String,
    pub request_model: String,
    /// 写入时实际用于计价的模型名（pricing_model_source 解析后的结果）。
    /// 落库供回填使用：缺价行补价后必须按写入时的基准重算，而不是
    /// 用 model/request_model 猜——路由接管下三者可能各不相同。
    /// 错误行（未计价）为空字符串。
    pub pricing_model: String,
    pub usage: TokenUsage,
    pub cost: Option<CostBreakdown>,
    pub latency_ms: u64,
    pub first_token_ms: Option<u64>,
    pub status_code: u16,
    pub error_message: Option<String>,
    pub session_id: Option<String>,
    /// 供应商类型 (claude, claude_auth, codex, gemini, gemini_cli, openrouter)
    pub provider_type: Option<String>,
    /// 是否为流式请求
    pub is_streaming: bool,
    /// 成本倍数
    pub cost_multiplier: String,
    /// Fork：发起本次请求的局域网网关密钥 id（None = 非网关密钥流量）。用于 per-key 配额统计。
    pub gateway_key_id: Option<String>,
}

/// 使用量记录器
pub struct UsageLogger<'a> {
    db: &'a Database,
    diagnostic_logging: bool,
}

impl<'a> UsageLogger<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self {
            db,
            diagnostic_logging: true,
        }
    }

    /// Disabling diagnostics still accounts for attributed gateway usage, but
    /// never stores the request's model/session/error metadata in request logs.
    pub fn with_diagnostic_logging(mut self, enabled: bool) -> Self {
        self.diagnostic_logging = enabled;
        self
    }

    /// 记录成功的请求
    pub fn log_request(&self, log: &RequestLog) -> Result<(), AppError> {
        let conn = crate::database::lock_conn!(self.db.conn);

        let (input_cost, output_cost, cache_read_cost, cache_creation_cost, total_cost) =
            if let Some(cost) = &log.cost {
                (
                    cost.input_cost.to_string(),
                    cost.output_cost.to_string(),
                    cost.cache_read_cost.to_string(),
                    cost.cache_creation_cost.to_string(),
                    cost.total_cost.to_string(),
                )
            } else {
                (
                    "0".to_string(),
                    "0".to_string(),
                    "0".to_string(),
                    "0".to_string(),
                    "0".to_string(),
                )
            };

        let created_at = chrono::Utc::now().timestamp();
        let input_token_semantics =
            if crate::services::sql_helpers::is_cache_inclusive_app(log.app_type.as_str()) {
                INPUT_TOKEN_SEMANTICS_TOTAL
            } else {
                INPUT_TOKEN_SEMANTICS_FRESH
            };
        let semantic = UsageSemantic::from_log(log, input_token_semantics);
        if let Some(key_id) = log.gateway_key_id.as_deref() {
            Self::record_gateway_usage_on_conn(
                &conn,
                &log.request_id,
                key_id,
                &semantic,
                &total_cost,
                created_at,
            )?;
        }
        if !self.diagnostic_logging {
            return Ok(());
        }
        let existing = Self::load_existing_semantic(&conn, &log.request_id)?;

        let (request_id, replace_session_log, collision) = match existing {
            None => (log.request_id.clone(), false, false),
            Some((data_source, _existing_semantic))
                if data_source.as_deref() == Some("session_log") =>
            {
                (log.request_id.clone(), true, false)
            }
            Some((data_source, existing_semantic))
                if data_source.as_deref().unwrap_or("proxy") == "proxy"
                    && existing_semantic == semantic =>
            {
                return Ok(());
            }
            Some(_) => {
                let fallback = format!("{}:collision:{}", log.request_id, semantic.sha256());
                if let Some((data_source, existing_semantic)) =
                    Self::load_existing_semantic(&conn, &fallback)?
                {
                    if data_source.as_deref().unwrap_or("proxy") == "proxy"
                        && existing_semantic == semantic
                    {
                        return Ok(());
                    }
                    return Err(AppError::Database(format!(
                        "usage collision fallback 主键发生 SHA-256 冲突: {fallback}"
                    )));
                }
                (fallback, false, true)
            }
        };

        let insert_verb = if replace_session_log {
            "INSERT OR REPLACE"
        } else {
            "INSERT OR IGNORE"
        };
        let error_message = log.error_message.as_deref().map(bound_error_message);
        let sql = format!(
            "{insert_verb} INTO proxy_request_logs (
                request_id, provider_id, app_type, model, request_model, pricing_model,
                input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                input_token_semantics,
                input_cost_usd, output_cost_usd, cache_read_cost_usd, cache_creation_cost_usd, total_cost_usd,
                latency_ms, first_token_ms, status_code, error_message, session_id,
                provider_type, is_streaming, cost_multiplier, created_at, gateway_key_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)"
        );
        let affected_rows = conn
            .execute(
                &sql,
                rusqlite::params![
                    request_id,
                    log.provider_id,
                    log.app_type,
                    log.model,
                    log.request_model,
                    log.pricing_model,
                    log.usage.input_tokens,
                    log.usage.output_tokens,
                    log.usage.cache_read_tokens,
                    log.usage.cache_creation_tokens,
                    input_token_semantics,
                    input_cost,
                    output_cost,
                    cache_read_cost,
                    cache_creation_cost,
                    total_cost,
                    log.latency_ms as i64,
                    log.first_token_ms.map(|v| v as i64),
                    log.status_code as i64,
                    error_message,
                    log.session_id,
                    log.provider_type,
                    log.is_streaming as i64,
                    log.cost_multiplier,
                    created_at,
                    log.gateway_key_id,
                ],
            )
            .map_err(|e| AppError::Database(format!("记录请求日志失败: {e}")))?;

        if affected_rows > 0 {
            if collision {
                log::warn!(
                    "usage request_id collision: primary={}, fallback={request_id}",
                    log.request_id
                );
            }
            crate::usage_events::notify_log_recorded();
        }

        Ok(())
    }

    fn record_gateway_usage_on_conn(
        conn: &rusqlite::Connection,
        request_id: &str,
        key_id: &str,
        semantic: &UsageSemantic,
        total_cost_usd: &str,
        created_at: i64,
    ) -> Result<(), AppError> {
        let semantic_hash = semantic.sha256();
        // Diagnostic collisions have deterministic suffixes. Normalize only
        // this exact suffix so importing those rows and replaying the original
        // upstream response produce the same durable accounting identity.
        let collision_suffix = format!(":collision:{semantic_hash}");
        let base_request_id = request_id
            .strip_suffix(&collision_suffix)
            .unwrap_or(request_id);
        let identity = serde_json::to_vec(&(key_id, base_request_id, semantic_hash))
            .expect("gateway usage identity is serializable");
        let usage_id = Sha256::digest(identity)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let tokens = semantic.gateway_billable_tokens();
        conn.execute(
            "INSERT INTO forkdb.gateway_usage_ledger
             (usage_id, gateway_key_id, billable_tokens, total_cost_usd, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(usage_id) DO UPDATE SET billable_tokens = excluded.billable_tokens
             WHERE billable_tokens != excluded.billable_tokens",
            rusqlite::params![usage_id, key_id, tokens, total_cost_usd, created_at],
        )
        .map_err(|e| AppError::Database(format!("记录网关配额用量失败: {e}")))?;
        Ok(())
    }

    /// Called during schema initialization, before diagnostic-log pruning.
    /// The same identity as live accounting corrects old token calculations in
    /// place, including collision rows. Only rows with retained source details
    /// are touched: pruned/diagnostics-disabled ledger entries stay intact.
    /// Original timestamps and recorded costs are never rewritten on conflict.
    pub(crate) fn backfill_gateway_usage_on_conn(
        conn: &rusqlite::Connection,
    ) -> Result<(), AppError> {
        let mut statement = conn.prepare(
            "SELECT request_id, gateway_key_id, app_type, provider_id, model,
                    input_token_semantics, input_tokens, output_tokens,
                    cache_read_tokens, cache_creation_tokens, status_code,
                    total_cost_usd, created_at
             FROM proxy_request_logs WHERE gateway_key_id IS NOT NULL",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let semantic = UsageSemantic {
                app_type: row.get(2)?,
                provider_id: row.get(3)?,
                model: row.get(4)?,
                input_token_semantics: row.get(5)?,
                input_tokens: row.get::<_, i64>(6)? as u32,
                output_tokens: row.get::<_, i64>(7)? as u32,
                cache_read_tokens: row.get::<_, i64>(8)? as u32,
                cache_creation_tokens: row.get::<_, i64>(9)? as u32,
                status_code: row.get::<_, i64>(10)? as u16,
            };
            Self::record_gateway_usage_on_conn(
                conn,
                &row.get::<_, String>(0)?,
                &row.get::<_, String>(1)?,
                &semantic,
                &row.get::<_, String>(11)?,
                row.get(12)?,
            )?;
        }
        Ok(())
    }

    fn load_existing_semantic(
        conn: &rusqlite::Connection,
        request_id: &str,
    ) -> Result<Option<(Option<String>, UsageSemantic)>, AppError> {
        conn.query_row(
            "SELECT data_source, app_type, provider_id, model, input_token_semantics,
                    input_tokens, output_tokens, cache_read_tokens,
                    cache_creation_tokens, status_code
             FROM proxy_request_logs WHERE request_id = ?1",
            [request_id],
            |row| {
                Ok((
                    row.get(0)?,
                    UsageSemantic {
                        app_type: row.get(1)?,
                        provider_id: row.get(2)?,
                        model: row.get(3)?,
                        input_token_semantics: row.get(4)?,
                        input_tokens: row.get::<_, i64>(5)? as u32,
                        output_tokens: row.get::<_, i64>(6)? as u32,
                        cache_read_tokens: row.get::<_, i64>(7)? as u32,
                        cache_creation_tokens: row.get::<_, i64>(8)? as u32,
                        status_code: row.get::<_, i64>(9)? as u16,
                    },
                ))
            },
        )
        .optional()
        .map_err(|error| AppError::Database(format!("查询 usage request_id 失败: {error}")))
    }

    /// 记录失败的请求（带请求上下文）
    ///
    /// `model` 是发往上游的模型名，`request_model` 是客户端请求里的原名（没映射时两者相同）。
    #[allow(clippy::too_many_arguments)]
    pub fn log_error_with_context(
        &self,
        request_id: String,
        provider_id: String,
        app_type: String,
        model: String,
        request_model: String,
        status_code: u16,
        error_message: String,
        latency_ms: u64,
        is_streaming: bool,
        session_id: Option<String>,
        provider_type: Option<String>,
    ) -> Result<(), AppError> {
        let log = RequestLog {
            request_id,
            provider_id,
            app_type,
            model,
            request_model,
            // 错误行未经过计价，留空（回填的 has_usage 闸门也不会碰全 0 行）
            pricing_model: String::new(),
            usage: TokenUsage::default(),
            cost: None,
            latency_ms,
            first_token_ms: None,
            status_code,
            error_message: Some(error_message),
            session_id,
            provider_type,
            is_streaming,
            cost_multiplier: "1.0".to_string(),
            gateway_key_id: None,
        };

        self.log_request(&log)
    }

    /// 获取模型定价
    pub fn get_model_pricing(&self, model_id: &str) -> Result<Option<ModelPricing>, AppError> {
        let conn = crate::database::lock_conn!(self.db.conn);
        let row = find_model_pricing_row(&conn, model_id)?;
        match row {
            Some((input, output, cache_read, cache_creation)) => {
                ModelPricing::from_strings(&input, &output, &cache_read, &cache_creation)
                    .map(Some)
                    .map_err(|e| AppError::Database(format!("解析定价数据失败: {e}")))
            }
            None => Ok(None),
        }
    }

    /// 获取计费模式来源（按请求模型 / 按返回模型），只读应用级全局配置
    pub async fn resolve_pricing_model_source(&self, app_type: &str) -> String {
        // Claude Desktop 网关没有独立的全局计费配置（proxy_config 的 CHECK 不含
        // claude-desktop），继承 claude 的设置。
        let default_app_type = match app_type.parse::<AppType>() {
            Ok(AppType::ClaudeDesktop) => AppType::Claude,
            Ok(app) => app,
            // 不认识的应用没有 proxy_config 行，和查不到行一样按返回模型计费。
            Err(_) => return PRICING_SOURCE_RESPONSE.to_string(),
        };
        match self.db.get_pricing_model_source(&default_app_type).await {
            Ok(value) if value == PRICING_SOURCE_RESPONSE || value == PRICING_SOURCE_REQUEST => {
                value
            }
            Ok(value) => {
                log::warn!("[USG-003] 计费模式无效 (app_type={app_type}): {value}");
                PRICING_SOURCE_RESPONSE.to_string()
            }
            Err(e) => {
                log::warn!("[USG-003] 获取计费模式失败 (app_type={app_type}): {e}");
                PRICING_SOURCE_RESPONSE.to_string()
            }
        }
    }

    /// 计算并记录请求
    #[allow(clippy::too_many_arguments)]
    pub fn log_with_calculation(
        &self,
        request_id: String,
        provider_id: String,
        multiplier: Decimal,
        app_type: String,
        model: String,
        request_model: String,
        pricing_model: String,
        usage: TokenUsage,
        latency_ms: u64,
        first_token_ms: Option<u64>,
        status_code: u16,
        session_id: Option<String>,
        provider_type: Option<String>,
        is_streaming: bool,
        gateway_key_id: Option<String>,
    ) -> Result<(), AppError> {
        let pricing = self.get_model_pricing(&pricing_model)?;

        let has_usage = usage.input_tokens > 0
            || usage.output_tokens > 0
            || usage.cache_read_tokens > 0
            || usage.cache_creation_tokens > 0;

        if pricing.is_none() && has_usage && !is_placeholder_pricing_model(&pricing_model) {
            log::warn!("[USG-002] 模型定价未找到，成本将记录为 0: {pricing_model}");
        }

        let cost =
            CostCalculator::try_calculate_for_app(&app_type, &usage, pricing.as_ref(), multiplier);

        let log = RequestLog {
            request_id,
            provider_id,
            app_type,
            model,
            request_model,
            pricing_model,
            usage,
            cost,
            latency_ms,
            first_token_ms,
            status_code,
            error_message: None,
            session_id,
            provider_type,
            is_streaming,
            cost_multiplier: multiplier.normalize().to_string(),
            gateway_key_id,
        };

        self.log_request(&log)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn billing_multiplier_applies_to_all_models_and_preserves_history() -> Result<(), AppError> {
        let db = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(db.conn);
            conn.execute_batch(r#"INSERT INTO providers (id, app_type, name, settings_config, meta)
              VALUES ('discount', 'claude', 'Discount', '{}', '{"billingMultiplier":"0.5","costMultiplier":"9"}'),
                     ('discount', 'codex', 'Other app', '{}', '{}');
              INSERT INTO model_pricing (model_id, display_name, input_cost_per_million, output_cost_per_million)
              VALUES ('billing-a', 'A', '2', '10'), ('billing-b', 'B', '4', '20');"#)?;
        }
        let logger = UsageLogger::new(&db);
        for (id, app, model) in [
            ("first", "claude", "billing-a"),
            ("second", "claude", "billing-b"),
            ("other", "codex", "billing-a"),
            ("missing", "claude", "billing-missing"),
        ] {
            logger.log_with_calculation(
                id.into(),
                "discount".into(),
                crate::provider::billing_multiplier(
                    &db.get_provider_by_id("discount", &app.parse::<AppType>().unwrap())?
                        .unwrap(),
                ),
                app.into(),
                model.into(),
                model.into(),
                model.into(),
                TokenUsage {
                    input_tokens: 1000000,
                    ..Default::default()
                },
                1,
                None,
                200,
                None,
                None,
                false,
                None,
            )?;
        }
        {
            let conn = crate::database::lock_conn!(db.conn);
            for (id, expected) in [
                ("first", "1.000000"),
                ("second", "2.000000"),
                ("other", "2.000000"),
            ] {
                let total: String = conn.query_row(
                    "SELECT total_cost_usd FROM proxy_request_logs WHERE request_id=?1",
                    [id],
                    |r| r.get(0),
                )?;
                assert_eq!(
                    total.parse::<Decimal>().unwrap(),
                    expected.parse::<Decimal>().unwrap()
                );
            }
            let stored: String = conn.query_row(
                "SELECT cost_multiplier FROM proxy_request_logs WHERE request_id='missing'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(stored, "0.5");
            conn.execute(
                "UPDATE providers SET meta='{}' WHERE id='discount' AND app_type='claude'",
                [],
            )?;
            let old: String = conn.query_row(
                "SELECT cost_multiplier FROM proxy_request_logs WHERE request_id='first'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(old, "0.5");
        }
        logger.log_with_calculation(
            "reset".into(),
            "discount".into(),
            Decimal::ONE,
            "claude".into(),
            "billing-a".into(),
            "billing-a".into(),
            "billing-a".into(),
            TokenUsage {
                input_tokens: 1000000,
                ..Default::default()
            },
            1,
            None,
            200,
            None,
            None,
            true,
            None,
        )?;
        let conn = crate::database::lock_conn!(db.conn);
        let stored: String = conn.query_row(
            "SELECT cost_multiplier FROM proxy_request_logs WHERE request_id='reset'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(stored, "1");
        Ok(())
    }

    #[test]
    fn billing_multiplier_validates_decimal_bounds() {
        for raw in ["-1", "NaN", "inf", "1001", "abc", "", "1e2"] {
            assert!(
                crate::provider::parse_billing_multiplier(raw).is_err(),
                "{raw}"
            );
        }
        for raw in ["0", "0.5", "1", "1.5", "1000"] {
            assert!(
                crate::provider::parse_billing_multiplier(raw).is_ok(),
                "{raw}"
            );
        }
    }

    fn request_log(request_id: &str, input_tokens: u32) -> RequestLog {
        RequestLog {
            request_id: request_id.to_string(),
            provider_id: "provider-1".to_string(),
            app_type: "codex".to_string(),
            model: "gpt-5.6".to_string(),
            request_model: "gpt-5.6".to_string(),
            pricing_model: "gpt-5.6".to_string(),
            usage: TokenUsage {
                input_tokens,
                output_tokens: 5,
                cache_read_tokens: 2,
                cache_creation_tokens: 0,
                cache_creation_1h_tokens: 0,
                model: None,
                message_id: Some("resp-1".to_string()),
            },
            cost: None,
            latency_ms: 10,
            first_token_ms: Some(2),
            status_code: 200,
            error_message: None,
            session_id: None,
            provider_type: Some("codex".to_string()),
            is_streaming: true,
            cost_multiplier: "1".to_string(),
            gateway_key_id: None,
        }
    }

    #[test]
    fn test_log_request() -> Result<(), AppError> {
        let db = Database::memory()?;

        // 插入测试定价
        {
            let conn = crate::database::lock_conn!(db.conn);
            conn.execute(
                "INSERT INTO model_pricing (model_id, display_name, input_cost_per_million, output_cost_per_million)
                 VALUES ('test-model', 'Test Model', '3.0', '15.0')",
                [],
            )
            .unwrap();
        }

        let logger = UsageLogger::new(&db);

        let usage = TokenUsage {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cache_creation_1h_tokens: 0,
            model: None,
            message_id: None,
        };

        logger.log_with_calculation(
            "req-123".to_string(),
            "provider-1".to_string(),
            Decimal::ONE,
            "claude".to_string(),
            "test-model".to_string(),
            "req-model".to_string(),
            "test-model".to_string(),
            usage,
            100,
            None,
            200,
            None,
            Some("claude".to_string()),
            false,
            None,
        )?;

        // 验证记录已插入
        let conn = crate::database::lock_conn!(db.conn);
        let (count, request_model): (i64, String) = conn
            .query_row(
                "SELECT COUNT(*), request_model FROM proxy_request_logs WHERE request_id = 'req-123'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(request_model, "req-model");
        Ok(())
    }

    #[test]
    fn identical_replay_writes_and_notifies_once() -> Result<(), AppError> {
        let db = Database::memory()?;
        let logger = UsageLogger::new(&db);
        crate::usage_events::take_test_notify_count();
        let log = request_log("stable-id", 10);

        logger.log_request(&log)?;
        logger.log_request(&log)?;

        let conn = crate::database::lock_conn!(db.conn);
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM proxy_request_logs WHERE request_id = 'stable-id'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 1);
        assert_eq!(crate::usage_events::take_test_notify_count(), 1);
        Ok(())
    }

    #[test]
    fn gateway_usage_survives_disabled_diagnostic_logging_without_request_log(
    ) -> Result<(), AppError> {
        let db = Database::memory()?;
        let mut log = request_log("gateway-only", 10);
        log.gateway_key_id = Some("key-1".to_string());

        UsageLogger::new(&db)
            .with_diagnostic_logging(false)
            .log_request(&log)?;

        let conn = crate::database::lock_conn!(db.conn);
        let diagnostic_rows: i64 = conn.query_row(
            "SELECT COUNT(*) FROM proxy_request_logs WHERE request_id = 'gateway-only'",
            [],
            |row| row.get(0),
        )?;
        let ledger_rows: i64 = conn.query_row(
            "SELECT COUNT(*) FROM forkdb.gateway_usage_ledger WHERE gateway_key_id = 'key-1'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(diagnostic_rows, 0);
        assert_eq!(ledger_rows, 1);
        Ok(())
    }

    #[test]
    fn gateway_quota_counts_equivalent_cached_requests_equally() -> Result<(), AppError> {
        let db = Database::memory()?;
        let logger = UsageLogger::new(&db).with_diagnostic_logging(false);
        for app in ["claude", "codex", "gemini", "grokbuild"] {
            let mut log = request_log(app, 700);
            log.app_type = app.to_string();
            log.gateway_key_id = Some(app.to_string());
            log.usage.input_tokens = if is_cache_inclusive_app(app) {
                1000
            } else {
                700
            };
            log.usage.output_tokens = 50;
            log.usage.cache_read_tokens = 200;
            log.usage.cache_creation_tokens = 100;
            logger.log_request(&log)?;
            logger.log_request(&log)?;
            assert_eq!(db.sum_gateway_key_usage(app, 0)?.0, 850, "{app}");
        }
        Ok(())
    }

    #[test]
    fn gateway_normalization_matches_sql_for_legacy_and_current_semantics() -> Result<(), AppError>
    {
        let db = Database::memory()?;
        let conn = crate::database::lock_conn!(db.conn);
        let fresh_input = crate::services::sql_helpers::fresh_input_sql("");
        let sql = format!(
            "SELECT ({fresh_input}) + output_tokens + cache_creation_tokens
             FROM (SELECT ?1 AS app_type, ?2 AS input_token_semantics,
                          ?3 AS input_tokens, ?4 AS output_tokens,
                          ?5 AS cache_read_tokens, ?6 AS cache_creation_tokens)"
        );
        for (app, semantics, input, expected) in [
            ("codex", INPUT_TOKEN_SEMANTICS_TOTAL, 1000, 850),
            ("codex", INPUT_TOKEN_SEMANTICS_LEGACY, 900, 850),
            ("codex", INPUT_TOKEN_SEMANTICS_FRESH, 700, 850),
            ("claude", INPUT_TOKEN_SEMANTICS_FRESH, 700, 850),
            ("gemini", INPUT_TOKEN_SEMANTICS_TOTAL, 1000, 850),
            ("grokbuild", INPUT_TOKEN_SEMANTICS_TOTAL, 1000, 850),
            // Malformed counters and unknown future semantics retain the SQL
            // fallback; no unsigned underflow or guessed cache deduction.
            ("codex", INPUT_TOKEN_SEMANTICS_TOTAL, 100, 250),
            ("codex", 99, 1000, 1150),
        ] {
            let semantic = UsageSemantic {
                app_type: app.to_string(),
                provider_id: "p".to_string(),
                model: "m".to_string(),
                input_token_semantics: semantics,
                input_tokens: input,
                output_tokens: 50,
                cache_read_tokens: 200,
                cache_creation_tokens: 100,
                status_code: 200,
            };
            let sql_tokens: i64 = conn.query_row(
                &sql,
                rusqlite::params![app, semantics, input, 50, 200, 100],
                |row| row.get(0),
            )?;
            assert_eq!(sql_tokens, expected, "{app}/{semantics}");
            assert_eq!(
                semantic.gateway_billable_tokens(),
                sql_tokens,
                "{app}/{semantics}"
            );
        }
        Ok(())
    }

    #[test]
    fn gateway_backfill_corrects_retained_rows_without_erasing_pruned_usage() -> Result<(), AppError>
    {
        let db = Database::memory()?;
        let mut log = request_log("retained", 1000);
        log.gateway_key_id = Some("key-1".to_string());
        log.usage.output_tokens = 50;
        log.usage.cache_read_tokens = 200;
        log.usage.cache_creation_tokens = 100;
        UsageLogger::new(&db).log_request(&log)?;

        let conn = crate::database::lock_conn!(db.conn);
        // Model the previous unnormalized ledger calculation. A source-less
        // row represents a request whose diagnostics were disabled or pruned.
        conn.execute(
            "UPDATE forkdb.gateway_usage_ledger
             SET billable_tokens = 1150, total_cost_usd = '4.75', created_at = 11",
            [],
        )?;
        conn.execute(
            "INSERT INTO forkdb.gateway_usage_ledger VALUES ('pruned', 'key-1', 123, '0.25', 12)",
            [],
        )?;
        for _ in 0..2 {
            Database::backfill_gateway_usage_ledger_on_conn(&conn)?;
        }
        let corrected: (i64, String, i64) = conn.query_row(
            "SELECT billable_tokens, total_cost_usd, created_at
             FROM forkdb.gateway_usage_ledger WHERE usage_id != 'pruned'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(corrected, (850, "4.75".to_string(), 11));
        let preserved: (i64, String, i64) = conn.query_row(
            "SELECT billable_tokens, total_cost_usd, created_at
             FROM forkdb.gateway_usage_ledger WHERE usage_id = 'pruned'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(preserved, (123, "0.25".to_string(), 12));
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM forkdb.gateway_usage_ledger",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 2);
        Ok(())
    }

    #[test]
    fn semantic_collision_uses_deterministic_idempotent_fallback() -> Result<(), AppError> {
        let db = Database::memory()?;
        let logger = UsageLogger::new(&db);
        crate::usage_events::take_test_notify_count();
        let first = request_log("shared-id", 10);
        let second = request_log("shared-id", 20);

        logger.log_request(&first)?;
        logger.log_request(&second)?;
        logger.log_request(&second)?;

        let conn = crate::database::lock_conn!(db.conn);
        let rows: Vec<(String, i64)> = conn
            .prepare(
                "SELECT request_id, input_tokens FROM proxy_request_logs ORDER BY input_tokens",
            )?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], ("shared-id".to_string(), 10));
        assert!(rows[1].0.starts_with("shared-id:collision:"));
        assert_eq!(rows[1].1, 20);
        assert_eq!(crate::usage_events::take_test_notify_count(), 2);
        Ok(())
    }

    #[test]
    fn only_session_log_primary_rows_may_be_replaced() -> Result<(), AppError> {
        let db = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(db.conn);
            for (request_id, data_source) in [
                ("session-primary", "session_log"),
                ("codex-primary", "codex_session"),
            ] {
                conn.execute(
                    "INSERT INTO proxy_request_logs (
                        request_id, provider_id, app_type, model, input_tokens,
                        output_tokens, cache_read_tokens, cache_creation_tokens,
                        latency_ms, status_code, created_at, data_source
                     ) VALUES (?1, '_session', 'claude', 'old', 1, 1, 0, 0, 0, 200, 1, ?2)",
                    rusqlite::params![request_id, data_source],
                )?;
            }
        }
        let logger = UsageLogger::new(&db);

        let mut session_replacement = request_log("session-primary", 10);
        session_replacement.app_type = "claude".to_string();
        logger.log_request(&session_replacement)?;
        logger.log_request(&request_log("codex-primary", 20))?;

        let conn = crate::database::lock_conn!(db.conn);
        let session_source: String = conn.query_row(
            "SELECT data_source FROM proxy_request_logs WHERE request_id = 'session-primary'",
            [],
            |row| row.get(0),
        )?;
        let codex_input: i64 = conn.query_row(
            "SELECT input_tokens FROM proxy_request_logs WHERE request_id = 'codex-primary'",
            [],
            |row| row.get(0),
        )?;
        let fallback_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM proxy_request_logs
             WHERE request_id LIKE 'codex-primary:collision:%'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(session_source, "proxy");
        assert_eq!(codex_input, 1);
        assert_eq!(fallback_count, 1);
        Ok(())
    }

    #[test]
    fn claude_desktop_proxy_replaces_matching_session_log_row() -> Result<(), AppError> {
        let db = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(db.conn);
            conn.execute(
                "INSERT INTO proxy_request_logs (
                    request_id, provider_id, app_type, model, input_tokens,
                    output_tokens, cache_read_tokens, cache_creation_tokens,
                    latency_ms, status_code, created_at, data_source
                 ) VALUES ('session:msg_desktop', '_session', 'claude',
                    'claude-sonnet-4-5', 10, 5, 2, 1, 0, 200, 1, 'session_log')",
                [],
            )?;
        }

        let usage = TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 2,
            cache_creation_tokens: 1,
            cache_creation_1h_tokens: 0,
            model: Some("claude-sonnet-4-5".to_string()),
            message_id: Some("msg_desktop".to_string()),
        };
        let request_id = usage.dedup_request_id(crate::proxy::usage::parser::dedup_scope_for_app(
            "claude-desktop",
            "desktop-provider",
        ));
        let mut proxy_log = request_log(&request_id, 10);
        proxy_log.provider_id = "desktop-provider".to_string();
        proxy_log.app_type = "claude-desktop".to_string();
        proxy_log.model = "claude-sonnet-4-5".to_string();
        proxy_log.request_model = "claude-sonnet-4-5".to_string();
        proxy_log.pricing_model = "claude-sonnet-4-5".to_string();
        proxy_log.usage = usage;

        UsageLogger::new(&db).log_request(&proxy_log)?;

        let conn = crate::database::lock_conn!(db.conn);
        let (count, source, app_type): (i64, String, String) = conn.query_row(
            "SELECT COUNT(*), data_source, app_type FROM proxy_request_logs
             WHERE request_id = 'session:msg_desktop'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(count, 1);
        assert_eq!(source, "proxy");
        assert_eq!(app_type, "claude-desktop");
        Ok(())
    }

    #[test]
    fn test_log_error() -> Result<(), AppError> {
        let db = Database::memory()?;
        let logger = UsageLogger::new(&db);

        logger.log_error_with_context(
            "req-error".to_string(),
            "provider-1".to_string(),
            "claude".to_string(),
            "unknown-model".to_string(),
            "unknown-model".to_string(),
            500,
            "Internal Server Error".to_string(),
            50,
            false,
            None,
            None,
        )?;

        // 验证错误记录已插入
        let conn = crate::database::lock_conn!(db.conn);
        let (status, error): (i64, Option<String>) = conn
            .query_row(
                "SELECT status_code, error_message FROM proxy_request_logs WHERE request_id = 'req-error'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, 500);
        assert_eq!(error, Some("Internal Server Error".to_string()));
        Ok(())
    }

    #[test]
    fn bound_error_message_keeps_char_boundaries() {
        assert_eq!(bound_error_message(""), "");
        assert_eq!(bound_error_message("short"), "short");

        let exact = "e".repeat(MAX_ERROR_MESSAGE_CHARS);
        assert_eq!(bound_error_message(&exact), exact);

        // 3 字节（汉）与 4 字节（emoji）字符：按字符计数，截断点落在字符边界
        for unit in ["汉", "🦀"] {
            let long = unit.repeat(MAX_ERROR_MESSAGE_CHARS + 1);
            let bounded = bound_error_message(&long);
            assert_eq!(bounded.chars().count(), MAX_ERROR_MESSAGE_CHARS);
            assert_eq!(bounded, unit.repeat(MAX_ERROR_MESSAGE_CHARS));
        }
    }

    #[test]
    fn log_error_bounds_oversized_error_message() -> Result<(), AppError> {
        let db = Database::memory()?;
        let logger = UsageLogger::new(&db);

        // #6706：中转站返回数 MB 的整页 HTML 报错，不能原样入库
        let prefix = "HTTP 502 Bad Gateway: <html><title>上游错误</title>";
        let body = format!("{prefix}{}", "🦀".repeat(MAX_ERROR_MESSAGE_CHARS * 100));
        logger.log_error_with_context(
            "req-huge-error".to_string(),
            "provider-1".to_string(),
            "claude".to_string(),
            "claude-sonnet-4-5".to_string(),
            "claude-sonnet-4-5".to_string(),
            502,
            body,
            50,
            false,
            None,
            None,
        )?;

        let conn = crate::database::lock_conn!(db.conn);
        let stored: String = conn.query_row(
            "SELECT error_message FROM proxy_request_logs WHERE request_id = 'req-huge-error'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(stored.chars().count(), MAX_ERROR_MESSAGE_CHARS);
        assert!(stored.starts_with(prefix));
        Ok(())
    }

    #[test]
    fn grokbuild_logs_total_input_token_semantics() -> Result<(), AppError> {
        let db = Database::memory()?;
        let logger = UsageLogger::new(&db);
        let log = RequestLog {
            request_id: "grok-semantics".to_string(),
            provider_id: "grok-provider".to_string(),
            app_type: "grokbuild".to_string(),
            model: "grok-4.5".to_string(),
            request_model: "grok-4.5".to_string(),
            pricing_model: String::new(),
            usage: TokenUsage::default(),
            cost: None,
            latency_ms: 1,
            first_token_ms: None,
            status_code: 200,
            error_message: None,
            session_id: None,
            provider_type: Some("grokbuild".to_string()),
            is_streaming: false,
            cost_multiplier: "1".to_string(),
            gateway_key_id: None,
        };

        logger.log_request(&log)?;

        let conn = crate::database::lock_conn!(db.conn);
        let semantics: i64 = conn.query_row(
            "SELECT input_token_semantics FROM proxy_request_logs WHERE request_id = 'grok-semantics'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(semantics, INPUT_TOKEN_SEMANTICS_TOTAL);
        Ok(())
    }
}
