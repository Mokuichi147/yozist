-- 全文検索を trigram トークナイザへ移行。
-- unicode61 は CJK を単語分割できないため、日本語本文は文全体が 1 トークンになり
-- 部分一致検索が常に 0 件になっていた。trigram なら 3 文字以上の部分文字列で
-- 一致でき、3 文字未満はアプリ層（search_fts）が LIKE へフォールバックする。
-- FTS5 はトークナイザを後から変更できないので、作り直してデータを移す
-- （FTS5 テーブルは元テキストを保持しているので SELECT でそのまま移せる）。

ALTER TABLE files_fts RENAME TO files_fts_old;

CREATE VIRTUAL TABLE files_fts USING fts5(
    file_id UNINDEXED,
    display_name,
    tags,
    content,
    tokenize = 'trigram'
);

INSERT INTO files_fts (file_id, display_name, tags, content)
    SELECT file_id, display_name, tags, content FROM files_fts_old;

DROP TABLE files_fts_old;
