-- ゴミ箱機能: 論理削除された日時を保持する列。
-- `deleted = 1` のファイルが「いつ削除されたか」を表示・並び替えするために使う。
-- NULL は未削除、または削除時刻が記録されていない旧データ（遡及バックフィルはしない）。
ALTER TABLE files ADD COLUMN deleted_at TEXT;

-- ゴミ箱一覧は deleted_at の新しい順で引くため索引を張る。
CREATE INDEX IF NOT EXISTS idx_files_deleted_at ON files(deleted_at DESC);
