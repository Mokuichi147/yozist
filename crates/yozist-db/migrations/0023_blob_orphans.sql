-- 参照を失った可能性のある blob の削除候補キュー（issue #10 の逆デルタ化・GC 用）。
-- デルタ再符号化で置き換えられた旧 blob や、ファイル完全削除で参照が消えた blob を
-- 候補として登録し、猶予期間の経過後にスイーパが「commits から本当に参照されて
-- いない」ことを再確認してから実体を削除する。候補はあくまでヒントであり、
-- 登録されたまま参照が残っていても実体は消されない（スイーパ側で再検証する）。
CREATE TABLE IF NOT EXISTS blob_orphans (
    blob_id     TEXT PRIMARY KEY NOT NULL,
    orphaned_at TEXT NOT NULL
);
