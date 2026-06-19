-- シリーズごとの並び順設定。
-- 値: 'created_asc'(既定) | 'created_desc' | 'name_asc' | 'name_desc' | 'manual'
-- 既存シリーズは登録日時の昇順をデフォルトとする。
ALTER TABLE series ADD COLUMN sort_order TEXT NOT NULL DEFAULT 'created_asc';
