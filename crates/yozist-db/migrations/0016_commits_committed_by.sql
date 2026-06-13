-- コミットを実行したユーザー名ラベル。
-- CRDT マージ用の commits.actor (ランダム ActorId) とは別物で、「誰が変更したか」の表示・監査用。
-- ユーザー削除後も名前が残るよう ID ではなくラベルを保存する（files.created_by/updated_by と同方針）。
-- NULL は記録なし（旧データ・SMB 経由の書き込みなど）。遡及バックフィルは行わない。
ALTER TABLE commits ADD COLUMN committed_by TEXT;
