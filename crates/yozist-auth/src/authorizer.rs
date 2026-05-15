//! ACL ルールの永続化と認可判定（`Authorizer` 実装）。
//!
//! # 評価ロジック（v1）
//! 1. `AuthContext::System` → 常に allow
//! 2. `AuthContext::Anonymous` → `Read`/`View` のみ allow（公開リソース）
//! 3. `AuthContext::User { user, groups }`:
//!    a. subject が user または所属 group の rule を取得
//!    b. 対象が target と一致するものを抽出
//!       - `Target::File(id)` → `target_type='file' AND target_ref=id`
//!       - `Target::Tag(id)` → `target_type='tag' AND target_ref=id`
//!       - `Target::Series(id)` → `target_type='series' AND target_ref=id`
//!       - `Target::Share(name)` → `target_type='share' AND target_ref=name`
//!       - `Target::Query(_)` → 未対応（TODO）
//!    c. `priority DESC` でソート、最初にマッチした rule の effect を返す
//!    d. マッチ無し:
//!       - rule が一切無い場合は **default allow**（単一ユーザー bootstrap 用）
//!       - 1 つでも rule があれば **default deny**
//!
//! # TODO
//! - [ ] `Target::Query` の評価（saved query → 対象ファイル展開）
//! - [ ] Tag/Series rule とファイルの間接マッチ（ファイル経由のチェック）
//! - [ ] 期限切れ rule の自動除外
//! - [ ] 結果キャッシュ（rule リスト変更時に invalidation）

use async_trait::async_trait;
use sqlx::{Row, SqlitePool};
use uuid::Uuid;
use yozist_core::{GroupId, UserId};

use crate::{
    AuthContext, AuthError, Authorizer, Permission, PermissionMask, Subject, Target,
};

/// SQLite ベースの ACL ストア + 認可判定器。
///
/// `SqliteAuthService` と同じプールを共有する。
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
        let (tt, tref) = target_to_db(&p.target)?;
        let mask = p.mask.bits() as i64;
        let effect = if p.allow { "allow" } else { "deny" };
        let expires = p
            .expires_at
            .map(|d| {
                d.format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default()
            });

        sqlx::query(
            r#"INSERT INTO acl_rules
               (id, subject_type, subject_id, target_type, target_ref,
                permission_mask, effect, priority, expires_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(id.to_string())
        .bind(st)
        .bind(&sid)
        .bind(tt)
        .bind(&tref)
        .bind(mask)
        .bind(effect)
        .bind(p.priority as i64)
        .bind(expires)
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
        user: &UserId,
        groups: &[GroupId],
        target: &Target,
    ) -> Result<Vec<RuleRow>, AuthError> {
        let (tt, tref) = target_to_db(target)?;

        // subject 条件を動的に組み立てる。
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
                 AND (expires_at IS NULL OR expires_at > datetime('now'))
               ORDER BY priority DESC, effect ASC"#
        );
        let mut q = sqlx::query(&sql);
        for b in &binds {
            q = q.bind(b);
        }
        q = q.bind(tt).bind(tref);
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

fn target_to_db(t: &Target) -> Result<(&'static str, String), AuthError> {
    Ok(match t {
        Target::Share(name) => ("share", name.clone()),
        Target::Tag(id) => ("tag", id.to_string()),
        Target::Series(id) => ("series", id.to_string()),
        Target::File(id) => ("file", id.to_string()),
        Target::Query(_) => {
            return Err(AuthError::Other(
                "query target evaluation not yet supported".into(),
            ));
        }
    })
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
                // anonymous は読取系のみ許容。書き込みは拒否。
                Ok(required.intersects(PermissionMask::VIEW | PermissionMask::READ)
                    && !required.intersects(PermissionMask::WRITE | PermissionMask::ADMIN))
            }
            AuthContext::User { user, groups } => {
                let rules = self.matching_rules(&user.id, groups, target).await?;
                // 一切 rule が無く、かつ DB 全体でも 1 件も rule が無いなら
                // bootstrap モードとして allow する。
                if rules.is_empty() {
                    if self.rule_count().await? == 0 {
                        return Ok(true);
                    }
                    return Ok(false);
                }
                // priority 降順。同一 priority は deny を優先。
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
    use crate::{AuthService, Permission, SqliteAuthService};
    use yozist_core::FileId;
    use yozist_db::SqliteMetaStore;

    async fn fixtures() -> (DbAuthorizer, SqliteAuthService) {
        let store = SqliteMetaStore::open_in_memory().await.unwrap();
        let pool = store.pool().clone();
        let auth = SqliteAuthService::new(pool.clone(), b"sec".to_vec());
        let authz = DbAuthorizer::new(pool);
        (authz, auth)
    }

    #[tokio::test]
    async fn bootstrap_allows_authenticated_user_when_no_rules() {
        let (authz, auth) = fixtures().await;
        let user = auth.create_user("alice", "pw").await.unwrap();
        let ctx = AuthContext::User {
            user,
            groups: vec![],
        };
        let allowed = authz
            .check(
                &ctx,
                &Target::File(FileId::new()),
                PermissionMask::WRITE,
            )
            .await
            .unwrap();
        assert!(allowed, "bootstrap mode should allow write");
    }

    #[tokio::test]
    async fn anonymous_can_read_not_write() {
        let (authz, _auth) = fixtures().await;
        let ctx = AuthContext::Anonymous;
        let target = Target::File(FileId::new());
        assert!(authz
            .check(&ctx, &target, PermissionMask::READ)
            .await
            .unwrap());
        assert!(!authz
            .check(&ctx, &target, PermissionMask::WRITE)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn explicit_deny_overrides_implicit_allow() {
        let (authz, auth) = fixtures().await;
        let user = auth.create_user("bob", "pw").await.unwrap();
        let file = FileId::new();

        // deny rule を追加 → 全体に rule が存在するので bootstrap mode は終了
        authz
            .add_rule(&Permission {
                subject: Subject::User(user.id),
                target: Target::File(file),
                mask: PermissionMask::WRITE,
                allow: false,
                priority: 10,
                expires_at: None,
            })
            .await
            .unwrap();

        let ctx = AuthContext::User {
            user,
            groups: vec![],
        };
        let allowed = authz
            .check(&ctx, &Target::File(file), PermissionMask::WRITE)
            .await
            .unwrap();
        assert!(!allowed);
        // 同一 file の READ も rule に書かれていないので deny
        let read = authz
            .check(&ctx, &Target::File(file), PermissionMask::READ)
            .await
            .unwrap();
        assert!(!read);
    }

    #[tokio::test]
    async fn higher_priority_allow_beats_lower_deny() {
        let (authz, auth) = fixtures().await;
        let user = auth.create_user("carol", "pw").await.unwrap();
        let file = FileId::new();
        // 低 priority deny
        authz
            .add_rule(&Permission {
                subject: Subject::User(user.id),
                target: Target::File(file),
                mask: PermissionMask::WRITE,
                allow: false,
                priority: 1,
                expires_at: None,
            })
            .await
            .unwrap();
        // 高 priority allow
        authz
            .add_rule(&Permission {
                subject: Subject::User(user.id),
                target: Target::File(file),
                mask: PermissionMask::WRITE | PermissionMask::READ,
                allow: true,
                priority: 50,
                expires_at: None,
            })
            .await
            .unwrap();
        let ctx = AuthContext::User {
            user,
            groups: vec![],
        };
        assert!(authz
            .check(&ctx, &Target::File(file), PermissionMask::WRITE)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn group_rule_grants_member_access() {
        let (authz, auth) = fixtures().await;
        let user = auth.create_user("dan", "pw").await.unwrap();
        let group = auth.create_group("editors", None).await.unwrap();
        auth.add_user_to_group(user.id, group.id).await.unwrap();

        let file = FileId::new();
        authz
            .add_rule(&Permission {
                subject: Subject::Group(group.id),
                target: Target::File(file),
                mask: PermissionMask::WRITE,
                allow: true,
                priority: 5,
                expires_at: None,
            })
            .await
            .unwrap();

        let ctx = AuthContext::User {
            user,
            groups: vec![group.id],
        };
        assert!(authz
            .check(&ctx, &Target::File(file), PermissionMask::WRITE)
            .await
            .unwrap());
    }
}
