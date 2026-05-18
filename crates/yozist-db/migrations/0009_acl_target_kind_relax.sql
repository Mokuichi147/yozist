-- acl_rules.target_type の CHECK 制約を緩めて任意文字列を許容する。
-- yozist-auth の Target を opaque (kind, ref_) 型に統一したため、
-- 認可ライブラリ側が固有のドメイン (file/tag/series/share/query) を
-- 知らずに済むようにする。
-- SQLite は CHECK 制約のみの DROP に対応していないのでテーブル再作成。
-- 0007 で expires_at が drop されている前提でカラム構成を組む。

CREATE TABLE acl_rules_new (
    id              TEXT PRIMARY KEY NOT NULL,
    subject_type    TEXT NOT NULL CHECK(subject_type IN ('user','group')),
    subject_id      TEXT NOT NULL,
    target_type     TEXT NOT NULL,
    target_ref      TEXT NOT NULL,
    permission_mask INTEGER NOT NULL,
    effect          TEXT NOT NULL CHECK(effect IN ('allow','deny')),
    priority        INTEGER NOT NULL DEFAULT 0
);

INSERT INTO acl_rules_new
    (id, subject_type, subject_id, target_type, target_ref, permission_mask, effect, priority)
SELECT id, subject_type, subject_id, target_type, target_ref, permission_mask, effect, priority
FROM acl_rules;

DROP TABLE acl_rules;
ALTER TABLE acl_rules_new RENAME TO acl_rules;

CREATE INDEX IF NOT EXISTS idx_acl_subject
    ON acl_rules(subject_type, subject_id);
CREATE INDEX IF NOT EXISTS idx_acl_target
    ON acl_rules(target_type, target_ref);
