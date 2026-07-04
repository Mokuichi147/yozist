-- コミットの差分保存（issue #10）。delta_base が NULL のコミットはフルスナップ
-- ショット（従来通り blob = 完全な内容）。NULL でない場合、blob には delta_base
-- が指すコミットの内容を zstd 辞書として圧縮した差分（パッチ）が入っており、
-- 復元には基準コミットの内容が必要。基準は常に同一ファイル内のコミットを指す
-- ため、files 削除時の commits カスケード削除で鎖が壊れることはない。
-- 既存行は NULL = フルスナップショットのままで互換。
ALTER TABLE commits ADD COLUMN delta_base TEXT;
