-- yozist 初期スキーマ
-- 設計原則: ファイルは FileId で一元管理。物理パスは持たない。
-- すべての時刻は UTC ISO8601 (TEXT) で保存（time クレートと互換）。

CREATE TABLE IF NOT EXISTS files (
    id              TEXT PRIMARY KEY NOT NULL,           -- UUIDv7
    display_name    TEXT NOT NULL,
    size            INTEGER NOT NULL DEFAULT 0,
    mime            TEXT,
    current_commit  TEXT REFERENCES commits(id) ON DELETE SET NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    deleted         INTEGER NOT NULL DEFAULT 0,
    version         INTEGER NOT NULL DEFAULT 0           -- 楽観ロック用
);

CREATE INDEX IF NOT EXISTS idx_files_updated_at ON files(updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_files_deleted ON files(deleted);

CREATE TABLE IF NOT EXISTS tags (
    id          TEXT PRIMARY KEY NOT NULL,
    name        TEXT NOT NULL UNIQUE,
    kind        TEXT NOT NULL CHECK(kind IN ('system','ai','manual')),
    confidence  REAL                                     -- AI 推測のみ
);

CREATE INDEX IF NOT EXISTS idx_tags_kind ON tags(kind);

CREATE TABLE IF NOT EXISTS file_tags (
    file_id TEXT NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    tag_id  TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (file_id, tag_id)
);

CREATE INDEX IF NOT EXISTS idx_file_tags_tag ON file_tags(tag_id);

CREATE TABLE IF NOT EXISTS series (
    id          TEXT PRIMARY KEY NOT NULL,
    name        TEXT NOT NULL UNIQUE,
    description TEXT
);

CREATE TABLE IF NOT EXISTS series_members (
    series_id   TEXT NOT NULL REFERENCES series(id) ON DELETE CASCADE,
    file_id     TEXT NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    order_index REAL NOT NULL,                           -- f64 で中間挿入容易
    PRIMARY KEY (series_id, file_id)
);

CREATE INDEX IF NOT EXISTS idx_series_members_order
    ON series_members(series_id, order_index);

CREATE TABLE IF NOT EXISTS commits (
    id          TEXT PRIMARY KEY NOT NULL,
    file_id     TEXT NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    parent      TEXT REFERENCES commits(id),
    actor       TEXT NOT NULL,
    blob        TEXT NOT NULL,
    format_id   TEXT NOT NULL,
    timestamp   TEXT NOT NULL,
    message     TEXT
);

CREATE INDEX IF NOT EXISTS idx_commits_file ON commits(file_id, timestamp DESC);

CREATE TABLE IF NOT EXISTS blob_refs (
    blob_id     TEXT NOT NULL,
    file_id     TEXT NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    PRIMARY KEY (blob_id, file_id)
);

-- ACL ルール（権限・パス発行）
CREATE TABLE IF NOT EXISTS acl_rules (
    id              TEXT PRIMARY KEY NOT NULL,
    subject_type    TEXT NOT NULL CHECK(subject_type IN ('user','group')),
    subject_id      TEXT NOT NULL,
    target_type     TEXT NOT NULL CHECK(target_type IN ('share','tag','series','file','query')),
    target_ref      TEXT NOT NULL,                       -- 文字列 or JSON
    permission_mask INTEGER NOT NULL,                    -- bit: view=1 read=2 write=4 admin=8
    effect          TEXT NOT NULL CHECK(effect IN ('allow','deny')),
    priority        INTEGER NOT NULL DEFAULT 0,
    expires_at      TEXT
);

CREATE INDEX IF NOT EXISTS idx_acl_subject
    ON acl_rules(subject_type, subject_id);
CREATE INDEX IF NOT EXISTS idx_acl_target
    ON acl_rules(target_type, target_ref);

-- ユーザー・グループ（yozist-auth と共有）
CREATE TABLE IF NOT EXISTS users (
    id              TEXT PRIMARY KEY NOT NULL,
    username        TEXT NOT NULL UNIQUE,
    display_name    TEXT,
    password_hash   TEXT NOT NULL,
    is_active       INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS groups (
    id              TEXT PRIMARY KEY NOT NULL,
    name            TEXT NOT NULL UNIQUE,
    description     TEXT
);

CREATE TABLE IF NOT EXISTS user_groups (
    user_id   TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    group_id  TEXT NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    PRIMARY KEY (user_id, group_id)
);
