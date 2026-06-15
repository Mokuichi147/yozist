-- 「保存クエリ」を「フィルター」に改称（負の遺産を残さないため概念ごと統一）。
-- テーブル saved_queries → filters、列 query_json → definition_json、索引も改称する。
-- SMB 上は `\\host\yozist\filters\<name>\` でアクセスする。

ALTER TABLE saved_queries RENAME TO filters;
ALTER TABLE filters RENAME COLUMN query_json TO definition_json;

DROP INDEX IF EXISTS idx_saved_queries_name;
DROP INDEX IF EXISTS idx_saved_queries_expires;
CREATE INDEX IF NOT EXISTS idx_filters_name ON filters(name);
CREATE INDEX IF NOT EXISTS idx_filters_expires ON filters(expires_at);
