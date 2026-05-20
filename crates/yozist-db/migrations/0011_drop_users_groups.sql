-- ユーザー/グループ管理を upstream `user-permission` に委譲したため、
-- yozist.db からは関連テーブルを削除する。認証用データは別 SQLite ファイル
-- (例: auth.db) に格納される。
--
-- saved_queries.created_by は元々 users(id) への FK だったが、テーブル削除に
-- 伴い FK が無効化される。値の型も UUID(TEXT) から user-permission の i64 に
-- 変わるが、SQLite では TEXT/INTEGER に互換性があるので既存カラム型 TEXT を
-- そのまま使い、アプリ側で i64 として読み書きする (sqlx は TEXT カラムから
-- i64 を読めない場合があるため、INTEGER に再定義する)。
-- audit_log.actor_id も従来 UUID 文字列を保存していたが、以降は i64 を文字列化
-- して保存する。データ型 TEXT のまま運用可能なのでカラム変更は不要。

-- 1) saved_queries.created_by を TEXT FK から INTEGER に変更する。
--    SQLite の ALTER TABLE は FK 削除や型変更に対応しないためテーブル再作成。
CREATE TABLE saved_queries_new (
    id              TEXT PRIMARY KEY NOT NULL,
    name            TEXT NOT NULL UNIQUE,
    query_json      TEXT NOT NULL,
    description     TEXT,
    created_by      INTEGER,
    created_at      TEXT NOT NULL,
    expires_at      TEXT
);

-- 既存データの created_by (UUID 文字列) は新 DB の user.id に対応しないため NULL に倒す。
INSERT INTO saved_queries_new
    (id, name, query_json, description, created_by, created_at, expires_at)
SELECT id, name, query_json, description, NULL, created_at, expires_at
FROM saved_queries;

DROP TABLE saved_queries;
ALTER TABLE saved_queries_new RENAME TO saved_queries;

CREATE INDEX IF NOT EXISTS idx_saved_queries_name ON saved_queries(name);
CREATE INDEX IF NOT EXISTS idx_saved_queries_expires ON saved_queries(expires_at);

-- 2) ユーザー・グループ系テーブルを削除。
DROP TABLE IF EXISTS user_groups;
DROP TABLE IF EXISTS groups;
DROP TABLE IF EXISTS users;
