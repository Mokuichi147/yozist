//! 保存クエリ（条件付き仮想ビュー）の解決ロジック。
//!
//! REST（WebUI の一覧）と SMB（`yozist\queries\<名前>\`）の双方が本関数を共有し、
//! 条件評価のセマンティクスを一致させる。スマートフォルダ風に、タグ（システム /
//! 手動 / AI / 種別不問）・シリーズ・種類(MIME)・名前・日付（作成 / 更新）の条件を
//! `MatchMode`（すべて / いずれか）で組み合わせて絞り込む。
//!
//! 実装はまず候補ファイルを取得し、各ファイルを条件に対してメモリ上で評価する
//! 素朴な方式。大規模化したら `list_files_by_tags` 等での前段絞り込みやインデックス
//! 化を検討する（`QUERY_FILE_LIMIT` で上限を設ける）。

use std::collections::{HashMap, HashSet};

use time::{Duration, OffsetDateTime};
use yozist_core::{FileId, FileMeta, MatchMode, QueryCondition, QueryDef, Tag, TagKind};

use crate::{DbError, MetaStore};

/// 評価対象として取得するファイル数の上限。
const QUERY_FILE_LIMIT: u32 = 1000;

/// 保存クエリ定義を解決し、条件にマッチする `FileMeta` 一覧を返す。
pub async fn resolve_query(
    meta: &dyn MetaStore,
    q: &QueryDef,
) -> Result<Vec<FileMeta>, DbError> {
    let candidates = meta.list_files(QUERY_FILE_LIMIT, 0).await?;

    // タグ条件（レガシー tags_and/tags_not を含む）があるならまとめて取得。
    let needs_tags = !q.tags_and.is_empty()
        || !q.tags_not.is_empty()
        || q.conditions.iter().any(|c| is_tag_field(&c.field));
    let tags_by_file: HashMap<FileId, Vec<Tag>> = if needs_tags && !candidates.is_empty() {
        let ids: Vec<FileId> = candidates.iter().map(|f| f.id).collect();
        let mut m: HashMap<FileId, Vec<Tag>> = HashMap::new();
        for (fid, tag) in meta.list_tags_of_many(&ids).await? {
            m.entry(fid).or_default().push(tag);
        }
        m
    } else {
        HashMap::new()
    };

    // シリーズ条件で参照される各シリーズの所属ファイル集合を事前計算。
    let mut series_members: HashMap<String, HashSet<FileId>> = HashMap::new();
    let series_names: Vec<String> = q
        .conditions
        .iter()
        .filter(|c| c.field == "series")
        .map(|c| c.value.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if !series_names.is_empty() {
        let all = meta.list_series().await?;
        for name in series_names {
            if series_members.contains_key(&name) {
                continue;
            }
            let mut set = HashSet::new();
            for s in all.iter().filter(|s| s.name.to_lowercase() == name) {
                for mem in meta.list_series_members(&s.id).await? {
                    set.insert(mem.file_id);
                }
            }
            series_members.insert(name, set);
        }
    }

    let now = OffsetDateTime::now_utc();
    let out = candidates
        .into_iter()
        .filter(|f| {
            eval_file(
                f,
                q,
                tags_by_file.get(&f.id),
                &series_members,
                now,
            )
        })
        .collect();
    Ok(out)
}

fn is_tag_field(field: &str) -> bool {
    matches!(field, "tag" | "manual_tag" | "system_tag" | "ai_tag")
}

/// 1 ファイルが保存クエリにマッチするか。
fn eval_file(
    f: &FileMeta,
    q: &QueryDef,
    tags: Option<&Vec<Tag>>,
    series_members: &HashMap<String, HashSet<FileId>>,
    now: OffsetDateTime,
) -> bool {
    let empty = Vec::new();
    let tags = tags.unwrap_or(&empty);

    let mut results: Vec<bool> = Vec::new();
    // レガシー: tags_and = 含む（種別不問）、tags_not = 含まない。
    for t in &q.tags_and {
        results.push(file_has_tag(tags, None, t));
    }
    for t in &q.tags_not {
        results.push(!file_has_tag(tags, None, t));
    }
    for c in &q.conditions {
        results.push(eval_condition(f, c, tags, series_members, now));
    }

    if results.is_empty() {
        return true; // 条件なし → 全件
    }
    match q.match_mode {
        MatchMode::All => results.iter().all(|&b| b),
        MatchMode::Any => results.iter().any(|&b| b),
    }
}

fn eval_condition(
    f: &FileMeta,
    c: &QueryCondition,
    tags: &[Tag],
    series_members: &HashMap<String, HashSet<FileId>>,
    now: OffsetDateTime,
) -> bool {
    let value = c.value.trim();
    match c.field.as_str() {
        "tag" => set_op(&c.op, file_has_tag(tags, None, value)),
        "manual_tag" => set_op(&c.op, file_has_tag(tags, Some(TagKind::Manual), value)),
        "system_tag" => set_op(&c.op, file_has_tag(tags, Some(TagKind::System), value)),
        "ai_tag" => set_op(&c.op, file_has_tag(tags, Some(TagKind::Ai), value)),
        "series" => {
            let member = series_members
                .get(&value.to_lowercase())
                .is_some_and(|set| set.contains(&f.id));
            set_op(&c.op, member)
        }
        "mime" => {
            let mime = f.mime.as_deref().unwrap_or("");
            let has = !value.is_empty() && mime.to_lowercase().contains(&value.to_lowercase());
            set_op(&c.op, has)
        }
        "name" => text_op(&f.display_name, &c.op, value),
        "created" => date_op(f.created_at, &c.op, value, c.unit.as_deref(), now),
        "updated" => date_op(f.updated_at, &c.op, value, c.unit.as_deref(), now),
        // 未知の field は無視（True 扱いで他条件の評価を妨げない）。
        _ => true,
    }
}

/// 集合系（タグ・シリーズ・種類）の include/exclude 判定。
fn set_op(op: &str, has: bool) -> bool {
    match op {
        "exclude" | "not_contains" | "is_not" => !has,
        _ => has, // include / contains / is（既定）
    }
}

fn file_has_tag(tags: &[Tag], kind: Option<TagKind>, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    tags.iter().any(|t| {
        t.name.eq_ignore_ascii_case(name) && kind.is_none_or(|k| t.kind == k)
    })
}

/// 文字列（名前）に対する text 系演算。
fn text_op(haystack: &str, op: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let h = haystack.to_lowercase();
    let n = needle.to_lowercase();
    match op {
        "is" => h == n,
        "is_not" => h != n,
        "not_contains" => !h.contains(&n),
        "starts_with" => h.starts_with(&n),
        "ends_with" => h.ends_with(&n),
        _ => h.contains(&n), // contains（既定）
    }
}

/// 日付（作成日 / 更新日）に対する相対演算。`within`=N単位以内、`before`/`after`=
/// N単位より前/後。月・年は近似（30日 / 365日）。
fn date_op(
    dt: OffsetDateTime,
    op: &str,
    value: &str,
    unit: Option<&str>,
    now: OffsetDateTime,
) -> bool {
    let n: i64 = match value.trim().parse() {
        Ok(n) => n,
        Err(_) => return true, // 数値でなければ条件を無視
    };
    let days = match unit.unwrap_or("day") {
        "year" => n.saturating_mul(365),
        "month" => n.saturating_mul(30),
        _ => n,
    };
    let threshold = now - Duration::days(days);
    match op {
        "before" => dt < threshold,
        "after" => dt > threshold,
        _ => dt >= threshold, // within（既定）= 直近 N 単位以内
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteMetaStore;
    use yozist_core::{ActorId, Series, SeriesId, SeriesMember};

    async fn store() -> SqliteMetaStore {
        SqliteMetaStore::open_in_memory().await.unwrap()
    }

    fn file(name: &str, mime: &str) -> FileMeta {
        let now = OffsetDateTime::now_utc();
        FileMeta {
            id: FileId::new(),
            display_name: name.into(),
            size: 10,
            mime: Some(mime.into()),
            charset: None,
            current_commit: None,
            created_at: now,
            updated_at: now,
            deleted: false,
            created_by: None,
            updated_by: None,
            created_by_user_id: None,
            updated_by_user_id: None,
        }
    }

    async fn tag(s: &SqliteMetaStore, file: &FileId, name: &str, kind: TagKind) {
        let t = Tag {
            id: yozist_core::TagId::new(),
            name: name.into(),
            kind,
            confidence: None,
        };
        let id = s.upsert_tag(&t).await.unwrap();
        s.attach_tag(file, &id).await.unwrap();
    }

    fn cond(field: &str, op: &str, value: &str) -> QueryCondition {
        QueryCondition {
            field: field.into(),
            op: op.into(),
            value: value.into(),
            unit: None,
        }
    }

    #[tokio::test]
    async fn tag_kind_and_mime_conditions() {
        let s = store().await;
        let pdf = file("報告書.pdf", "application/pdf");
        let png = file("写真.png", "image/png");
        s.insert_file(&pdf).await.unwrap();
        s.insert_file(&png).await.unwrap();
        // タグ名は種別に依らず一意のため、種別ごとに別名で付与する。
        tag(&s, &pdf.id, "重要", TagKind::Manual).await;
        tag(&s, &png.id, "画像", TagKind::System).await;

        // 手動タグ「重要」を含む かつ 種類 pdf → pdf のみ。
        let q = QueryDef {
            match_mode: MatchMode::All,
            conditions: vec![
                cond("manual_tag", "include", "重要"),
                cond("mime", "include", "pdf"),
            ],
            ..Default::default()
        };
        let got = resolve_query(&s, &q).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].display_name, "報告書.pdf");

        // システムタグとして「重要」を探しても、それは手動タグなのでヒットしない。
        let q = QueryDef {
            conditions: vec![cond("system_tag", "include", "重要")],
            ..Default::default()
        };
        assert_eq!(resolve_query(&s, &q).await.unwrap().len(), 0);

        // システムタグ「画像」を含む → png のみ。
        let q = QueryDef {
            conditions: vec![cond("system_tag", "include", "画像")],
            ..Default::default()
        };
        let got = resolve_query(&s, &q).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].display_name, "写真.png");
    }

    #[tokio::test]
    async fn match_any_and_exclude_and_name() {
        let s = store().await;
        let a = file("draft-memo.txt", "text/plain");
        let b = file("final.pdf", "application/pdf");
        s.insert_file(&a).await.unwrap();
        s.insert_file(&b).await.unwrap();

        // いずれか: 名前に "draft" を含む or 種類 pdf → 両方。
        let q = QueryDef {
            match_mode: MatchMode::Any,
            conditions: vec![
                cond("name", "contains", "draft"),
                cond("mime", "include", "pdf"),
            ],
            ..Default::default()
        };
        assert_eq!(resolve_query(&s, &q).await.unwrap().len(), 2);

        // すべて: 種類 pdf を含まない かつ 名前 "memo" を含む → a のみ。
        let q = QueryDef {
            match_mode: MatchMode::All,
            conditions: vec![
                cond("mime", "exclude", "pdf"),
                cond("name", "contains", "memo"),
            ],
            ..Default::default()
        };
        let got = resolve_query(&s, &q).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].display_name, "draft-memo.txt");
    }

    #[tokio::test]
    async fn series_membership_condition() {
        let s = store().await;
        let inside = file("a.txt", "text/plain");
        let outside = file("b.txt", "text/plain");
        s.insert_file(&inside).await.unwrap();
        s.insert_file(&outside).await.unwrap();
        let series = Series {
            id: SeriesId::new(),
            name: "連載".into(),
            description: None,
        };
        let sid = s.upsert_series(&series).await.unwrap();
        s.add_to_series(&SeriesMember {
            series_id: sid,
            file_id: inside.id,
            order_index: 1.0,
        })
        .await
        .unwrap();

        let q = QueryDef {
            conditions: vec![cond("series", "include", "連載")],
            ..Default::default()
        };
        let got = resolve_query(&s, &q).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, inside.id);
    }

    #[tokio::test]
    async fn legacy_tags_still_work() {
        let s = store().await;
        let f = file("x.txt", "text/plain");
        s.insert_file(&f).await.unwrap();
        tag(&s, &f.id, "仕事", TagKind::Manual).await;
        let _ = ActorId::new();

        let q = QueryDef {
            tags_and: vec!["仕事".into()],
            ..Default::default()
        };
        assert_eq!(resolve_query(&s, &q).await.unwrap().len(), 1);

        let q = QueryDef {
            tags_not: vec!["仕事".into()],
            ..Default::default()
        };
        assert_eq!(resolve_query(&s, &q).await.unwrap().len(), 0);
    }
}
