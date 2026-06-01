-- テキストファイルの元エンコーディング（charset ラベル）を保持する列。
-- CRDT/blob は常に UTF-8 で保存し、ダウンロードや SMB read の際に
-- この charset へ再エンコードして「元の形式」で返すために使う。
-- 例: "Shift_JIS", "EUC-JP", "UTF-16LE", "UTF-16BE", "UTF-8", "UTF-8-BOM"。
-- NULL はバイナリ（LWW）または charset 未判定。
ALTER TABLE files ADD COLUMN charset TEXT;
