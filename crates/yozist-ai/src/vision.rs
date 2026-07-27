//! OpenAI 互換 `/chat/completions` を叩く vision タグ推測プロバイダ。
//!
//! 画像は data URI（`data:<mime>;base64,...`）として `image_url` に載せる。
//! 出力は `response_format: json_schema` の strict モードで固定し、パースが
//! 転ばないようにする（対応していないサーバでも、素の JSON が返れば通る）。

use async_trait::async_trait;
use base64::Engine as _;
use serde::Deserialize;
use std::time::Duration;

use crate::{AiError, AiProvider, TagSuggestion};

/// 生成タスクの指示。タグの言語を日本語に固定するのは、narashi の言語優先
/// （`Language::Japanese`）と揃えて表記ゆれの寄せ先を安定させるため。
const SYSTEM_PROMPT: &str = "あなたは画像にタグを付けるアシスタントです。\
     画像に実際に写っているもの・場所・構図・雰囲気を表すタグを、\
     すべて日本語（名詞または名詞句）で挙げてください。\
     推測に自信がないものは confidence を低くしてください。\
     JSON のみを出力し、説明文は書かないでください。";

pub struct OpenAiVisionProvider {
    http: reqwest::Client,
    /// `{base_url}/chat/completions` まで解決済みのエンドポイント。
    endpoint: String,
    model: String,
    api_key: Option<String>,
    max_tags: usize,
    /// `reasoning_effort` に載せる値。`None` ならフィールド自体を送らない。
    reasoning_effort: Option<String>,
}

impl OpenAiVisionProvider {
    /// `base_url` は `/v1` まで含む OpenAI 互換のベース URL。
    /// `api_key` はローカルサーバ相手なら `None` でよい（ヘッダを送らない）。
    /// `reasoning_effort` が `None`（または空）ならそのフィールドを送らない。
    pub fn new(
        base_url: &str,
        model: impl Into<String>,
        api_key: Option<String>,
        max_tags: usize,
        reasoning_effort: Option<String>,
        timeout: Duration,
    ) -> Result<Self, AiError> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| AiError::Permanent(format!("HTTP クライアントを作れません: {e}")))?;
        Ok(Self {
            http,
            endpoint: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            model: model.into(),
            api_key: api_key.filter(|k| !k.trim().is_empty()),
            max_tags,
            reasoning_effort: reasoning_effort.filter(|v| !v.trim().is_empty()),
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    fn request_body(&self, content: &[u8], mime: &str) -> serde_json::Value {
        let b64 = base64::engine::general_purpose::STANDARD.encode(content);
        let instruction = format!(
            "この画像を表すタグを日本語で最大 {} 個挙げ、JSON で出力してください。",
            self.max_tags
        );
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": SYSTEM_PROMPT },
                { "role": "user", "content": [
                    { "type": "text", "text": instruction },
                    { "type": "image_url",
                      "image_url": { "url": format!("data:{mime};base64,{b64}") } },
                ]},
            ],
            // タグ付けは創作ではないので出力を安定させる方に振る。
            "temperature": 0.2,
            // タグ 10 件の JSON 自体は 200 トークン程度だが、推論モデルは
            // reasoning に 300-700 トークン使う。合計が max_tokens を超えると
            // 本文が空のまま打ち切られるので、実測（約 700）の倍を確保する。
            "max_tokens": 2048,
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "tags",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "properties": {
                            "tags": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "name": { "type": "string" },
                                        "confidence": { "type": "number" },
                                    },
                                    "required": ["name", "confidence"],
                                    "additionalProperties": false,
                                },
                            },
                        },
                        "required": ["tags"],
                        "additionalProperties": false,
                    },
                },
            },
        });

        // 思考過程は結果に要らない。推論モデルは reasoning に数百トークン使い、
        // max_tokens に達すると本文が空のまま打ち切られる（実測あり）ので、
        // 対応サーバでは切っておく。値はサーバによって受け付ける語彙が違うため
        // 設定可能にしてある（空指定でフィールドごと省略）。
        if let Some(effort) = &self.reasoning_effort {
            body["reasoning_effort"] = serde_json::Value::String(effort.clone());
        }
        body
    }
}

/// 応答から本文だけを取り出すための最小の型。`reasoning_content` など他の
/// フィールドは無視する。
#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
    /// `length` なら max_tokens で打ち切られている（本文が空になりうる）。
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChatMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct TagsPayload {
    tags: Vec<TagItem>,
}

#[derive(Deserialize)]
struct TagItem {
    name: String,
    #[serde(default)]
    confidence: Option<f32>,
}

#[async_trait]
impl AiProvider for OpenAiVisionProvider {
    async fn suggest_tags(
        &self,
        content: &[u8],
        mime: &str,
    ) -> Result<Vec<TagSuggestion>, AiError> {
        let mut req = self.http.post(&self.endpoint);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .json(&self.request_body(content, mime))
            .send()
            .await
            // 接続不能・タイムアウトは環境要因なので再試行の余地がある。
            .map_err(|e| AiError::Provider(format!("リクエストに失敗しました: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let body = truncate_for_log(&body);
            let msg = format!("HTTP {status}: {body}");
            return Err(if is_permanent_status(status.as_u16()) {
                AiError::Permanent(msg)
            } else {
                AiError::Provider(msg)
            });
        }

        let parsed: ChatResponse = resp
            .json()
            .await
            .map_err(|e| AiError::Permanent(format!("応答を解釈できません: {e}")))?;
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| AiError::Provider("応答に choices がありません".into()))?;

        Ok(parse_tags(&content_of(choice)?)?)
    }

    async fn summarize(&self, _content: &[u8]) -> Result<String, AiError> {
        Err(AiError::NotImplemented)
    }
}

/// この HTTP ステータスは再試行しても結果が変わらないか。
///
/// OpenAI 互換サーバの 400 は用途が広く、**一時的な状態にも使われる**。実測でも
/// 同時実行中に `400 {"error":"Model is unloaded."}` / `{"error":"terminated"}`
/// が返った。これを恒久失敗にすると、モデルの入れ替え中に走った一括生成が
/// まとめて「二度と生成しない」状態に落ちる。そのため 400 は再試行に回し、
/// 設定・入力が原因だと確実に言えるものだけを恒久失敗にする。
///
/// 恒久扱いにするステータス:
/// - 401 / 403 … 認証・認可（キーを直すまで何度投げても同じ）
/// - 404 … エンドポイントかモデル名が存在しない
/// - 405 / 413 / 414 / 415 / 422 … リクエストの作り方そのものが通らない
fn is_permanent_status(status: u16) -> bool {
    matches!(status, 401 | 403 | 404 | 405 | 413 | 414 | 415 | 422)
}

/// 応答の 1 択目から本文を取り出す。
///
/// 本文が空になるのは主に 2 通りで、再試行の可否が違う:
/// - `max_tokens` で打ち切られた（`finish_reason = length`）… 同じ設定で投げ
///   直しても同じなので恒久失敗
/// - それ以外（モデルのロード／スワップ中など）… 時間を置けば通るので再試行
///
/// 後者を恒久失敗にすると、大きいモデルへ切り替えた直後の一括生成が
/// 「読み込み中だった」というだけで全滅する。
fn content_of(choice: ChatChoice) -> Result<String, AiError> {
    let content = choice.message.content.unwrap_or_default();
    if !content.trim().is_empty() {
        return Ok(content);
    }
    Err(if choice.finish_reason.as_deref() == Some("length") {
        AiError::Permanent(
            "応答が max_tokens で打ち切られ、本文が空でした\
             （推論の長いモデルでは上限を上げる必要があります）"
                .into(),
        )
    } else {
        AiError::Provider(format!(
            "応答本文が空です（モデルの読み込み中かもしれません。finish_reason={}）",
            choice.finish_reason.as_deref().unwrap_or("なし")
        ))
    })
}

/// モデルが返した本文からタグ配列を取り出す。
///
/// `response_format` に対応していないサーバがコードフェンスで包んでくることが
/// あるため、素のパースに失敗したら最初の `{` から最後の `}` までを拾い直す。
fn parse_tags(content: &str) -> Result<Vec<TagSuggestion>, AiError> {
    let payload: TagsPayload = serde_json::from_str(content)
        .or_else(|_| {
            let start = content.find('{');
            let end = content.rfind('}');
            match (start, end) {
                (Some(s), Some(e)) if s < e => serde_json::from_str(&content[s..=e]),
                _ => serde_json::from_str(content),
            }
        })
        .map_err(|e| {
            AiError::Permanent(format!(
                "タグ JSON を解釈できません: {e} (本文: {})",
                truncate_for_log(content)
            ))
        })?;

    Ok(payload
        .tags
        .into_iter()
        .map(|t| TagSuggestion {
            name: t.name,
            // confidence 欠落は「不明」であって 0 ではない。閾値で切り捨てられ
            // ないよう、中立な 1.0 ではなく閾値通過寄りの既定にしておく。
            confidence: t.confidence.unwrap_or(1.0).clamp(0.0, 1.0),
        })
        .collect())
}

/// エラーメッセージに埋める本文を短く切る（DB の error 列とログの肥大化防止）。
fn truncate_for_log(s: &str) -> String {
    const LIMIT: usize = 300;
    if s.chars().count() <= LIMIT {
        return s.to_string();
    }
    let head: String = s.chars().take(LIMIT).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_json() {
        let got = parse_tags(r#"{"tags":[{"name":"湖","confidence":0.95}]}"#).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "湖");
        assert!((got[0].confidence - 0.95).abs() < 1e-6);
    }

    /// json_schema 非対応のサーバはコードフェンスで包んで返すことがある。
    /// ここで拾えないと、そのサーバでは一切タグが付かない。
    #[test]
    fn parses_json_wrapped_in_code_fence() {
        let body = "```json\n{\"tags\":[{\"name\":\"山\",\"confidence\":0.8}]}\n```";
        let got = parse_tags(body).unwrap();
        assert_eq!(got[0].name, "山");
    }

    #[test]
    fn missing_confidence_defaults_to_one() {
        let got = parse_tags(r#"{"tags":[{"name":"空"}]}"#).unwrap();
        assert_eq!(got[0].confidence, 1.0);
    }

    /// 範囲外の confidence をそのまま通すと、閾値比較や DB の REAL 列に
    /// 意味の無い値が入る。
    #[test]
    fn confidence_is_clamped() {
        let got = parse_tags(r#"{"tags":[{"name":"a","confidence":42},{"name":"b","confidence":-1}]}"#)
            .unwrap();
        assert_eq!(got[0].confidence, 1.0);
        assert_eq!(got[1].confidence, 0.0);
    }

    #[test]
    fn non_json_body_is_permanent_error() {
        let err = parse_tags("すみません、画像を読めませんでした").unwrap_err();
        assert!(matches!(err, AiError::Permanent(_)));
    }

    fn choice(content: Option<&str>, finish: Option<&str>) -> ChatChoice {
        ChatChoice {
            message: ChatMessage {
                content: content.map(str::to_string),
            },
            finish_reason: finish.map(str::to_string),
        }
    }

    /// 大きいモデルへ切り替えた直後はロード中で本文が空のまま返ることがある。
    /// これを恒久失敗にすると、一括生成が「読み込み中だった」だけで全滅する。
    #[test]
    fn empty_content_without_length_is_retryable() {
        let err = content_of(choice(Some(""), Some("stop"))).unwrap_err();
        assert!(matches!(err, AiError::Provider(_)));
        let err = content_of(choice(None, None)).unwrap_err();
        assert!(matches!(err, AiError::Provider(_)));
    }

    /// max_tokens で打ち切られた場合は、同じ設定で投げ直しても同じ。
    #[test]
    fn truncated_response_is_permanent() {
        let err = content_of(choice(Some("   "), Some("length"))).unwrap_err();
        assert!(matches!(err, AiError::Permanent(_)), "{err:?}");
    }

    #[test]
    fn content_is_returned_as_is() {
        let got = content_of(choice(Some("{\"tags\":[]}"), Some("stop"))).unwrap();
        assert_eq!(got, "{\"tags\":[]}");
    }

    fn provider(effort: Option<&str>) -> OpenAiVisionProvider {
        OpenAiVisionProvider::new(
            "http://example.invalid/v1",
            "m",
            None,
            5,
            effort.map(str::to_string),
            Duration::from_secs(1),
        )
        .unwrap()
    }

    /// 空指定はフィールドごと省略する。未知のフィールドを弾くサーバへ
    /// `""` を送りつけないため。
    #[test]
    fn reasoning_effort_is_omitted_when_blank() {
        for effort in [None, Some(""), Some("  ")] {
            let body = provider(effort).request_body(b"x", "image/jpeg");
            assert!(body.get("reasoning_effort").is_none(), "{effort:?}");
        }
    }

    #[test]
    fn reasoning_effort_is_sent_when_set() {
        let body = provider(Some("none")).request_body(b"x", "image/jpeg");
        assert_eq!(body["reasoning_effort"], "none");
    }

    /// 400 は再試行に回す。実測で、同時実行中のモデル入れ替えが
    /// `400 {"error":"Model is unloaded."}` として返ってきた。恒久失敗に
    /// すると、その瞬間に走っていた一括生成がまとめて死ぬ。
    #[test]
    fn transient_statuses_are_retryable() {
        for status in [400, 408, 409, 425, 429, 500, 502, 503, 504] {
            assert!(!is_permanent_status(status), "{status} は再試行すべき");
        }
    }

    /// 設定・入力が原因と確実に言えるものだけ、即座に諦める。
    #[test]
    fn configuration_errors_are_permanent() {
        for status in [401, 403, 404, 405, 413, 414, 415, 422] {
            assert!(is_permanent_status(status), "{status} は再試行しても無駄");
        }
    }

    /// 末尾スラッシュの有無でエンドポイントが二重スラッシュにならない。
    #[test]
    fn endpoint_joins_without_double_slash() {
        let p = OpenAiVisionProvider::new(
            "http://example.invalid/v1/",
            "m",
            None,
            5,
            None,
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(p.endpoint, "http://example.invalid/v1/chat/completions");
    }
}
