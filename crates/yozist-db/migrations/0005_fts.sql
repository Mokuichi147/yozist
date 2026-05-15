-- 全文検索: FTS5 仮想テーブル
-- display_name + タグ名 + (テキストファイルなら) 内容を索引化。
-- アプリ層から upsert/delete する想定 (トリガでなく明示制御)。

CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(
    file_id UNINDEXED,
    display_name,
    tags,
    content,
    tokenize = 'unicode61'
);
