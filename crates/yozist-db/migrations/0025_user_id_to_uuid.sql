-- user-permission-core 0.4.0 で User.id が i64 → UUID v7 (uuid::Uuid) に変更されたのに伴い、
-- yozist 側で「内部追跡用の不変キー」として保持している *_user_id / filters.created_by 列を
-- INTEGER から TEXT (UUID文字列) へ変更する。
-- SQLite は列型変更に対応しないためテーブル再作成で行う。開発中の DB につき値の移行は行わない
-- (適用前に data/auth.db, data/yozist.sqlite を削除してクリーンな状態から作り直す運用とする)。

-- 1) files.created_by_user_id / updated_by_user_id
CREATE TABLE files_new (
    id                  TEXT PRIMARY KEY NOT NULL,
    display_name        TEXT NOT NULL,
    size                INTEGER NOT NULL DEFAULT 0,
    mime                TEXT,
    current_commit      TEXT REFERENCES commits(id) ON DELETE SET NULL,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    deleted             INTEGER NOT NULL DEFAULT 0,
    version             INTEGER NOT NULL DEFAULT 0,
    charset             TEXT,
    created_by          TEXT,
    updated_by          TEXT,
    created_by_user_id  TEXT,
    updated_by_user_id  TEXT,
    deleted_at          TEXT
);

INSERT INTO files_new
    (id, display_name, size, mime, current_commit, created_at, updated_at,
     deleted, version, charset, created_by, updated_by,
     created_by_user_id, updated_by_user_id, deleted_at)
SELECT id, display_name, size, mime, current_commit, created_at, updated_at,
       deleted, version, charset, created_by, updated_by,
       NULL, NULL, deleted_at
FROM files;

DROP TABLE files;
ALTER TABLE files_new RENAME TO files;

CREATE INDEX IF NOT EXISTS idx_files_updated_at ON files(updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_files_deleted ON files(deleted);
CREATE INDEX IF NOT EXISTS idx_files_deleted_at ON files(deleted_at DESC);

-- 2) commits.committed_by_user_id
CREATE TABLE commits_new (
    id                   TEXT PRIMARY KEY NOT NULL,
    file_id              TEXT NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    parent               TEXT REFERENCES commits(id),
    actor                TEXT NOT NULL,
    blob                 TEXT NOT NULL,
    format_id            TEXT NOT NULL,
    timestamp            TEXT NOT NULL,
    message              TEXT,
    committed_by         TEXT,
    committed_by_user_id TEXT,
    size                 INTEGER NOT NULL DEFAULT 0,
    delta_base           TEXT
);

INSERT INTO commits_new
    (id, file_id, parent, actor, blob, format_id, timestamp, message,
     committed_by, committed_by_user_id, size, delta_base)
SELECT id, file_id, parent, actor, blob, format_id, timestamp, message,
       committed_by, NULL, size, delta_base
FROM commits;

DROP TABLE commits;
ALTER TABLE commits_new RENAME TO commits;

CREATE INDEX IF NOT EXISTS idx_commits_file ON commits(file_id, timestamp DESC);

-- 3) filters.created_by
CREATE TABLE filters_new (
    id              TEXT PRIMARY KEY NOT NULL,
    name            TEXT NOT NULL UNIQUE,
    definition_json TEXT NOT NULL,
    description     TEXT,
    created_by      TEXT,
    created_at      TEXT NOT NULL,
    expires_at      TEXT
);

INSERT INTO filters_new
    (id, name, definition_json, description, created_by, created_at, expires_at)
SELECT id, name, definition_json, description, NULL, created_at, expires_at
FROM filters;

DROP TABLE filters;
ALTER TABLE filters_new RENAME TO filters;

CREATE INDEX IF NOT EXISTS idx_filters_name ON filters(name);
CREATE INDEX IF NOT EXISTS idx_filters_expires ON filters(expires_at);
