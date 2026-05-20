-- ACL ルールから期限機能を削除。
-- 現状コード上で常に NULL がセットされており、発行する経路も無いため。
-- 将来必要になった場合は別途追加マイグレーションで復活させる。

ALTER TABLE acl_rules DROP COLUMN expires_at;
