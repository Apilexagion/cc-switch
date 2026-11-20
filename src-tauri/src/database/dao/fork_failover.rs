//! Fork 扩展：模型族故障转移队列 DAO
//!
//! 从 dao/failover.rs 隔离出的 fork 独有逻辑，降低与上游合并冲突概率

use crate::app_config::AppType;
use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::provider::Provider;
use std::str::FromStr;

use super::failover::FailoverQueueItem;

impl Database {
    // ==================== 模型族独立故障转移队列（Fork 扩展） ====================

    /// 获取模型族独立故障转移队列（按 sort_index 排序）
    pub fn get_failover_queue_for_model(
        &self,
        app_type: &str,
        model_key: &str,
    ) -> Result<Vec<FailoverQueueItem>, AppError> {
        let conn = lock_conn!(self.conn);

        let mut stmt = conn
            .prepare(
                "SELECT p.id, p.name, q.sort_index, p.notes
                 FROM forkdb.fork_model_failover_queue q
                 JOIN main.providers p ON p.id = q.provider_id AND p.app_type = q.app_type
                 WHERE q.app_type = ?1 AND q.model_key = ?2
                 ORDER BY COALESCE(q.sort_index, 999999), p.id ASC",
            )
            .map_err(|e| AppError::Database(e.to_string()))?;

        let items = stmt
            .query_map(rusqlite::params![app_type, model_key], |row| {
                Ok(FailoverQueueItem {
                    provider_id: row.get(0)?,
                    provider_name: row.get(1)?,
                    sort_index: row.get(2)?,
                    provider_notes: row.get(3)?,
                })
            })
            .map_err(|e| AppError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(items)
    }

    /// 覆盖写入模型族独立故障转移队列（按传入顺序）
    pub fn set_failover_queue_for_model(
        &self,
        app_type: &str,
        model_key: &str,
        provider_ids: &[String],
    ) -> Result<(), AppError> {
        let mut conn = lock_conn!(self.conn);
        let tx = conn
            .transaction()
            .map_err(|e| AppError::Database(e.to_string()))?;

        tx.execute(
            "DELETE FROM forkdb.fork_model_failover_queue WHERE app_type = ?1 AND model_key = ?2",
            rusqlite::params![app_type, model_key],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;

        let now = chrono::Utc::now().to_rfc3339();
        for (index, provider_id) in provider_ids.iter().enumerate() {
            tx.execute(
                "INSERT INTO forkdb.fork_model_failover_queue
                 (app_type, model_key, provider_id, sort_index, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![app_type, model_key, provider_id, index as i64, &now],
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        }

        tx.commit().map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    /// 获取模型族可加入故障转移队列的供应商（不在当前模型族队列中的）
    pub fn get_available_providers_for_model_failover(
        &self,
        app_type: &str,
        model_key: &str,
    ) -> Result<Vec<Provider>, AppError> {
        let all = self.get_all_providers(&AppType::from_str(app_type)?)?;
        let existing = self
            .get_failover_queue_for_model(app_type, model_key)?
            .into_iter()
            .map(|item| item.provider_id)
            .collect::<std::collections::HashSet<_>>();

        Ok(all
            .into_values()
            .filter(|provider| !existing.contains(&provider.id))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::Database;
    use crate::app_config::AppType;
    use crate::provider::Provider;
    use serde_json::json;
    use std::str::FromStr;

    fn save_provider(db: &Database, app_type: &str, id: &str, name: &str) {
        let provider = Provider::with_id(id.to_string(), name.to_string(), json!({}), None);
        db.save_provider(
            &AppType::from_str(app_type).expect("valid app type"),
            &provider,
        )
        .expect("save provider in main database");
    }

    #[test]
    fn model_failover_queue_reads_provider_name_from_main_database() {
        let db = Database::memory().expect("memory database");
        save_provider(&db, "claude", "provider-a", "Provider A");

        db.set_failover_queue_for_model("claude", "sonnet", &["provider-a".to_string()])
            .expect("save model failover queue");

        let queue = db
            .get_failover_queue_for_model("claude", "sonnet")
            .expect("read model failover queue");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].provider_id, "provider-a");
        assert_eq!(queue[0].provider_name, "Provider A");
    }
}
