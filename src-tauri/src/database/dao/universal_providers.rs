//! 统一供应商 (Universal Provider) DAO
//!
//! 提供统一供应商的 CRUD 操作。

use crate::database::{lock_conn, to_json_string, Database};
use crate::error::AppError;
use crate::provider::UniversalProvider;
use std::collections::HashMap;

/// 统一供应商的 Settings Key
const UNIVERSAL_PROVIDERS_KEY: &str = "universal_providers";

impl Database {
    /// 获取所有统一供应商
    pub fn get_all_universal_providers(
        &self,
    ) -> Result<HashMap<String, UniversalProvider>, AppError> {
        let conn = lock_conn!(self.conn);

        let mut stmt = conn
            .prepare("SELECT value FROM settings WHERE key = ?")
            .map_err(|e| AppError::Database(e.to_string()))?;

        let result: Option<String> = stmt
            .query_row([UNIVERSAL_PROVIDERS_KEY], |row| row.get(0))
            .ok();

        match result {
            Some(json) => serde_json::from_str(&json)
                .map_err(|e| AppError::Database(format!("解析统一供应商数据失败: {e}"))),
            None => Ok(HashMap::new()),
        }
    }

    /// 获取单个统一供应商
    pub fn get_universal_provider(&self, id: &str) -> Result<Option<UniversalProvider>, AppError> {
        let providers = self.get_all_universal_providers()?;
        Ok(providers.get(id).cloned())
    }

    /// 保存统一供应商（添加或更新）
    pub fn save_universal_provider(&self, provider: &UniversalProvider) -> Result<(), AppError> {
        let mut providers = self.get_all_universal_providers()?;
        providers.insert(provider.id.clone(), provider.clone());
        self.save_all_universal_providers(&providers)
    }

    /// Caller holds the affected applications' switch locks and has checked
    /// current/route/Stack references. Keep child removal and parent removal
    /// indivisible even if one DELETE or the settings write fails.
    pub(crate) fn remove_universal_provider_children(
        &self,
        id: &str,
        apps: &[crate::app_config::AppType],
        remove_parent: bool,
    ) -> Result<bool, AppError> {
        use rusqlite::OptionalExtension;
        let mut conn = lock_conn!(self.conn);
        let tx = conn
            .transaction()
            .map_err(|e| AppError::Database(e.to_string()))?;
        let raw: Option<String> = tx
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                [UNIVERSAL_PROVIDERS_KEY],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| AppError::Database(e.to_string()))?;
        let mut providers: HashMap<String, UniversalProvider> = raw
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|e| AppError::Database(format!("解析统一供应商数据失败: {e}")))?
            .unwrap_or_default();
        let existed = providers.contains_key(id);
        for app in apps {
            let child = format!("universal-{app}-{id}");
            tx.execute(
                "DELETE FROM providers WHERE app_type = ?1 AND id = ?2",
                rusqlite::params![app, child],
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        }
        if remove_parent && providers.remove(id).is_some() {
            tx.execute(
                "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                [UNIVERSAL_PROVIDERS_KEY, &to_json_string(&providers)?],
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        }
        tx.commit().map_err(|e| AppError::Database(e.to_string()))?;
        Ok(existed)
    }

    /// 保存所有统一供应商（内部方法）
    fn save_all_universal_providers(
        &self,
        providers: &HashMap<String, UniversalProvider>,
    ) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        let json = to_json_string(providers)?;

        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)",
            [UNIVERSAL_PROVIDERS_KEY, &json],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_config::AppType;
    use crate::provider::Provider;

    #[test]
    fn child_delete_failure_rolls_back_all_children_and_the_parent() {
        let db = Database::memory().unwrap();
        let parent = UniversalProvider::new(
            "atomic".into(),
            "Atomic".into(),
            "custom".into(),
            "https://relay.example".into(),
            "placeholder".into(),
        );
        db.save_universal_provider(&parent).unwrap();
        for app in [AppType::Claude, AppType::Codex] {
            let child = Provider::with_id(
                format!("universal-{app}-atomic"),
                "Child".into(),
                serde_json::json!({}),
                None,
            );
            db.save_provider(&app, &child).unwrap();
        }
        db.conn.lock().unwrap().execute_batch("CREATE TRIGGER fail_universal_delete BEFORE DELETE ON providers WHEN OLD.app_type = 'codex' BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
        assert!(db
            .remove_universal_provider_children("atomic", &[AppType::Claude, AppType::Codex], true)
            .is_err());
        assert!(db.get_universal_provider("atomic").unwrap().is_some());
        for app in [AppType::Claude, AppType::Codex] {
            assert!(db
                .provider_exists(&format!("universal-{app}-atomic"), &app)
                .unwrap());
        }
    }
}
