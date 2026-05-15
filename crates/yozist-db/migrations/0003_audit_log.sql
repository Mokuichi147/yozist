-- 監査ログ: 誰がいつ何を行ったかを追跡
-- result列で成功/失敗を識別。target_typeでスコープを分類。

CREATE TABLE IF NOT EXISTS audit_log (
    id              TEXT PRIMARY KEY NOT NULL,
    timestamp       TEXT NOT NULL,
    actor_id        TEXT,           -- UserId or null (system/anonymous)
    actor_label     TEXT,           -- "alice" / "anonymous" / "system" / "smb:bob"
    action          TEXT NOT NULL,  -- create_file, commit, delete_file, attach_tag, detach_tag, ...
    target_type     TEXT,           -- file | tag | series | query | user | acl | share_token
    target_ref      TEXT,
    metadata_json   TEXT,
    result          TEXT NOT NULL   -- "ok" or "error: ..."
);

CREATE INDEX IF NOT EXISTS idx_audit_timestamp ON audit_log(timestamp DESC);
CREATE INDEX IF NOT EXISTS idx_audit_actor ON audit_log(actor_id, timestamp DESC);
CREATE INDEX IF NOT EXISTS idx_audit_target ON audit_log(target_type, target_ref);
