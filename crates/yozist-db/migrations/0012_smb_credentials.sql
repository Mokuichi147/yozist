-- SMB (NTLMv2) 認証用の NT ハッシュを保持する。
--
-- user-permission (auth.db) はパスワードを Argon2id で保存しており、これは
-- 一方向ハッシュのため NTLM のチャレンジ応答には使えない。NTLM はサーバ側に
-- NT ハッシュ (MD4(UTF-16LE(password))) を要求する。平文パスワードを観測できる
-- のは REST 認証経路 (register / login / change_password) だけなので、そこで
-- NT ハッシュを導出して本テーブルに保存し、サーバ起動時に SMB のユーザー
-- テーブルへ復元する。これによりサーバ再起動後もログイン無しで SMB 接続できる。
--
-- 注意: NT ハッシュは無塩 MD4 でパスワード等価の資格情報である。NTLM を使う
-- 以上これは不可避だが、auth.db の Argon2 ハッシュとは別物として扱う。
CREATE TABLE IF NOT EXISTS smb_credentials (
    username    TEXT PRIMARY KEY NOT NULL,
    nt_hash     BLOB NOT NULL,
    updated_at  TEXT NOT NULL
);
