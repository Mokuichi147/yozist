//! ACL ルールの永続化と認可判定（`Authorizer` 実装）。
//!
//! # 評価ロジック
//! 1. `AuthContext::System` → 常に allow
//! 2. `AuthContext::Anonymous` → `Read`/`View` のみ allow（公開リソース）
//! 3. `AuthContext::User { user, groups }`:
//!    a. subject が user または所属 group の rule を取得
//!    b. `target.kind` / `target.ref_` が一致するものを抽出
//!    c. `priority DESC` でソート、最初にマッチした rule の effect を返す
//!    d. マッチ無し:
//!       - rule が一切無い場合は **default allow**（単一ユーザー bootstrap 用）
//!       - 1 つでも rule があれば **default deny**

use async_trait::async_trait;
use sqlx::{Row, SqlitePool};
use uuid::Uuid;
use yozist_core::{GroupId, UserId};

use crate::{AuthContext, AuthError, Authorizer, Permission, PermissionMask, Subject, Target};

/// SQLite ベースの ACL ストア + 認可判定器。
pub struct DbAuthorizer {
    pool: SqlitePool,
}

impl DbAuthorizer {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 新規 rule を保存。
    pub async fn add_rule(&self, p: &Permission) -> Result<Uuid, AuthError> {
        let id = Uuid::now_v7();
        let (st, sid) = match &p.subject {
            Subject::User(u) => ("user", u.to_string()),
            Subject::Group(g) => ("group", g.to_string()),
        };
        let mask = p.mask.bits() as i64;
        let effect = if p.allow { "allow" } else { "deny" };

        sqlx::query(
            r#"INSERT INTO acl_rules
               (id, subject_type, subject_id, target_type, target_ref,
                permission_mask, effect, priority)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(id.to_string())
        .bind(st)
        .bind(&sid)
        .bind(&p.target.kind)
        .bind(&p.target.ref_)
        .bind(mask)
        .bind(effect)
        .bind(p.priority as i64)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// rule 削除。
    pub async fn delete_rule(&self, id: &Uuid) -> Result<(), AuthError> {
        sqlx::query("DELETE FROM acl_rules WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 全 rule 数（テスト・default-allow 判定用）。
    pub async fn rule_count(&self) -> Result<i64, AuthError> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM acl_rules")
            .fetch_one(&self.pool)
            .await?;
        Ok(n)
    }

    /// 指定 subject + target に該当する rule を `priority DESC` で取得。
    async fn matching_rules(
        &self,
        user: UserId,
        groups: &[GroupId],
        target: &Target,
    ) -> Result<Vec<RuleRow>, AuthError> {
        let mut subject_clauses = vec!["(subject_type = 'user' AND subject_id = ?)".to_string()];
        let mut binds: Vec<String> = vec![user.to_string()];
        for g in groups {
            subject_clauses.push("(subject_type = 'group' AND subject_id = ?)".to_string());
            binds.push(g.to_string());
        }
        let subject_sql = subject_clauses.join(" OR ");

        let sql = format!(
            r#"SELECT permission_mask, effect, priority
               FROM acl_rules
               WHERE ({subject_sql})
                 AND target_type = ?
                 AND target_ref = ?
               ORDER BY priority DESC, effect ASC"#
        );
        let mut q = sqlx::query(&sql);
        for b in &binds {
            q = q.bind(b);
        }
        q = q.bind(&target.kind).bind(&target.ref_);
        let rows = q.fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|r| {
                Ok(RuleRow {
                    mask: PermissionMask::from_bits_truncate(
                        r.try_get::<i64, _>("permission_mask")? as u32,
                    ),
                    allow: r.try_get::<String, _>("effect")? == "allow",
                    priority: r.try_get::<i64, _>("priority")? as i32,
                })
            })
            .collect()
    }
}

#[derive(Debug)]
struct RuleRow {
    mask: PermissionMask,
    allow: bool,
    #[allow(dead_code)]
    priority: i32,
}

#[async_trait]
impl Authorizer for DbAuthorizer {
    async fn check(
        &self,
        ctx: &AuthContext,
        target: &Target,
        required: PermissionMask,
    ) -> Result<bool, AuthError> {
        match ctx {
            AuthContext::System => Ok(true),
            AuthContext::Anonymous => {
                Ok(required.intersects(PermissionMask::VIEW | PermissionMask::READ)
                    && !required.intersects(PermissionMask::WRITE | PermissionMask::ADMIN))
            }
            AuthContext::User { user, groups } => {
                let rules = self.matching_rules(user.id, groups, target).await?;
                if rules.is_empty() {
                    if self.rule_count().await? == 0 {
                        return Ok(true);
                    }
                    return Ok(false);
                }
                for r in rules {
                    if r.mask.contains(required) {
                        return Ok(r.allow);
                    }
                }
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use user_permission_core::Database as AuthDb;
    use yozist_core::FileId;
    use yozist_db::SqliteMetaStore;

    struct Fixtures {
        authz: DbAuthorizer,
        auth: AuthDb,
        _tmp: tempfile::TempDir,
    }

    async fn fixtures() -> Fixtures {
        let store = SqliteMetaStore::open_in_memory().await.unwrap();
        let pool = store.pool().clone();
        let authz = DbAuthorizer::new(pool);
        let tmp = tempfile::tempdir().unwrap();
        let auth = AuthDb::open_local(tmp.path().join("auth.db"), Some(tmp.path().join("secret")))
            .await
            .unwrap();
        Fixtures {
            authz,
            auth,
            _tmp: tmp,
        }
    }

    #[tokio::test]
    async fn bootstrap_allows_authenticated_user_when_no_rules() {
        let f = fixtures().await;
        let user = f
            .auth
            .users()
            .create("alice", "zX9!qLm4-vK7wR", "Alice", None)
            .await
            .unwrap();
        let ctx = AuthContext::User {
            user,
            groups: vec![],
        };
        let allowed = f
            .authz
            .check(&ctx, &Target::file(FileId::new()), PermissionMask::WRITE)
            .await
            .unwrap();
        assert!(allowed, "bootstrap mode should allow write");
    }

    #[tokio::test]
    async fn anonymous_can_read_not_write() {
        let f = fixtures().await;
        let ctx = AuthContext::Anonymous;
        let target = Target::file(FileId::new());
        assert!(f
            .authz
            .check(&ctx, &target, PermissionMask::READ)
            .await
            .unwrap());
        assert!(!f
            .authz
            .check(&ctx, &target, PermissionMask::WRITE)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn explicit_deny_overrides_implicit_allow() {
        let f = fixtures().await;
        let user = f
            .auth
            .users()
            .create("bob", "zX9!qLm4-vK7wR", "Bob", None)
            .await
            .unwrap();
        let file = FileId::new();

        f.authz
            .add_rule(&Permission {
                subject: Subject::User(user.id),
                target: Target::file(file),
                mask: PermissionMask::WRITE,
                allow: false,
                priority: 10,
            })
            .await
            .unwrap();

        let ctx = AuthContext::User {
            user,
            groups: vec![],
        };
        assert!(!f
            .authz
            .check(&ctx, &Target::file(file), PermissionMask::WRITE)
            .await
            .unwrap());
        assert!(!f
            .authz
            .check(&ctx, &Target::file(file), PermissionMask::READ)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn group_rule_grants_member_access() {
        let f = fixtures().await;
        let user = f
            .auth
            .users()
            .create("dan", "zX9!qLm4-vK7wR", "Dan", None)
            .await
            .unwrap();
        let group = f
            .auth
            .groups()
            .create("editors", "Editors", false, None)
            .await
            .unwrap();
        f.auth
            .groups()
            .add_user(group.id, user.id, None)
            .await
            .unwrap();

        let file = FileId::new();
        f.authz
            .add_rule(&Permission {
                subject: Subject::Group(group.id),
                target: Target::file(file),
                mask: PermissionMask::WRITE,
                allow: true,
                priority: 5,
            })
            .await
            .unwrap();

        let ctx = AuthContext::User {
            user,
            groups: vec![group.id],
        };
        assert!(f
            .authz
            .check(&ctx, &Target::file(file), PermissionMask::WRITE)
            .await
            .unwrap());
        let _ = Duration::from_millis(0); // 未使用警告抑制
    }
}
