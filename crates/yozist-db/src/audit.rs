//! 監査ログ。書き込み系操作の追跡用。
//!
//! 一元化された `MetaStore` 同一 DB に保存し、REST/SMB/WebUI を含む全経路から
//! 同じ口で記録する。失敗操作も `result=error:...` として記録する。

use sqlx::{Row, SqlitePool};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AuditEntry {
    pub id: String,
    pub timestamp: String,
    pub actor_id: Option<String>,
    pub actor_label: Option<String>,
    pub action: String,
    pub target_type: Option<String>,
    pub target_ref: Option<String>,
    pub metadata_json: Option<String>,
    pub result: String,
}

#[derive(Default, Debug, Clone)]
pub struct AuditRecord<'a> {
    pub actor_id: Option<&'a str>,
    pub actor_label: Option<&'a str>,
    pub action: &'a str,
    pub target_type: Option<&'a str>,
    pub target_ref: Option<&'a str>,
    pub metadata_json: Option<&'a str>,
    pub result: &'a str,
}

#[derive(Clone)]
pub struct AuditLog {
    pool: SqlitePool,
}

impl AuditLog {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 1 件のレコードを書き込む。失敗してもログ自体には伝搬しない（呼出し側で
    /// `tracing::warn!` 程度に留める想定）。
    pub async fn record(&self, r: &AuditRecord<'_>) -> Result<(), sqlx::Error> {
        let id = Uuid::now_v7().to_string();
        let now = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        sqlx::query(
            "INSERT INTO audit_log
               (id, timestamp, actor_id, actor_label, action,
                target_type, target_ref, metadata_json, result)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(now)
        .bind(r.actor_id)
        .bind(r.actor_label)
        .bind(r.action)
        .bind(r.target_type)
        .bind(r.target_ref)
        .bind(r.metadata_json)
        .bind(r.result)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 直近 N 件を新しい順で取得。
    pub async fn recent(&self, limit: u32) -> Result<Vec<AuditEntry>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, timestamp, actor_id, actor_label, action,
                    target_type, target_ref, metadata_json, result
             FROM audit_log
             ORDER BY timestamp DESC
             LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| AuditEntry {
                id: r.try_get("id").unwrap_or_default(),
                timestamp: r.try_get("timestamp").unwrap_or_default(),
                actor_id: r.try_get("actor_id").ok(),
                actor_label: r.try_get("actor_label").ok(),
                action: r.try_get("action").unwrap_or_default(),
                target_type: r.try_get("target_type").ok(),
                target_ref: r.try_get("target_ref").ok(),
                metadata_json: r.try_get("metadata_json").ok(),
                result: r.try_get("result").unwrap_or_default(),
            })
            .collect())
    }
}

pub type SharedAuditLog = Arc<AuditLog>;
