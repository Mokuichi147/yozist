-- グループ階層 (parent_group_id) 機能を削除。
-- 0006_group_hierarchy.sql で導入したが、実際に階層を作成する経路は
-- 限定的で、本番運用上のユースケースが無いため簡素化する。

DROP INDEX IF EXISTS idx_groups_parent;
ALTER TABLE groups DROP COLUMN parent_group_id;
