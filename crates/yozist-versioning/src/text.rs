//! テキストの文字コード（charset）判定・変換ユーティリティ。
//!
//! # 方針
//! CRDT（yrs）は内部表現が UTF-8 文字列のため、編集・マージは必ず UTF-8 を
//! 経由する。そこで「取り込み時に元エンコーディングを判定して UTF-8 へデコード」
//! し、blob には UTF-8 平文を保存する。元 charset は `FileMeta.charset` に記録し、
//! ダウンロードや SMB read の際に [`encode_text`] で元の形式へ再エンコードする。
//!
//! # 判定ロジック
//! 1. 純 ASCII / 空 → `UTF-8`（windows-1252 等への誤判定を避ける）
//! 2. BOM があれば最優先（`UTF-8-BOM` / `UTF-16LE` / `UTF-16BE`）
//! 3. それ以外は `chardetng` で推定（Shift-JIS / EUC-JP / windows-1252 等）
//!
//! chardetng は UTF-16 を判定しないため、BOM 無し UTF-16 は対象外（範囲外）。

/// UTF-8 BOM。
const BOM_UTF8: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// 全バイトが ASCII（0x00–0x7F）か。
fn is_ascii(bytes: &[u8]) -> bool {
    bytes.iter().all(|b| *b < 0x80)
}

/// バイト列のエンコーディングを推定する（BOM → chardetng の順）。
/// 戻り値は `encoding_rs` の静的エンコーディング参照。
fn sniff(bytes: &[u8]) -> &'static encoding_rs::Encoding {
    if let Some((enc, _bom_len)) = encoding_rs::Encoding::for_bom(bytes) {
        return enc;
    }
    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(bytes, true);
    // tld ヒント無し・UTF-8 を許可。
    detector.guess(None, true)
}

/// 元エンコーディングを判定し、保存用の charset ラベルを返す。
///
/// 返すラベルは [`encode_text`] が解釈できる形式:
/// `encoding_rs` の正規名（例 `"Shift_JIS"`, `"EUC-JP"`, `"windows-1252"`,
/// `"UTF-16LE"`, `"UTF-16BE"`, `"UTF-8"`）に加え、UTF-8 BOM 付きを表す
/// 独自ラベル `"UTF-8-BOM"`。
pub fn detect_charset(bytes: &[u8]) -> String {
    if is_ascii(bytes) {
        // 空ファイルや純 ASCII は UTF-8 として扱う（再エンコードは恒等）。
        return "UTF-8".to_string();
    }
    if let Some((enc, _)) = encoding_rs::Encoding::for_bom(bytes) {
        if enc == encoding_rs::UTF_8 {
            return "UTF-8-BOM".to_string();
        }
        // UTF-16LE / UTF-16BE。
        return enc.name().to_string();
    }
    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(bytes, true);
    detector.guess(None, true).name().to_string()
}

/// バイト列をテキストとして UTF-8 文字列へデコードする。
///
/// BOM があれば尊重し（`encoding_rs::decode` が BOM を除去）、無ければ
/// `chardetng` の推定エンコーディングでデコードする。不正シーケンスは
/// U+FFFD に置換され、エラーにはしない（「可能な範囲で幅広く受け入れる」方針）。
pub fn decode_text(bytes: &[u8]) -> String {
    let enc = sniff(bytes);
    // decode は先頭 BOM を検出した場合 enc を上書きし、BOM を除去する。
    enc.decode(bytes).0.into_owned()
}

/// UTF-8 文字列を指定 charset へ再エンコードする。
///
/// `encoding_rs` のエンコーダは仕様上 UTF-16 への出力を持たないため、
/// `UTF-16LE` / `UTF-16BE` は BOM 付きで手動エンコードする。`UTF-8-BOM` は
/// 先頭に UTF-8 BOM を付与する。未知ラベルや変換不能時は UTF-8 にフォールバック。
///
/// 注意: 元が Shift-JIS 等でも、編集で UTF-8 専用文字（絵文字等）が混入した
/// 場合、対象 charset で表現できない文字は数値文字参照や置換へ変換され、
/// 完全な往復にはならない（ユーザー了承済みの制約）。
pub fn encode_text(text: &str, charset: &str) -> Vec<u8> {
    match charset.to_ascii_lowercase().as_str() {
        "utf-8" => text.as_bytes().to_vec(),
        "utf-8-bom" => {
            let mut out = Vec::with_capacity(text.len() + BOM_UTF8.len());
            out.extend_from_slice(&BOM_UTF8);
            out.extend_from_slice(text.as_bytes());
            out
        }
        "utf-16le" => {
            let mut out = vec![0xFF, 0xFE]; // BOM (LE)
            for unit in text.encode_utf16() {
                out.extend_from_slice(&unit.to_le_bytes());
            }
            out
        }
        "utf-16be" => {
            let mut out = vec![0xFE, 0xFF]; // BOM (BE)
            for unit in text.encode_utf16() {
                out.extend_from_slice(&unit.to_be_bytes());
            }
            out
        }
        _ => {
            let enc = encoding_rs::Encoding::for_label(charset.as_bytes())
                .unwrap_or(encoding_rs::UTF_8);
            enc.encode(text).0.into_owned()
        }
    }
}

/// HTTP `Content-Type` ヘッダに載せる charset トークンを返す。
/// 独自ラベル `UTF-8-BOM` は `UTF-8` に正規化する（BOM はバイト列側で表現）。
pub fn http_charset(charset: &str) -> &str {
    charset.strip_suffix("-BOM").unwrap_or(charset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_utf8() {
        assert_eq!(detect_charset(b"hello"), "UTF-8");
        assert_eq!(detect_charset(b""), "UTF-8");
        assert_eq!(decode_text(b"hello"), "hello");
    }

    #[test]
    fn shift_jis_roundtrip() {
        let text = "こんにちは世界";
        let (sjis, _, _) = encoding_rs::SHIFT_JIS.encode(text);
        assert!(sjis.iter().any(|b| *b >= 0x80), "Shift-JIS バイトのはず");
        // 判定
        let label = detect_charset(&sjis);
        assert_eq!(label, "Shift_JIS", "検出 charset");
        // デコード
        assert_eq!(decode_text(&sjis), text);
        // 元 charset へ再エンコードして往復一致
        assert_eq!(encode_text(text, &label), sjis.to_vec());
    }

    #[test]
    fn euc_jp_roundtrip() {
        let text = "日本語のテスト";
        let (euc, _, _) = encoding_rs::EUC_JP.encode(text);
        let label = detect_charset(&euc);
        assert_eq!(label, "EUC-JP");
        assert_eq!(decode_text(&euc), text);
        assert_eq!(encode_text(text, &label), euc.to_vec());
    }

    #[test]
    fn utf16le_bom_roundtrip() {
        let text = "あいう abc";
        let mut bytes = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(detect_charset(&bytes), "UTF-16LE");
        assert_eq!(decode_text(&bytes), text);
        assert_eq!(encode_text(text, "UTF-16LE"), bytes);
        assert_eq!(http_charset("UTF-16LE"), "UTF-16LE");
    }

    #[test]
    fn utf16be_bom_roundtrip() {
        let text = "テストΩ";
        let mut bytes = vec![0xFE, 0xFF];
        for u in text.encode_utf16() {
            bytes.extend_from_slice(&u.to_be_bytes());
        }
        assert_eq!(detect_charset(&bytes), "UTF-16BE");
        assert_eq!(decode_text(&bytes), text);
        assert_eq!(encode_text(text, "UTF-16BE"), bytes);
    }

    #[test]
    fn utf8_bom_preserved() {
        let text = "日本語";
        let mut bytes = BOM_UTF8.to_vec();
        bytes.extend_from_slice(text.as_bytes());
        assert_eq!(detect_charset(&bytes), "UTF-8-BOM");
        // BOM は decode で除去される
        assert_eq!(decode_text(&bytes), text);
        // 再エンコードで BOM が復元される
        assert_eq!(encode_text(text, "UTF-8-BOM"), bytes);
        // HTTP ヘッダ用は素の UTF-8
        assert_eq!(http_charset("UTF-8-BOM"), "UTF-8");
    }

    #[test]
    fn utf8_multibyte_detected_as_utf8() {
        let text = "絵文字🎉と日本語";
        let label = detect_charset(text.as_bytes());
        assert_eq!(label, "UTF-8");
        assert_eq!(decode_text(text.as_bytes()), text);
    }

    #[test]
    fn unknown_label_falls_back_to_utf8() {
        assert_eq!(encode_text("hi", "no-such-charset"), b"hi".to_vec());
    }
}
