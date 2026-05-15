-- グループの親子関係を追加。
-- ユーザーが group A に所属し、group A の親が group B なら、
-- ACL 評価では group A と group B の両方が subject として扱われる。

ALTER TABLE groups ADD COLUMN parent_group_id TEXT REFERENCES groups(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_groups_parent ON groups(parent_group_id);
