-- 実行ユーザーの不変キー（users.id）。表示用ラベル（committed_by / created_by /
-- updated_by）とは別に、改名・同名再登録に強い内部追跡のため保持する。
-- 表示はしない（API/UI には出さない）。FK ではなく値として持ち、ユーザー削除後も
-- 履歴行は残る（名前は対応するラベル列が担保する）。NULL は記録なし
-- （旧データ・SMB/匿名）。遡及バックフィルは行わない。
ALTER TABLE commits ADD COLUMN committed_by_user_id INTEGER;
ALTER TABLE files ADD COLUMN created_by_user_id INTEGER;
ALTER TABLE files ADD COLUMN updated_by_user_id INTEGER;
