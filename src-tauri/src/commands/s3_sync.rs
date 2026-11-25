use serde_json::Value;
use tauri::State;

use crate::commands::sync_support::{self, SyncBackend};
use crate::database::Database;
use crate::error::AppError;
use crate::services::{s3_auto_sync, s3_sync as s3_sync_service};
use crate::settings::{self, S3SyncSettings, WebDavSyncStatus};
use crate::store::AppState;

pub(crate) struct S3Backend;

impl SyncBackend for S3Backend {
    type Settings = S3SyncSettings;
    const NAME: &'static str = "S3";
    const NOT_CONFIGURED: [&'static str; 3] = [
        "s3.sync.not_configured",
        "未配置 S3 同步",
        "S3 sync is not configured.",
    ];
    const DISABLED: [&'static str; 3] =
        ["s3.sync.disabled", "S3 同步未启用", "S3 sync is disabled."];

    fn load() -> Option<S3SyncSettings> {
        settings::get_s3_sync_settings()
    }
    fn store(settings: Option<S3SyncSettings>) -> Result<(), AppError> {
        settings::set_s3_sync_settings(settings)
    }
    fn store_status(status: WebDavSyncStatus) -> Result<(), AppError> {
        settings::update_s3_sync_status(status)
    }
    fn is_enabled(settings: &S3SyncSettings) -> bool {
        settings.enabled
    }
    fn status_mut(settings: &mut S3SyncSettings) -> &mut WebDavSyncStatus {
        &mut settings.status
    }
    fn secret_mut(settings: &mut S3SyncSettings) -> &mut String {
        &mut settings.secret_access_key
    }
    fn normalize_and_validate(settings: &mut S3SyncSettings) -> Result<(), AppError> {
        settings.normalize();
        settings.validate()
    }
    fn suppress_auto_sync() -> impl Send {
        s3_auto_sync::AutoSyncSuppressionGuard::new()
    }
    async fn check_connection(settings: &S3SyncSettings) -> Result<(), AppError> {
        s3_sync_service::check_connection(settings).await
    }
    fn destination(s: &S3SyncSettings) -> String {
        s3_sync_service::destination(s)
    }
    async fn upload<'a>(
        db: &'a Database,
        settings: &'a mut S3SyncSettings,
        token: Option<&'a str>,
    ) -> Result<Value, AppError> {
        s3_sync_service::upload_checked(db, settings, token).await
    }
    async fn download<'a>(
        db: &'a Database,
        settings: &'a mut S3SyncSettings,
        intent: crate::services::sync_review::Pull<'a>,
    ) -> Result<Value, AppError> {
        s3_sync_service::download(db, settings, intent).await
    }
    async fn fetch_remote_info(settings: &S3SyncSettings) -> Result<Option<Value>, AppError> {
        s3_sync_service::fetch_remote_info(settings).await
    }
}

#[tauri::command]
pub async fn s3_test_connection(
    settings: S3SyncSettings,
    #[allow(non_snake_case)] preserveEmptyPassword: Option<bool>,
) -> Result<Value, String> {
    sync_support::test_connection::<S3Backend>(settings, preserveEmptyPassword).await
}

#[tauri::command]
pub async fn s3_sync_upload(
    state: State<'_, AppState>,
    token: Option<String>,
) -> Result<Value, String> {
    sync_support::sync_upload::<S3Backend>(&state, token).await
}

#[tauri::command]
pub async fn s3_sync_download(
    state: State<'_, AppState>,
    token: Option<String>,
) -> Result<Value, String> {
    sync_support::sync_download::<S3Backend>(&state, token).await
}

#[tauri::command]
pub async fn s3_sync_save_settings(
    settings: S3SyncSettings,
    #[allow(non_snake_case)] passwordTouched: Option<bool>,
) -> Result<Value, String> {
    sync_support::sync_save_settings::<S3Backend>(settings, passwordTouched).await
}

#[tauri::command]
pub async fn s3_sync_clear_settings() -> Result<Value, String> {
    sync_support::sync_clear_settings::<S3Backend>().await
}

#[tauri::command]
pub async fn s3_sync_fetch_remote_info() -> Result<Value, String> {
    sync_support::sync_fetch_remote_info::<S3Backend>().await
}
