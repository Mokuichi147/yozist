CREATE TABLE jobs (
    id           TEXT PRIMARY KEY,
    kind         TEXT NOT NULL,
    dedup_key    TEXT,
    payload      TEXT NOT NULL,
    status       TEXT NOT NULL,
    attempts     INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL DEFAULT 3,
    run_after    TEXT NOT NULL,
    error        TEXT,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

-- 未完了 (pending/running) の間だけ重複投入を防ぐ。done/failed に遷移した行は
-- インデックス対象から外れるため、同じ dedup_key で再度 enqueue できる
-- （cache-warm の再試行、cache-regenerate の強制再生成に必要）。
CREATE UNIQUE INDEX idx_jobs_dedup ON jobs(kind, dedup_key)
    WHERE dedup_key IS NOT NULL AND status IN ('pending', 'running');
CREATE INDEX idx_jobs_poll ON jobs(status, run_after);
