use serde_json::Value;
use tauri::State;

use crate::commands::sync_support::{self, SyncBackend};
use crate::database::Database;
use crate::error::AppError;
use crate::services::{webdav_auto_sync, webdav_sync as webdav_sync_service};
use crate::settings::{self, WebDavSyncSettings, WebDavSyncStatus};
use crate::store::AppState;

pub(crate) struct WebDavBackend;

impl SyncBackend for WebDavBackend {
    type Settings = WebDavSyncSettings;
    const NAME: &'static str = "WebDAV";
    const NOT_CONFIGURED: [&'static str; 3] = [
        "webdav.sync.not_configured",
        "未配置 WebDAV 同步",
        "WebDAV sync is not configured.",
    ];
    const DISABLED: [&'static str; 3] = [
        "webdav.sync.disabled",
        "WebDAV 同步未启用",
        "WebDAV sync is disabled.",
    ];

    fn load() -> Option<WebDavSyncSettings> {
        settings::get_webdav_sync_settings()
    }
    fn store(settings: Option<WebDavSyncSettings>) -> Result<(), AppError> {
        settings::set_webdav_sync_settings(settings)
    }
    fn store_status(status: WebDavSyncStatus) -> Result<(), AppError> {
        settings::update_webdav_sync_status(status)
    }
    fn is_enabled(settings: &WebDavSyncSettings) -> bool {
        settings.enabled
    }
    fn status_mut(settings: &mut WebDavSyncSettings) -> &mut WebDavSyncStatus {
        &mut settings.status
    }
    fn secret_mut(settings: &mut WebDavSyncSettings) -> &mut String {
        &mut settings.password
    }
    fn normalize_and_validate(settings: &mut WebDavSyncSettings) -> Result<(), AppError> {
        settings.normalize();
        settings.validate()
    }
    fn suppress_auto_sync() -> impl Send {
        webdav_auto_sync::AutoSyncSuppressionGuard::new()
    }
    async fn check_connection(settings: &WebDavSyncSettings) -> Result<(), AppError> {
        webdav_sync_service::check_connection(settings).await
    }
    fn destination(s: &WebDavSyncSettings) -> String {
        webdav_sync_service::destination(s)
    }
    async fn upload<'a>(
        db: &'a Database,
        settings: &'a mut WebDavSyncSettings,
        token: Option<&'a str>,
    ) -> Result<Value, AppError> {
        webdav_sync_service::upload_checked(db, settings, token, false).await
    }
    async fn download<'a>(
        db: &'a Database,
        settings: &'a mut WebDavSyncSettings,
        intent: crate::services::sync_review::Pull<'a>,
    ) -> Result<Value, AppError> {
        webdav_sync_service::download(db, settings, intent).await
    }
    async fn fetch_remote_info(settings: &WebDavSyncSettings) -> Result<Option<Value>, AppError> {
        webdav_sync_service::fetch_remote_info(settings).await
    }
}

#[tauri::command]
pub async fn webdav_test_connection(
    settings: WebDavSyncSettings,
    #[allow(non_snake_case)] preserveEmptyPassword: Option<bool>,
) -> Result<Value, String> {
    sync_support::test_connection::<WebDavBackend>(settings, preserveEmptyPassword).await
}

#[tauri::command]
pub async fn webdav_sync_upload(
    state: State<'_, AppState>,
    token: Option<String>,
) -> Result<Value, String> {
    sync_support::sync_upload::<WebDavBackend>(&state, token).await
}

#[tauri::command]
pub async fn webdav_sync_download(
    state: State<'_, AppState>,
    token: Option<String>,
) -> Result<Value, String> {
    sync_support::sync_download::<WebDavBackend>(&state, token).await
}

#[tauri::command]
pub async fn webdav_sync_save_settings(
    settings: WebDavSyncSettings,
    #[allow(non_snake_case)] passwordTouched: Option<bool>,
) -> Result<Value, String> {
    sync_support::sync_save_settings::<WebDavBackend>(settings, passwordTouched).await
}

#[tauri::command]
pub async fn webdav_sync_clear_settings() -> Result<Value, String> {
    sync_support::sync_clear_settings::<WebDavBackend>().await
}

#[tauri::command]
pub async fn webdav_sync_fetch_remote_info() -> Result<Value, String> {
    sync_support::sync_fetch_remote_info::<WebDavBackend>().await
}
