//! narashi による AI タグ名の表記ゆれ解消。
//!
//! LLM は同じ被写体に対して「白背景 / 白い背景 / 白バック」のような揺れた表記を
//! 返す。そのまま登録すると `tags.name` が UNIQUE なだけに別タグとして増え続け、
//! 絞り込みが機能しなくなる。そこで多言語埋め込みでグルーピングし、
//!
//! 1. グループ内に**既存タグ**があればそこへ寄せる（新語彙を増やさない）
//! 2. 無ければ narashi が選んだ代表（最も汎用的な表記）を使う
//!
//! という順で最終的なタグ名を決める。埋め込みは OpenAI 互換エンドポイントに
//! 投げるため、モデルの重みをローカルへ落とす必要はない。

use narashi::{Group, Language, Model, Narashi, Options};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::AiError;

/// 正規化の結果 1 件。`raw_name` は LLM の生出力で、寄せ具合を後から確認できる
/// よう `ai_file_tags.raw_name` に残す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTag {
    pub raw_name: String,
    pub name: String,
}

pub struct TagNormalizer {
    // narashi は同期 API（ureq）なので、呼び出しは spawn_blocking に逃がす。
    narashi: Arc<Narashi>,
    threshold: f32,
}

impl TagNormalizer {
    /// `base_url` は `/v1` まで含む OpenAI 互換のベース URL。
    /// `model` は埋め込みモデル名（互換サーバでは任意の文字列が使える）。
    pub fn new(
        base_url: &str,
        model: impl Into<String>,
        api_key: Option<String>,
        threshold: f32,
    ) -> Result<Self, AiError> {
        let mut opts = Options::new()
            .with_model(Model::OpenAi(model.into()))
            .with_openai_base_url(base_url.trim_end_matches('/').to_string())
            // 代表選出は日本語を優先する。LLM 側にも日本語で出させているが、
            // 英語が混ざった時に英語側が代表に選ばれると語彙が割れる。
            .with_language_priority([Language::Japanese]);
        if let Some(key) = api_key.filter(|k| !k.trim().is_empty()) {
            opts = opts.with_openai_api_key(key);
        }
        let narashi = Narashi::with_options(opts)
            .map_err(|e| AiError::Permanent(format!("narashi を初期化できません: {e}")))?;
        Ok(Self {
            narashi: Arc::new(narashi),
            threshold,
        })
    }

    /// `candidates`（LLM の生出力）を、既存タグ語彙 `vocabulary` へ寄せて返す。
    /// `vocabulary` は使用数の多い順に渡すこと（同じグループに複数の既存タグが
    /// 居たとき、より使われている方を寄せ先に選ぶ）。
    pub async fn resolve(
        &self,
        candidates: Vec<String>,
        vocabulary: Vec<String>,
    ) -> Result<Vec<ResolvedTag>, AiError> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        // 候補がすべて既存タグと完全一致なら、寄せる余地が無いので埋め込みを
        // 呼ばない。似た写真が続くとこれが大半を占める。
        //
        // 呼び出しを削るのは速度のためだけではない: 埋め込みと LLM を同じ
        // GPU に載せているサーバでは、1 ジョブごとに埋め込みを挟むとモデルの
        // 入れ替えが起き、実行中の生成が落ちる（実測で `Model is unloaded.`）。
        let known: HashSet<&str> = vocabulary.iter().map(String::as_str).collect();
        if !has_new_candidate(&candidates, &known) {
            return Ok(dedup_by_name(
                candidates.iter().map(|c| (c.clone(), c.clone())).collect(),
            ));
        }

        // 既存語彙と候補をまとめて 1 回で埋め込む。候補ごとに既存タグ全件と
        // similarity を取ると呼び出し回数が語彙数に比例して現実的でない。
        let mut texts: Vec<String> = Vec::with_capacity(vocabulary.len() + candidates.len());
        texts.extend(vocabulary.iter().cloned());
        for c in &candidates {
            if !known.contains(c.as_str()) {
                texts.push(c.clone());
            }
        }

        let narashi = self.narashi.clone();
        let threshold = self.threshold;
        let groups = tokio::task::spawn_blocking(move || narashi.normalize(&texts, threshold))
            .await
            .map_err(|e| AiError::Provider(format!("正規化タスクが落ちました: {e}")))?
            .map_err(|e| AiError::Provider(format!("埋め込みの取得に失敗しました: {e}")))?;

        Ok(map_candidates(&groups, &candidates, &vocabulary))
    }
}

/// 既存語彙に無い候補が 1 つでもあるか（＝埋め込みを呼ぶ価値があるか）。
///
/// 全部が既存タグと完全一致なら、どうグループ分けしても各候補は自分自身の
/// 名前に落ちる（同名の既存タグが最優先の寄せ先になるため）。
fn has_new_candidate(candidates: &[String], vocabulary: &HashSet<&str>) -> bool {
    candidates.iter().any(|c| !vocabulary.contains(c.as_str()))
}

/// グループ分けの結果から、各候補の最終的なタグ名を決める。
///
/// narashi 呼び出しから切り離してあるのは、寄せ先の選び方（既存タグ優先・
/// 使用数優先）がこの機能の肝で、ネットワーク無しで検証したいため。
fn map_candidates(
    groups: &[Group],
    candidates: &[String],
    vocabulary: &[String],
) -> Vec<ResolvedTag> {
    // 語彙は使用数の多い順に渡される。同じグループに複数居たら順位の小さい方。
    let rank: HashMap<&str, usize> = vocabulary
        .iter()
        .enumerate()
        .map(|(i, name)| (name.as_str(), i))
        .collect();

    let mut target_of: HashMap<&str, &str> = HashMap::new();
    for g in groups {
        let existing = g
            .members
            .iter()
            .filter_map(|m| rank.get(m.as_str()).map(|r| (*r, m.as_str())))
            .min_by_key(|(r, _)| *r)
            .map(|(_, name)| name);
        let target = existing.unwrap_or(g.canonical.as_str());
        for m in &g.members {
            target_of.insert(m.as_str(), target);
        }
    }

    dedup_by_name(
        candidates
            .iter()
            .map(|c| {
                let name = *target_of.get(c.as_str()).unwrap_or(&c.as_str());
                (c.clone(), name.to_string())
            })
            .collect(),
    )
}

/// 寄せた結果として候補同士が同じタグ名に落ちることがある。候補は信頼度の
/// 高い順に並んでいるので、先に出た方を残す。重複したまま返すと同じタグへの
/// 付与が二重に走る。
fn dedup_by_name(pairs: Vec<(String, String)>) -> Vec<ResolvedTag> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for (raw_name, name) in pairs {
        if seen.contains(&name) {
            continue;
        }
        seen.push(name.clone());
        out.push(ResolvedTag { raw_name, name });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(canonical: &str, members: &[&str]) -> Group {
        Group {
            canonical: canonical.into(),
            members: members.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// 既存タグと同じグループに落ちた候補は、代表よりも既存タグを優先する。
    /// ここで代表を採ると、意味が同じタグが既存語彙と別に増えていく。
    #[test]
    fn candidate_snaps_to_existing_vocabulary() {
        let groups = vec![group("白背景", &["白背景", "白い背景"])];
        let got = map_candidates(&groups, &strs(&["白い背景"]), &strs(&["白背景"]));
        assert_eq!(
            got,
            vec![ResolvedTag {
                raw_name: "白い背景".into(),
                name: "白背景".into()
            }]
        );
    }

    /// 代表が既存タグでない場合でも、グループ内に既存タグが居ればそちらへ寄せる。
    #[test]
    fn existing_vocabulary_wins_over_canonical() {
        let groups = vec![group("背景", &["背景", "白い背景", "白背景"])];
        let got = map_candidates(&groups, &strs(&["白い背景"]), &strs(&["白背景"]));
        assert_eq!(got[0].name, "白背景");
    }

    /// 既存タグが複数居たら、より使われている方（語彙リストで先に来る方）。
    #[test]
    fn most_used_existing_tag_wins() {
        let groups = vec![group("湖", &["湖", "みずうみ", "湖水"])];
        let got = map_candidates(&groups, &strs(&["湖水"]), &strs(&["みずうみ", "湖"]));
        assert_eq!(got[0].name, "みずうみ", "語彙リストの先頭ほど使用数が多い");
    }

    /// 既存タグが無ければ narashi の代表をそのまま使う。
    #[test]
    fn falls_back_to_canonical_when_no_existing_tag() {
        let groups = vec![group("山", &["山", "山岳"])];
        let got = map_candidates(&groups, &strs(&["山岳"]), &strs(&["空"]));
        assert_eq!(got[0].name, "山");
        assert_eq!(got[0].raw_name, "山岳", "生の出力は保持する");
    }

    /// 同一バッチ内の揺れが同じタグ名に落ちたら 1 件にまとめる。重複したまま
    /// 返すと attach が同じタグに二重に走る。
    #[test]
    fn duplicate_targets_are_collapsed_keeping_first() {
        let groups = vec![group("湖", &["湖", "みずうみ"])];
        let got = map_candidates(&groups, &strs(&["湖", "みずうみ"]), &[]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].raw_name, "湖", "信頼度の高い方（先に来る方）を残す");
    }

    /// どのグループにも現れない候補は素通しする（グループ分けの取りこぼしで
    /// タグが消えるのは避ける）。
    #[test]
    fn unknown_candidate_passes_through() {
        let got = map_candidates(&[], &strs(&["夕焼け"]), &strs(&["湖"]));
        assert_eq!(got[0].name, "夕焼け");
    }

    fn vocab(v: &[String]) -> HashSet<&str> {
        v.iter().map(String::as_str).collect()
    }

    /// 候補が全部既存タグと完全一致なら埋め込みを呼ばない。似た写真が続くと
    /// これが大半で、毎回呼ぶと埋め込みモデルと LLM が同じ GPU を奪い合う。
    #[test]
    fn skips_embedding_when_all_candidates_are_known() {
        let v = strs(&["湖", "岩", "空"]);
        assert!(!has_new_candidate(&strs(&["湖", "岩"]), &vocab(&v)));
        assert!(!has_new_candidate(&[], &vocab(&v)));
    }

    /// 新語が 1 つでもあれば呼ぶ（寄せ先を探す必要がある）。
    #[test]
    fn embeds_when_any_candidate_is_new() {
        let v = strs(&["湖", "岩"]);
        assert!(has_new_candidate(&strs(&["湖", "夕焼け"]), &vocab(&v)));
        assert!(has_new_candidate(&strs(&["夕焼け"]), &vocab(&v)));
        assert!(has_new_candidate(&strs(&["山"]), &HashSet::new()));
    }

    /// 省略経路でも重複はまとめる（同じタグへの付与が二重に走らないこと）。
    #[test]
    fn dedup_keeps_first_occurrence() {
        let got = dedup_by_name(vec![
            ("湖".into(), "湖".into()),
            ("みずうみ".into(), "湖".into()),
            ("岩".into(), "岩".into()),
        ]);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].raw_name, "湖");
        assert_eq!(got[1].name, "岩");
    }
}
