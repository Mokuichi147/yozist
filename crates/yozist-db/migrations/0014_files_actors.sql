-- ファイルの作成者・最終更新者（ユーザー名ラベル）。
-- 一覧・詳細での表示用。ユーザー削除後も名前が残るよう ID ではなくラベルを保存する。
-- NULL は記録なし（旧データ・SMB 経由の書き込みなど）。
ALTER TABLE files ADD COLUMN created_by TEXT;
ALTER TABLE files ADD COLUMN updated_by TEXT;
