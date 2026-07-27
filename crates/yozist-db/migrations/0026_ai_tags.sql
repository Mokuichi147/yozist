-- AI 自動タグ生成の記録。
--
-- 生成結果そのもの（タグ）は手動タグと同じ tags / file_tags に載せる。AI タグは
-- 1 枚あたり LLM 推論 20 秒級のコストで作られるユーザーデータであり、消えても
-- 作り直せるプレビューキャッシュとは性質が違うため、キャッシュ DB ではなく
-- メタ DB に置く。ここで足すのは「誰が付けたか」「どのモデルで付けたか」という
-- tags / file_tags が持てない情報だけ。

-- ファイル単位の生成記録。モデルを差し替えた時に付け直し対象を列挙するために
-- 使う。1 ファイルにつき最新の 1 回だけを保持する（履歴は監査ログ側の責務）。
CREATE TABLE IF NOT EXISTS ai_tag_runs (
    file_id    TEXT PRIMARY KEY NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    commit_id  TEXT NOT NULL,
    model      TEXT NOT NULL,
    status     TEXT NOT NULL CHECK(status IN ('pending','ready','failed')),
    error      TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_ai_tag_runs_model ON ai_tag_runs(model);

-- AI が付与したタグの所有権。file_tags は付与者を持たないため、この表が無いと
-- 再生成時に「AI が付けた分だけ外す」ことができず、手動タグまで巻き込む。
-- raw_name は narashi による正規化前の LLM 出力そのままで、表記ゆれの寄せ具合を
-- 後から確認するために残す。
CREATE TABLE IF NOT EXISTS ai_file_tags (
    file_id    TEXT NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    tag_id     TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    model      TEXT NOT NULL,
    raw_name   TEXT NOT NULL,
    confidence REAL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (file_id, tag_id)
);
CREATE INDEX IF NOT EXISTS idx_ai_file_tags_tag ON ai_file_tags(tag_id);
