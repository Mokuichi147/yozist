-- 共有URLトークンの管理。JWTの jti クレームで本テーブルを参照する。
-- 失効や使用履歴を追跡し、revoked_at が non-NULL であれば失効扱い。

CREATE TABLE IF NOT EXISTS share_tokens (
    jti         TEXT PRIMARY KEY NOT NULL,    -- UUIDv7
    kind        TEXT NOT NULL CHECK(kind IN ('file','query')),
    target_id   TEXT NOT NULL,                -- FileId / SavedQueryId
    issuer      TEXT,                          -- 発行ユーザー名
    issued_at   TEXT NOT NULL,
    expires_at  TEXT,
    revoked_at  TEXT                           -- non-NULL = 失効
);

CREATE INDEX IF NOT EXISTS idx_share_tokens_kind_target
    ON share_tokens(kind, target_id);
CREATE INDEX IF NOT EXISTS idx_share_tokens_issuer
    ON share_tokens(issuer);
