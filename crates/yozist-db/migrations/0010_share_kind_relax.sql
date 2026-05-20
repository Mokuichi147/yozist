-- share_tokens.kind の CHECK 制約 (IN ('file','query')) を緩めて
-- 任意文字列を許容する。yozist-auth の汎用化に合わせる。
-- SQLite は CHECK 制約のみの DROP 不可なのでテーブル再作成。

CREATE TABLE share_tokens_new (
    jti         TEXT PRIMARY KEY NOT NULL,
    kind        TEXT NOT NULL,
    target_id   TEXT NOT NULL,
    issuer      TEXT,
    issued_at   TEXT NOT NULL,
    expires_at  TEXT,
    revoked_at  TEXT
);

INSERT INTO share_tokens_new
    (jti, kind, target_id, issuer, issued_at, expires_at, revoked_at)
SELECT jti, kind, target_id, issuer, issued_at, expires_at, revoked_at
FROM share_tokens;

DROP TABLE share_tokens;
ALTER TABLE share_tokens_new RENAME TO share_tokens;

CREATE INDEX IF NOT EXISTS idx_share_tokens_kind_target
    ON share_tokens(kind, target_id);
CREATE INDEX IF NOT EXISTS idx_share_tokens_issuer
    ON share_tokens(issuer);
