//! Fork 扩展：模型族故障转移相关 Tauri 命令
//!
//! 从 commands/failover.rs 隔离出的 fork 独有命令，降低与上游合并冲突概率

use crate::app_config::AppType;
use crate::database::{Database, FailoverQueueItem};
use crate::error::AppError;
use crate::provider::Provider;
use crate::store::AppState;
use std::collections::HashSet;
use std::str::FromStr;

fn require_fork_failover_app(app_type: &str) -> Result<AppType, AppError> {
    let app = AppType::from_str(app_type)?;
    if !app.supports_local_proxy() {
        return Err(AppError::InvalidInput(format!(
            "{} 不支持故障转移",
            app.as_str()
        )));
    }
    Ok(app)
}

fn require_fork_failover_provider(
    db: &Database,
    app_type: &str,
    provider_id: &str,
) -> Result<Provider, AppError> {
    let app_type = &AppType::from_str(app_type)?;
    let provider = db
        .get_provider_by_id(provider_id, app_type)?
        .ok_or_else(|| AppError::InvalidInput(format!("供应商不存在: {provider_id}")))?;
    if !crate::proxy::provider_router::provider_supports_failover(app_type, &provider) {
        return Err(AppError::InvalidInput(
            "Codex Official 账号卡不支持自动故障转移".to_string(),
        ));
    }
    Ok(provider)
}

fn filter_failover_queue_items(
    db: &Database,
    app_type: &str,
    queue: Vec<FailoverQueueItem>,
) -> Result<Vec<FailoverQueueItem>, AppError> {
    let app_type = &AppType::from_str(app_type)?;
    let providers = db
        .get_provider_rows(app_type)?
        .into_readable("模型族故障转移队列");
    Ok(queue
        .into_iter()
        .filter(|item| {
            providers.get(&item.provider_id).is_some_and(|provider| {
                crate::proxy::provider_router::provider_supports_failover(app_type, provider)
            })
        })
        .collect())
}

fn filter_available_providers(app_type: &AppType, providers: Vec<Provider>) -> Vec<Provider> {
    providers
        .into_iter()
        .filter(|provider| {
            crate::proxy::provider_router::provider_supports_failover(app_type, provider)
        })
        .collect()
}

fn normalize_provider_ids(
    db: &Database,
    app_type: &str,
    provider_ids: &[String],
) -> Result<Vec<String>, AppError> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::with_capacity(provider_ids.len());
    for provider_id in provider_ids {
        let provider_id = provider_id.trim();
        if provider_id.is_empty() {
            return Err(AppError::InvalidInput("供应商 ID 不能为空".to_string()));
        }
        if !seen.insert(provider_id.to_string()) {
            return Err(AppError::InvalidInput(format!(
                "故障转移队列包含重复供应商: {provider_id}"
            )));
        }
        require_fork_failover_provider(db, app_type, provider_id)?;
        normalized.push(provider_id.to_string());
    }
    Ok(normalized)
}

/// 获取模型族独立故障转移队列（Fork 扩展）
#[tauri::command]
pub async fn get_failover_queue_for_model(
    state: tauri::State<'_, AppState>,
    app_type: String,
    model_key: String,
) -> Result<Vec<FailoverQueueItem>, String> {
    get_failover_queue_for_model_internal(&state, &app_type, &model_key)
        .await
        .map_err(|e| e.to_string())
}

/// 获取模型族可添加到队列的供应商（Fork 扩展）
#[tauri::command]
pub async fn get_available_providers_for_model_failover(
    state: tauri::State<'_, AppState>,
    app_type: String,
    model_key: String,
) -> Result<Vec<Provider>, String> {
    get_available_providers_for_model_failover_internal(&state, &app_type, &model_key)
        .await
        .map_err(|e| e.to_string())
}

/// 覆盖写入模型族独立故障转移队列（Fork 扩展）
#[tauri::command]
pub async fn set_failover_queue_for_model(
    state: tauri::State<'_, AppState>,
    app_type: String,
    model_key: String,
    provider_ids: Vec<String>,
) -> Result<(), String> {
    set_failover_queue_for_model_internal(&state, &app_type, &model_key, &provider_ids)
        .await
        .map_err(|e| e.to_string())
}

// ==================== Internal helpers + test hooks ====================

async fn get_failover_queue_for_model_internal(
    state: &AppState,
    app_type: &str,
    model_key: &str,
) -> Result<Vec<FailoverQueueItem>, AppError> {
    let app = require_fork_failover_app(app_type)?;
    let app_type = app.as_str();
    let queue = state.db.get_failover_queue_for_model(app_type, model_key)?;
    filter_failover_queue_items(&state.db, app_type, queue)
}

#[cfg_attr(not(feature = "test-hooks"), doc(hidden))]
pub async fn get_failover_queue_for_model_test_hook(
    state: &AppState,
    app_type: &str,
    model_key: &str,
) -> Result<Vec<FailoverQueueItem>, AppError> {
    get_failover_queue_for_model_internal(state, app_type, model_key).await
}

async fn get_available_providers_for_model_failover_internal(
    state: &AppState,
    app_type: &str,
    model_key: &str,
) -> Result<Vec<Provider>, AppError> {
    let app = require_fork_failover_app(app_type)?;
    let app_type = app.as_str();
    let providers = state
        .db
        .get_available_providers_for_model_failover(app_type, model_key)?;
    Ok(filter_available_providers(&app, providers))
}

#[cfg_attr(not(feature = "test-hooks"), doc(hidden))]
pub async fn get_available_providers_for_model_failover_test_hook(
    state: &AppState,
    app_type: &str,
    model_key: &str,
) -> Result<Vec<Provider>, AppError> {
    get_available_providers_for_model_failover_internal(state, app_type, model_key).await
}

async fn set_failover_queue_for_model_internal(
    state: &AppState,
    app_type: &str,
    model_key: &str,
    provider_ids: &[String],
) -> Result<(), AppError> {
    let app = require_fork_failover_app(app_type)?;
    let app_type = app.as_str();
    let provider_ids = normalize_provider_ids(&state.db, app_type, provider_ids)?;
    state
        .db
        .set_failover_queue_for_model(app_type, model_key, &provider_ids)
}

#[cfg_attr(not(feature = "test-hooks"), doc(hidden))]
pub async fn set_failover_queue_for_model_test_hook(
    state: &AppState,
    app_type: &str,
    model_key: &str,
    provider_ids: &[String],
) -> Result<(), AppError> {
    set_failover_queue_for_model_internal(state, app_type, model_key, provider_ids).await
}

#[cfg(test)]
mod tests {
    use super::{
        filter_available_providers, filter_failover_queue_items, normalize_provider_ids,
        require_fork_failover_app,
    };
    use crate::app_config::AppType;
    use crate::database::{Database, FailoverQueueItem, CODEX_OFFICIAL_PROVIDER_ID};
    use crate::provider::Provider;
    use serde_json::json;
    use std::str::FromStr;

    fn save_provider(db: &Database, app_type: &str, id: &str, name: &str, official: bool) {
        let mut provider = Provider::with_id(id.to_string(), name.to_string(), json!({}), None);
        if official {
            provider.category = Some("official".to_string());
        }
        db.save_provider(
            &AppType::from_str(app_type).expect("valid app type"),
            &provider,
        )
        .expect("save provider");
    }

    #[test]
    fn fork_failover_validates_app_provider_ownership_and_duplicates() {
        let db = Database::memory().expect("memory database");
        save_provider(&db, "codex", "provider-a", "Provider A", false);
        save_provider(&db, "claude", "provider-b", "Provider B", false);

        assert!(require_fork_failover_app("codex").is_ok());
        assert!(require_fork_failover_app("pi").is_err());
        assert!(require_fork_failover_app("unknown").is_err());

        let duplicate = vec!["provider-a".to_string(), "provider-a".to_string()];
        assert!(normalize_provider_ids(&db, "codex", &duplicate).is_err());
        assert!(normalize_provider_ids(&db, "codex", &["provider-b".to_string()]).is_err());
    }

    #[test]
    fn fork_failover_rejects_codex_official_provider_writes() {
        let db = Database::memory().expect("memory database");
        save_provider(
            &db,
            "codex",
            CODEX_OFFICIAL_PROVIDER_ID,
            "Codex Official",
            true,
        );

        assert!(
            normalize_provider_ids(&db, "codex", &[CODEX_OFFICIAL_PROVIDER_ID.to_string()])
                .is_err()
        );
    }

    #[test]
    fn fork_failover_filters_stale_codex_official_entries() {
        let db = Database::memory().expect("memory database");
        save_provider(&db, "codex", "provider-a", "Provider A", false);
        save_provider(
            &db,
            "codex",
            CODEX_OFFICIAL_PROVIDER_ID,
            "Codex Official",
            true,
        );

        let queue = vec![
            FailoverQueueItem {
                provider_id: CODEX_OFFICIAL_PROVIDER_ID.to_string(),
                provider_name: "Codex Official".to_string(),
                sort_index: Some(0),
                provider_notes: None,
            },
            FailoverQueueItem {
                provider_id: "provider-a".to_string(),
                provider_name: "Provider A".to_string(),
                sort_index: Some(1),
                provider_notes: None,
            },
        ];
        let filtered_queue =
            filter_failover_queue_items(&db, "codex", queue).expect("filter queue");
        assert_eq!(filtered_queue.len(), 1);
        assert_eq!(filtered_queue[0].provider_id, "provider-a");

        let providers = db
            .get_all_providers(&AppType::Codex)
            .expect("read providers")
            .into_values()
            .collect();
        let available = filter_available_providers(&AppType::Codex, providers);
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].id, "provider-a");
    }
}
