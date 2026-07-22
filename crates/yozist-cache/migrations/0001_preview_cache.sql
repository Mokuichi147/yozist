CREATE TABLE preview_cache (
    file_id     TEXT NOT NULL,
    commit_id   TEXT NOT NULL,
    variant     TEXT NOT NULL,
    status      TEXT NOT NULL,
    rel_path    TEXT,
    mime        TEXT,
    byte_size   INTEGER,
    width       INTEGER,
    height      INTEGER,
    error       TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (file_id, commit_id, variant)
);

CREATE INDEX idx_preview_cache_file ON preview_cache(file_id);
