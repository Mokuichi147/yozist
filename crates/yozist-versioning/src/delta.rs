//! コミット間の差分（デルタ）符号化。
//!
//! 前コミットの全文を zstd の辞書として新しい全文を圧縮する
//! （`zstd --patch-from` と同じ仕組み）。変更が小さいほどパッチは小さくなり、
//! テキスト・バイナリを問わず同一の機構で扱える。復元には基準（辞書）となる
//! 内容がそのまま必要なので、`VersioningEngine` はスナップショットから
//! デルタ鎖を順に適用して内容を再構成する。

use crate::VersioningError;

/// デルタとして保存する鎖の最大長。この長さに達したらフルスナップショットを
/// 保存して鎖を打ち切る。復元コストは「スナップショット読込 + 最大この回数の
/// パッチ適用」で抑えられる。
pub const SNAPSHOT_INTERVAL: usize = 8;

/// デルタ符号化を試みる内容サイズの上限（基準・対象それぞれ）。これを超える
/// 場合はフルスナップショットで保存する。zstd の辞書参照はウィンドウサイズに
/// 制限されるため、上限に合わせて `WindowLog` を固定している（下記 23 = 8 MiB）。
pub const DELTA_MAX_LEN: usize = 8 * 1024 * 1024;

/// 上限サイズ全体を辞書参照できるウィンドウ指数（2^23 = 8 MiB）。
const WINDOW_LOG: u32 = 23;

/// 圧縮レベル。blob 保存時の zstd と同程度の軽さを保つ。
const LEVEL: i32 = 3;

/// `base` を辞書に `target` を圧縮したパッチを返す。
///
/// 以下の場合は「デルタ保存に値しない」として `None` を返し、呼び出し側は
/// フルスナップショットへフォールバックする:
/// - どちらかが `DELTA_MAX_LEN` を超える（メモリ・復元コストの上限）
/// - パッチが `target` の通常圧縮より小さくならない（デルタの利得なし）
/// - zstd がエラーを返した
///
/// 返したパッチは必ず [`decode`] で `target` に復元できることを検証済み。
pub fn encode(base: &[u8], target: &[u8]) -> Option<Vec<u8>> {
    use zstd::zstd_safe::CParameter;

    if base.is_empty() || base.len() > DELTA_MAX_LEN || target.len() > DELTA_MAX_LEN {
        return None;
    }

    let mut enc = zstd::bulk::Compressor::new(LEVEL).ok()?;
    // 辞書ロード前にウィンドウを確定する（辞書は設定時のパラメータで処理される）。
    enc.set_parameter(CParameter::WindowLog(WINDOW_LOG)).ok()?;
    enc.set_parameter(CParameter::EnableLongDistanceMatching(true))
        .ok()?;
    enc.set_dictionary(LEVEL, base).ok()?;
    let patch = enc.compress(target).ok()?;

    // 利得判定: blob 層は保存時に通常の zstd 圧縮を行うため、比較対象は
    // 生サイズではなく「通常圧縮したフル内容」。それより小さい時だけ採用する。
    let plain = zstd::bulk::compress(target, LEVEL).ok()?;
    if patch.len() >= plain.len() {
        return None;
    }

    // 保存前にラウンドトリップを検証し、復元不能なパッチを決して残さない。
    match decode(base, &patch) {
        Ok(restored) if restored == target => Some(patch),
        _ => None,
    }
}

/// `base` を辞書に `patch` を伸長し、元の内容を返す。
pub fn decode(base: &[u8], patch: &[u8]) -> Result<Vec<u8>, VersioningError> {
    use zstd::zstd_safe::DParameter;

    let mut dec = zstd::bulk::Decompressor::new()
        .map_err(|e| VersioningError::Conflict(format!("delta decoder: {e}")))?;
    dec.set_parameter(DParameter::WindowLogMax(WINDOW_LOG))
        .map_err(|e| VersioningError::Conflict(format!("delta window: {e}")))?;
    dec.set_dictionary(base)
        .map_err(|e| VersioningError::Conflict(format!("delta dict: {e}")))?;
    dec.decompress(patch, DELTA_MAX_LEN)
        .map_err(|e| VersioningError::Conflict(format!("delta decode: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_text_edit_produces_tiny_patch() {
        let base = "こんにちは世界。".repeat(1000);
        let mut target = base.clone();
        target.push_str("追記です。");
        let patch = encode(base.as_bytes(), target.as_bytes()).expect("delta");
        // 1 行追記のパッチはフル内容の通常圧縮よりはるかに小さい
        assert!(patch.len() < 200, "patch too big: {}", patch.len());
        let restored = decode(base.as_bytes(), &patch).unwrap();
        assert_eq!(restored, target.as_bytes());
    }

    #[test]
    fn binary_edit_roundtrips() {
        // 圧縮の効きにくい擬似ランダムバイナリの一部だけを書き換えるケース。
        let mut base = vec![0u8; 512 * 1024];
        let mut x: u32 = 12345;
        for b in base.iter_mut() {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (x >> 24) as u8;
        }
        let mut target = base.clone();
        target[1000..1032].copy_from_slice(&[0xAB; 32]);
        let patch = encode(&base, &target).expect("delta");
        assert!(patch.len() < target.len() / 10, "patch: {}", patch.len());
        assert_eq!(decode(&base, &patch).unwrap(), target);
    }

    #[test]
    fn oversize_falls_back_to_none() {
        let base = vec![1u8; DELTA_MAX_LEN + 1];
        let target = vec![2u8; 8];
        assert!(encode(&base, &target).is_none());
        assert!(encode(&target, &base).is_none());
    }

    #[test]
    fn empty_base_falls_back_to_none() {
        assert!(encode(b"", b"hello").is_none());
    }
}
