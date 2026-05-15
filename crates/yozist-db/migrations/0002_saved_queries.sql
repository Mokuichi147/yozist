-- 保存クエリ（Shareable Path）
-- ユーザーが REST から「特定の条件にマッチするファイル群」を名前付きで保存し、
-- SMB の `\\host\queries\<name>\` でアクセス可能にする。

CREATE TABLE IF NOT EXISTS saved_queries (
    id              TEXT PRIMARY KEY NOT NULL,
    name            TEXT NOT NULL UNIQUE,
    query_json      TEXT NOT NULL,                  -- {"tags_and": [...], "tags_not": [...]}
    description     TEXT,
    created_by      TEXT REFERENCES users(id) ON DELETE SET NULL,
    created_at      TEXT NOT NULL,
    expires_at      TEXT                            -- 期限付き発行用
);

CREATE INDEX IF NOT EXISTS idx_saved_queries_name ON saved_queries(name);
CREATE INDEX IF NOT EXISTS idx_saved_queries_expires ON saved_queries(expires_at);
