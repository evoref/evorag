//! 会話の層の種類 (03 §3.3、04 §2)。本体は項目を省略せずに書く (値の無い項目は null、03 §4.2 の規則 7)。

use evorag_store::key::OwnedPart;
use evorag_store::{DeriveFn, Envelope, KindSpec, RecordIds, WriteOp};
use evorag_types::Id;
use serde::{Deserialize, Serialize};

pub const KIND_SESSION: &str = "session";
pub const KIND_TURN: &str = "turn";
pub const KIND_SIGNAL: &str = "signal";
/// 裏の処理の手順の進み具合 (03 §3.3)。
pub const KIND_JOB: &str = "job";
/// 利用者が画面から変えた、データ根の全体に効く設定 (03 §3.3、v0.2)。
pub const KIND_SETTING: &str = "setting";
/// 役割ごとに記録したモデル (06 §3.1、03 §3.3、v0.2)。
pub const KIND_MODEL_BINDING: &str = "model_binding";
/// 役割の名前 (足すだけで広げる文字列、03 §3.3)。
pub const ROLE_CHAT: &str = "chat";
pub const ROLE_EMBED: &str = "embed";
pub const ROLE_RERANK: &str = "rerank";
/// 設定の名前: 読んで無視した設定のキーと、その内容のハッシュ (同じなら 2 回目から知らせない、07 §4.1)。
pub const SETTING_IGNORED_CONFIG: &str = "notice.ignored_config";
/// 設定の名前: 答え方の既定 (07 §3 の `/style`)。
pub const SETTING_ANSWER_LANGUAGE: &str = "answer.language";
pub const SETTING_ANSWER_LENGTH: &str = "answer.length";
pub const SETTING_ANSWER_FORMAT: &str = "answer.format";
/// job の手順: 消した記録を索引と退避から除く (03 §7.3、04 §7.7 の 2)。
pub const JOB_PURGE_BACKUPS: &str = "purge_backups";
/// job の手順: 索引の項目だけを除く (起動時の突き合わせで除けなかった、DB に記録の無い前の世代の索引の行。
/// 退避には触れない、04 §7.7 の 2)。古い版は知らない手順の job を使わずに残す。
pub const JOB_PURGE_INDEXES: &str = "purge_indexes";
/// job の手順: 忘れるの退避の写し直し (03 §7.3)。忘れるの指示の ID を任意の項目 `directive` に持ち、`ids` は
/// 退避から除く ID だけを持つ。
pub const JOB_FORGET_BACKUPS: &str = "forget_backups";

/// restore で、置き換える DB から戻した DB へ引き継ぐ記録か (残っている整理の job、03 §10)。
/// `evorag_store::restore::restore` の `carry` に渡す。
#[must_use]
pub fn carried_on_restore(env: &Envelope) -> bool {
    #[derive(Deserialize)]
    struct Task {
        task: String,
    }
    env.kind == KIND_JOB
        && env.body_as::<Task>().is_ok_and(|b| {
            [JOB_PURGE_BACKUPS, JOB_PURGE_INDEXES, JOB_FORGET_BACKUPS].contains(&b.task.as_str())
        })
}

/// ターン → セッション。
pub const REL_IN_SESSION: &str = "in_session";
/// 応答のターン → 発話のターン。
pub const REL_REPLY_TO: &str = "reply_to";
/// 信号 → 対象のターン。
pub const REL_TARGET: &str = "target";
/// 関係: 埋め込み → 対象 (チャンク) (05 §2.2)。
pub const REL_EMBEDDING_OF: &str = "embedding_of";

/// 埋め込み (05 §2.2、v0.2)。どの記録のどの単位の、どのモデルの埋め込みか。ベクトルは `blobs` の
/// part 0 (f32 のリトルエンディアン)。作り直せる派生の記録なので、ジャーナルに書かない。
pub const KIND_EMBEDDING: &str = "embedding";
/// 埋め込みの単位: チャンク。
pub const UNIT_CHUNK: &str = "chunk";
/// 埋め込みの単位: ノートの検索の鍵 (04 §2)。`target` はノート、`key` は鍵の位置。
pub const UNIT_KEY: &str = "key";

/// `embedding` の本体 (v1)。出典の ID を本体にも持つ (03 §4.2 の規則 12)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingBody {
    /// 対象の記録 (チャンクかノート)。
    pub target: Id,
    /// 単位 (`chunk`・`key`)。知らない値は読んで残し、使わない。
    pub unit: String,
    /// 対象のセッション (今のセッションを候補から除くため)。
    pub session: Id,
    /// 埋め込みのモデルの model_key。
    pub model_key: String,
    pub dim: u32,
    /// 単位 `key` のときの鍵の位置 (0 から、04 §2)。チャンクでは書かない (v1 の後から足した項目なので、
    /// 前からあるチャンクの埋め込みの保存形を変えない)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<u32>,
    /// 作ったときの出典のターンの改訂の番号 (03 §3.3 の `src_rev`、v0.3 の任意の項目)。チャンクの埋め込みは
    /// チャンクの出典のターンの番号を持つ。ノートの鍵の埋め込みは null (ノートの `src_revs` で判定する)。
    /// 無い・null は 0 とみなす ([`SrcRev::of`])。
    #[serde(default)]
    pub src_rev: Option<u32>,
}

/// 関係: ノート → 出典の往復の発話のターン (04 §2)。
pub const REL_NOTE_OF: &str = "note_of";

/// 話題の検索の鍵 (04 §2、v0.2)。作り直せる派生の記録なので、ジャーナルに書かない。
pub const KIND_NOTE: &str = "note";

/// `note` の本体 (v1)。出典の往復の発話のターンの ID を本体にも持つ (03 §4.2 の規則 12)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteBody {
    pub session: Id,
    /// 区切りの往復 (各往復の利用者の発話のターンの ID、古い順)。
    pub exchanges: Vec<Id>,
    /// 検索の鍵 (検査を通ったもの、8 個まで)。空なら索引の項目を作らない。
    pub keys: Vec<String>,
    /// 鍵を作った応答のモデルの model_key。
    pub model_key: String,
    /// 区切りと鍵の作り方の版 (04 §2)。変われば作り直す。
    pub rules: u32,
    /// 鍵を作ったときに読んだ出典のターン (往復の発話と応答) ごとの改訂の番号 (v0.3 の任意の項目、03 §3.3、
    /// 04 §7.7 の 4)。項目の無い (古い) ノートは空で読み、出典の番号を 0 とみなす。合わなければノートを
    /// 作り直す。
    #[serde(default)]
    pub src_revs: Vec<SrcRev>,
    /// 鍵を作る入力を読んだときに見えていた、最も新しい忘れるの指示の ID (v0.3 の任意の項目、04 §7.2 の当て直し
    /// の一覧の 4)。指示が無ければ null。忘れるの当て直しは、伏せ字にしないときの `[非表示]` の範囲を持つ往復の
    /// ノートのうち、これが無いか指示より前のもの (その範囲を伏せずに作ったもの) を消す。
    #[serde(default)]
    pub forgets_seen: Option<Id>,
}

/// ターンの本体の、改訂の番号の項目の名前 (04 §7.3、03 §3.3 の任意の項目。無ければ 0)。
pub const TURN_REV_FIELD: &str = "rev";

/// 派生の記録の出典のターンと、作ったときのその改訂の番号 (03 §3.3 の `src_revs`、04 §7.7 の 4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SrcRev {
    pub turn: Id,
    pub rev: u32,
}

impl SrcRev {
    /// 任意の項目 `src_rev` の読み方 (03 §3.3): 無ければ 0。
    #[must_use]
    pub fn of(src_rev: Option<u32>) -> u32 {
        src_rev.unwrap_or(0)
    }
}

/// 関係: チャンク → 元のターン (05 §2.4)。
pub const REL_CHUNK_OF: &str = "chunk_of";

/// ターンの中の範囲 (05 §2.4、v0.2)。作り直せる派生の記録なので、ジャーナルに書かない。
pub const KIND_CHUNK: &str = "chunk";

/// `chunk` の本体 (v1)。本文は持たない (ターンの本文から作り直せる)。出典のターンの ID を本体にも持つ
/// (03 §4.2 の規則 12)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkBody {
    pub turn: Id,
    pub session: Id,
    /// ターンの本文の中の範囲 (文字の位置)。
    pub start: u32,
    pub end: u32,
    /// 区切り方の版 (05 §2.4)。変われば作り直す。
    pub rules: u32,
    /// 作ったときの出典のターンの改訂の番号 (03 §3.3 の `src_rev`、v0.3 の任意の項目)。無ければ 0 とみなす
    /// ([`SrcRev::of`])。今のターンの番号と合わないチャンクは、伏せ字の前の本文から作ったものとして消す・使わない
    /// (04 §7.2 の当て直しの 4、§7.7 の 1)。
    #[serde(default)]
    pub src_rev: Option<u32>,
}

impl ChunkBody {
    /// 作ったときの出典のターンの改訂の番号 (無ければ 0)。
    #[must_use]
    pub fn src_rev(&self) -> u32 {
        SrcRev::of(self.src_rev)
    }
}

/// 関係: 想起への操作 → 問いの応答のターン (05 §5.2)。
pub const REL_FEEDBACK_ANSWER: &str = "feedback_answer";
/// 関係: 想起への操作 → 対象の往復 (を代表するターン)。
pub const REL_FEEDBACK_TARGET: &str = "feedback_target";

/// 想起への利用者の操作 (05 §5.2、v0.2)。利用者の明示の操作なのでジャーナルに書く。
pub const KIND_RECALL_FEEDBACK: &str = "recall_feedback";
/// 帯の往復を添えて作り直した。
pub const FEEDBACK_ATTACH: &str = "attach";
/// 「この記録は、この問いには関係が無い」の印。
pub const FEEDBACK_IRRELEVANT: &str = "irrelevant";

/// `recall_feedback` の本体 (v1)。出典の ID を本体にも持つ (03 §4.2 の規則 12)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecallFeedbackBody {
    /// `attach`・`irrelevant`。知らない値は読んで残し、使わない。
    pub op: String,
    /// 問いの応答のターン。
    pub answer: Id,
    /// 対象の往復 (を代表するターン)。
    pub target: Id,
    /// その時のリランクの生の値 (リランクしていなければ無い)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    /// その時のリランカーの model_key。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reranker: Option<String>,
}

/// 副索引: セッションの更新時刻 (一覧を新しい順に並べる)。
pub(crate) const LOOKUP_UPDATED_AT: &str = "updated_at";

/// この版が書く、会話の層の種類。会話の追記は、新しい版の記録があっても今の `v` で足してよい
/// (巻き戻した後も会話を続けられるように、03 §4.2 の規則 6)。
///
/// どれも会話と利用者の明示の操作なので、ジャーナルにも書く (03 §6)。ジャーナルから取り込むときは、
/// 本体から関係と副索引を作り直す。
pub fn kinds() -> Vec<KindSpec> {
    // `derive` の関数と、それが作りうる関係の名前・副索引の項目 (`repair` が作り直すときに消すのはこれだけ)。
    let append = |name, derive: DeriveFn, rels, lookups| KindSpec {
        name,
        current_v: 1,
        create_when_newer_present: true,
        journaled: true,
        derive: Some(derive),
        derived_rels: rels,
        derived_lookups: lookups,
        monotonic: None,
        ids: RecordIds::Generated,
    };
    vec![
        append(KIND_SESSION, derive_session, &[], &[LOOKUP_UPDATED_AT]),
        // 改訂の番号は伏せ字のたびに上げる。取り込みと repair は、これを下げる行を当てない (03 §6、04 §7.3)。
        KindSpec {
            monotonic: Some(TURN_REV_FIELD),
            ..append(KIND_TURN, derive_turn, &[REL_IN_SESSION, REL_REPLY_TO], &[])
        },
        append(KIND_SIGNAL, derive_signal, &[REL_TARGET], &[]),
        append(
            KIND_RECALL_FEEDBACK,
            derive_feedback,
            &[REL_FEEDBACK_ANSWER, REL_FEEDBACK_TARGET],
            &[],
        ),
        // 利用者の明示の操作なのでジャーナルに書く。関係も副索引も持たない (件数が少ないので全部読む)。
        KindSpec {
            create_when_newer_present: true,
            journaled: true,
            ..KindSpec::new(KIND_SETTING, 1)
        },
        // 利用者が setup で確かめた (または最初の起動で記録した) 正本なので、ジャーナルに書く。
        KindSpec {
            create_when_newer_present: true,
            journaled: true,
            ..KindSpec::new(KIND_MODEL_BINDING, 1)
        },
        // 点検の結果は作り直せる派生の記録なので、ジャーナルには書かない (03 §3.3)。
        KindSpec::new(KIND_MODEL_PROBE, 1),
        // チャンク・埋め込み・ノートは作り直せる派生の記録 (05 §2.4・§2.2、04 §2)。関係は書くときに足し、
        // `repair` は本体から作り直す (03 §10)。
        KindSpec {
            derive: Some(derive_chunk),
            derived_rels: &[REL_CHUNK_OF],
            ..KindSpec::new(KIND_CHUNK, 1)
        },
        KindSpec {
            derive: Some(derive_embedding),
            derived_rels: &[REL_EMBEDDING_OF],
            ..KindSpec::new(KIND_EMBEDDING, 1)
        },
        KindSpec {
            derive: Some(derive_note),
            derived_rels: &[REL_NOTE_OF],
            ..KindSpec::new(KIND_NOTE, 1)
        },
        // v0.2 のしきい値は同梱の事例から求め直せるので、ジャーナルには書かない (03 §3.3)。
        KindSpec::new(KIND_THRESHOLD, 1),
        // job は作り直せる手順の記録なので、ジャーナルには書かない。
        KindSpec::new(KIND_JOB, 1),
        // 事実は LLM で作る派生の記録 (04 §2)。ジャーナルに書かない。関係と副索引は本体から作り直せる。
        KindSpec {
            derive: Some(crate::facts::derive_fact),
            derived_rels: crate::facts::FACT_RELS,
            derived_lookups: crate::facts::FACT_LOOKUPS,
            ..KindSpec::new(crate::facts::KIND_FACT, crate::facts::FACT_V)
        },
        // 訂正の候補と検証済みの訂正は、字句の規則と LLM で作る派生の記録 (04 §6)。ジャーナルに書かない。関係は
        // 本体 (`turn`) から作り直せる。
        KindSpec {
            derive: Some(crate::correction::derive_correction),
            derived_rels: &[crate::correction::REL_CORRECTION_OF],
            ..KindSpec::new(
                crate::correction::KIND_CORRECTION,
                crate::correction::CORRECTION_V,
            )
        },
        // 事実への指示は利用者の明示の操作の正本 (04 §7.5)。ジャーナルに書き、追記だけにする。
        KindSpec {
            create_when_newer_present: true,
            journaled: true,
            derive: Some(crate::facts::derive_directive),
            derived_rels: crate::facts::DIRECTIVE_RELS,
            derived_lookups: crate::facts::DIRECTIVE_LOOKUPS,
            ..KindSpec::new(
                crate::facts::KIND_FACT_DIRECTIVE,
                crate::facts::FACT_DIRECTIVE_V,
            )
        },
    ]
}

/// v0.5 の根で作らない種類 (03 §3.3)。v0.5 は、v0.4 のデータ根から取り込むときにだけ読む (`import-v04`)。
pub const V04_ONLY_KINDS: [&str; 4] = [
    crate::facts::KIND_FACT,
    crate::correction::KIND_CORRECTION,
    KIND_NOTE,
    crate::facts::KIND_FACT_DIRECTIVE,
];

/// v0.5 の新しいデータ根 (形式の版 `0.5`、03 §12.4) の種類。会話の層の種類 ([`kinds`] から v0.4 だけの種類
/// [`V04_ONLY_KINDS`] を除いたもの) に、指示・区切り・台帳・単位を足す。耐久性とジャーナルの正本は 03 §3.3 の
/// 「v0.5 の種類の耐久性とジャーナル」。
pub fn kinds_v05() -> Vec<KindSpec> {
    let mut out: Vec<KindSpec> = kinds()
        .into_iter()
        .filter(|k| !V04_ONLY_KINDS.contains(&k.name))
        .collect();
    out.extend([
        // 指示は利用者の明示の操作の正本 (04 §7.5)。ジャーナルに書き、追記だけにする。
        KindSpec {
            create_when_newer_present: true,
            journaled: true,
            derive: Some(crate::directive::derive_directive),
            derived_rels: crate::directive::DIRECTIVE_RELS,
            derived_lookups: crate::directive::DIRECTIVE_LOOKUPS,
            ..KindSpec::new(
                crate::directive::KIND_DIRECTIVE,
                crate::directive::DIRECTIVE_V,
            )
        },
        // 区切りは派生 (ジャーナルに書かない)。ID は最初の往復の発話のターンから作る (04 §2)。
        KindSpec {
            derive: Some(crate::segment::derive_segment),
            derived_rels: crate::segment::SEGMENT_RELS,
            derived_lookups: crate::segment::SEGMENT_LOOKUPS,
            ids: RecordIds::Content,
            ..KindSpec::new(crate::segment::KIND_SEGMENT, crate::segment::SEGMENT_V)
        },
        // 台帳は派生だが作り直せない (`Immediate` と退避で守る、03 §3.4)。ジャーナルには書かない。
        KindSpec {
            derive: Some(crate::ledger::derive_ledger),
            derived_rels: crate::ledger::LEDGER_RELS,
            derived_lookups: crate::ledger::LEDGER_LOOKUPS,
            ..KindSpec::new(crate::ledger::KIND_PROPOSAL, crate::ledger::PROPOSAL_V)
        },
        // 単位は導出で作り直す派生 (ジャーナル・退避・書き出しに写さない)。ID は中身から作る (04 §5.1)。
        KindSpec {
            derive: Some(crate::unit::derive_unit),
            derived_rels: crate::unit::UNIT_RELS,
            derived_lookups: crate::unit::UNIT_LOOKUPS,
            ids: RecordIds::Content,
            ..KindSpec::new(crate::unit::KIND_UNIT, crate::unit::UNIT_V)
        },
    ]);
    out
}

/// モデルの性質の点検の結果 (06 §3.3、v0.2)。
pub const KIND_MODEL_PROBE: &str = "model_probe";

/// `model_probe` の本体 (v1)。役割・model_key・`build_info` ごとに 1 件 (新しいものを採る)。
/// 測っていない項目は null。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelProbeBody {
    pub role: String,
    pub model_key: String,
    pub build_info: String,
    /// chat: プロンプトの処理の速さ (トークン毎秒)。
    pub prompt_tps: Option<f64>,
    /// chat: 生成の速さ (トークン毎秒)。
    pub gen_tps: Option<f64>,
    /// chat: 補助の呼び出しの設定で、思考が出なかった。
    pub thinking_off: Option<bool>,
    /// chat: 型縛りの出力が、型に合う JSON だった (空にならなかった)。
    pub structured_ok: Option<bool>,
    /// chat: 設定に思考を切る指定が無く、点検が `chat_template_kwargs = { enable_thinking = false }` を足して
    /// 確かめ直して通った。補助の呼び出しにその指定を足す (06 §3.3、OQ-52、v0.4)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aux_thinking_off: Option<bool>,
    /// embed: 次元の数。
    pub dims: Option<u32>,
    /// embed: 問い 1 件の埋め込みの時間 (ms)。
    pub embed_ms: Option<u32>,
    /// rerank: 往復 6 件のリランクの時間 (ms)。
    pub rerank6_ms: Option<u32>,
    /// 点検が失敗した理由 (利用者に見せる文言ではない)。
    pub error: Option<String>,
}

/// リランクのしきい値と帯の下限 (05 §3.7、v0.2)。
pub const KIND_THRESHOLD: &str = "threshold";
/// しきい値の対象の種類: 以前の会話 (05 §3.4)。足すだけで広げる文字列。
pub const TARGET_CONVERSATION: &str = "conversation";
/// 求めた方法: 同梱の事例 (05 §3.7)。v0.4 から `signals`。
pub const METHOD_BUNDLED: &str = "bundled";

/// `threshold` の本体 (v1)。リランカーの model_key と対象の種類ごとに、版を上げて足していく
/// (前の版を残し、巻き戻せる)。採用した時刻は封筒の作成時刻。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThresholdBody {
    pub reranker_key: String,
    pub target: String,
    pub version: u32,
    pub method: String,
    /// 使った同梱の事例 (`ja-1` など)。変わったら求め直す。
    pub cases: String,
    /// 合否 (05 §3.7)。落ちたときも記録し (値は使わない)、起動のたびに求め直さない。
    pub passed: bool,
    pub threshold: Option<f64>,
    pub band_low: Option<f64>,
    /// 数えた値 (`rank_ok`・`rank_n`・`value_ok` など。足すだけで広げる)。
    #[serde(default)]
    pub stats: std::collections::BTreeMap<String, u32>,
}

/// モデルの元の値 (違いの中身を見せるため、06 §3.1)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOrigin {
    pub model_key: String,
    /// `/props` の `model_path` のファイル名 (フォルダを除く)。
    pub file_name: String,
    /// `/v1/models` の `data[0].id`。
    pub model_name: String,
    pub n_params: Option<u64>,
    pub size: Option<u64>,
    pub n_vocab: Option<u64>,
    /// 記録したときの llama-server の `build_info`。
    pub build_info: String,
}

/// `model_binding` の本体 (v1)。役割ごとに 1 件 (2 件以上あれば `updated_at` の新しいものを採る)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelBindingBody {
    /// `chat`・`embed`・`rerank`。知らない役割は読んで残し、使わない (03 §3.3)。
    pub role: String,
    pub current: ModelOrigin,
    /// 前に記録していたモデル (新しいものが後ろ)。前のモデルに戻したときに見分ける (06 §3.2 の 8)。
    pub history: Vec<ModelOrigin>,
}

/// `setting` の本体 (v1)。名前ごとに 1 件 (2 件以上あれば `updated_at` の新しいものを採る、03 §3.3)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingBody {
    /// 設定の名前 (`answer.language` など)。
    pub name: String,
    /// 値。知らない値の読み方は、名前ごとに決める (答え方の既定は `auto`、07 §3)。
    pub value: String,
}

/// `job` の本体 (v1)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobBody {
    /// 手順の名前 (`purge_backups` など)。
    pub task: String,
    /// 手順の対象の記録の ID。
    pub ids: Vec<Id>,
    /// `forget_backups` の job の、忘れるの指示の ID (03 §7.3)。ほかの手順では null。
    #[serde(default)]
    pub directive: Option<Id>,
}

fn link(from: Id, rel: &str, to: Id) -> WriteOp {
    WriteOp::PutLink {
        from,
        rel: rel.into(),
        to,
        attrs: Vec::new(),
    }
}

fn derive_session(e: &Envelope) -> Vec<WriteOp> {
    vec![WriteOp::PutLookup {
        kind: KIND_SESSION.into(),
        field: LOOKUP_UPDATED_AT.into(),
        value: OwnedPart::Int(e.updated_at.as_millis()),
        id: e.id,
    }]
}

/// ターンの本体のうち、関係を作るのに要る項目だけ (03 §4.2 の規則 11)。
///
/// 本体の全体 (`TurnBody`) では読まない。新しい版が書いた、この版では読めない値 (知らない列挙の値など)
/// を含むターンでも、セッションとの関係を失わないようにする。
#[derive(Deserialize)]
struct TurnLinks {
    session: Id,
    #[serde(default)]
    reply_to: Option<Id>,
}

/// 信号の本体のうち、関係を作るのに要る項目だけ (03 §4.2 の規則 11)。
#[derive(Deserialize)]
struct SignalLinks {
    target: Id,
}

fn derive_turn(e: &Envelope) -> Vec<WriteOp> {
    let Ok(b) = e.body_as::<TurnLinks>() else {
        return Vec::new();
    };
    let mut ops = vec![link(e.id, REL_IN_SESSION, b.session)];
    if let Some(u) = b.reply_to {
        ops.push(link(e.id, REL_REPLY_TO, u));
    }
    ops
}

fn derive_feedback(e: &Envelope) -> Vec<WriteOp> {
    match e.body_as::<RecallFeedbackBody>() {
        Ok(b) => vec![
            link(e.id, REL_FEEDBACK_ANSWER, b.answer),
            link(e.id, REL_FEEDBACK_TARGET, b.target),
        ],
        Err(_) => Vec::new(),
    }
}

fn derive_signal(e: &Envelope) -> Vec<WriteOp> {
    match e.body_as::<SignalLinks>() {
        Ok(b) => vec![link(e.id, REL_TARGET, b.target)],
        Err(_) => Vec::new(),
    }
}

/// チャンクの本体のうち、関係を作るのに要る項目だけ (03 §4.2 の規則 11)。
#[derive(Deserialize)]
struct ChunkLinks {
    turn: Id,
}

fn derive_chunk(e: &Envelope) -> Vec<WriteOp> {
    match e.body_as::<ChunkLinks>() {
        Ok(b) => vec![link(e.id, REL_CHUNK_OF, b.turn)],
        Err(_) => Vec::new(),
    }
}

/// 埋め込みの本体のうち、関係を作るのに要る項目だけ。
#[derive(Deserialize)]
struct EmbeddingLinks {
    target: Id,
}

fn derive_embedding(e: &Envelope) -> Vec<WriteOp> {
    match e.body_as::<EmbeddingLinks>() {
        Ok(b) => vec![link(e.id, REL_EMBEDDING_OF, b.target)],
        Err(_) => Vec::new(),
    }
}

/// ノートの本体のうち、関係を作るのに要る項目だけ。
#[derive(Deserialize)]
struct NoteLinks {
    exchanges: Vec<Id>,
}

fn derive_note(e: &Envelope) -> Vec<WriteOp> {
    match e.body_as::<NoteLinks>() {
        Ok(b) => b
            .exchanges
            .into_iter()
            .map(|t| link(e.id, REL_NOTE_OF, t))
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Active,
}

/// `session` の本体 (v1)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBody {
    pub title: String,
    /// v0.1 は `chat` だけ (コーディングモードは v1.0 より後、ADR-0016)。
    pub mode: String,
    pub state: SessionState,
    /// このセッションでメモリを添えるか (`/sources`、FR-409)。無ければ設定 `memory.enabled`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnRole {
    User,
    Assistant,
    System,
}

/// ターンの状態 (03 §3.3)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    /// 発話: 送った。応答をまだ書いていない (起動したときにこのままなら「応答なし」)。
    Pending,
    /// 発話: 応答を書いた。
    Confirmed,
    /// 応答: 最後まで受け取った。
    Completed,
    /// 応答: 利用者が中断した。受け取ったところまでの本文を持つ。
    Interrupted,
    /// 応答: 失敗した。受け取ったところまでの本文と理由を持つ。
    Failed,
}

/// 生成の設定の記録 (06 §7)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerationRecord {
    pub temperature: f64,
    pub top_p: f64,
    pub max_tokens: Option<u32>,
    pub seed: Option<u64>,
}

/// `turn` の本体 (v1)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnBody {
    pub session: Id,
    pub role: TurnRole,
    pub state: TurnState,
    pub text: String,
    /// 応答を作ったモデル (06 §5、FR-508)。発話では null。
    pub model_key: Option<String>,
    pub generation: Option<GenerationRecord>,
    /// 応答に添えたもの (以前の会話のターンなど) の ID。
    pub attached: Vec<Id>,
    /// 応答の対象の発話 (応答だけ)。
    pub reply_to: Option<Id>,
    /// 中断・失敗の理由 (利用者に見せる文言ではなく、分類と詳細)。
    pub error: Option<String>,
    /// 想起の判定点と出典の区切り (応答だけ、05 §3.4・§5)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall: Option<RecallRecord>,
    /// 後から作り直せない観測値 (応答だけ、v0.2、05 §3.6)。判定点 (`recall`) の中には足さない
    /// (古い版が判定点を書き戻しても落ちないように、本体の最上位に置く、03 §3.3)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obs: Option<TurnObs>,
}

/// 応答のターンの観測値 (05 §3.6)。項目はすべて任意で、足すだけにする。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnObs {
    /// このセッションの出典の通し番号の始まり (05 §5.1)。無いターン (v0.1) は 1 から。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_base: Option<u32>,
    /// 履歴に入れた最も古いターン (05 §4.2)。次のターンは、ここから履歴を組み立てる。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_from: Option<Id>,
    /// 想起の引用を添えた位置 (`before_utterance`)。添えなかったら無い。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall_position: Option<String>,
    /// 選んでいた答え方の既定 (`language`・`length`・`format` → 値、07 §3)。
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub answer: std::collections::BTreeMap<String, String>,
    /// リランクした往復と、その生の値 (高い順、05 §3.6)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reranked: Vec<Reranked>,
    /// 採点したリランカーの model_key。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reranker: Option<String>,
    /// 使ったしきい値の記録の版 (05 §3.7)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_version: Option<u32>,
    /// 帯に出した往復 (05 §3.4)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub band: Vec<Id>,
    /// 利用者が帯から選んで添えた往復 (05 §5.2 の「添えて作り直す」)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_attached: Vec<Id>,
    /// 縮退の理由 (`no_embed`・`no_rerank` など、05 §3.5)。値は足すだけで、知らない値はそのまま読む。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded: Vec<String>,
    /// 置いた事実の表の版 (表の本文の SHA-256 の 16 進、04 §5.4、v0.3)。表を置かなかったら無い。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fact_table: Option<String>,
    /// 候補の上位 (融合の順。往復の代表のターンと、語彙とベクトルのそれぞれの順位、05 §3.6、v0.4)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<RecallCandidate>,
    /// 引用に詰めた窓 (05 §3.8、v0.4)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packed: Option<Packed>,
    /// 問いの分析の結果 (05 §3.1 の 5、v0.4)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<QueryObs>,
    /// 先読みの当たりと、応答が始まるまでの時間 (05 §4.5、v0.4)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefetch: Option<PrefetchObs>,
    /// 言い直して引けた問い: 直前の問いの候補に無かった往復をこの問いで添えたときの、直前の問いのターン
    /// (別名の鍵の材料、v0.4 は記録だけ)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rephrase: Option<Id>,
    /// 想起・文脈の作りの版 (名前 → 版)。
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub versions: std::collections::BTreeMap<String, String>,
    /// 発話のときの UTC からのずれ (分。発話のターン、v0.4)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz_offset: Option<i32>,
    /// 同じセッションの前の発話からの時間 (秒。発話のターン、v0.4)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<u64>,
    /// 起動の突き合わせを終えた時点で残っていた裏の処理の手順 (起動の後の最初の発話のターン、v0.4)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_at_start: Option<Vec<String>>,
}

/// 候補の 1 つ (05 §3.6、v0.4)。順位は 1 から。その順位の列に無ければ無い。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecallCandidate {
    pub id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lex: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vec: Option<u16>,
}

/// 引用に詰めた窓 (05 §3.8、v0.4)。本文は持たない。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Packed {
    /// 窓の並び (ターン、その改訂の番号、本文のバイト位置の範囲)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<WindowRef>,
    /// 引用の枠のトークン数。
    pub tokens: u32,
    /// 使った予算 (トークン)。
    pub budget: u32,
}

/// 窓 1 つ。`start..end` はターンの本文 (改訂の番号 `rev` のとき) のバイト位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowRef {
    pub turn: Id,
    #[serde(default)]
    pub rev: u32,
    pub start: u32,
    pub end: u32,
}

/// 問いの分析の結果 (05 §3.1 の 5、v0.4)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryObs {
    #[serde(default)]
    pub count: bool,
    #[serde(default)]
    pub date: bool,
    #[serde(default)]
    pub explicit: bool,
    /// 件数の意図で候補と予算を広げた。
    #[serde(default)]
    pub widened: bool,
}

/// 先読みの当たり (05 §4.5、v0.4)。`kind` は `a`・`b`・`c`・`none` (値は足すだけ。知らない値はそのまま読む)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrefetchObs {
    pub kind: String,
    /// 応答の経路を始めてから最初の本文の断片まで (ミリ秒)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u32>,
    /// llama-server が処理したプロンプトのトークン数 (`timings.prompt_n`)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processed: Option<u32>,
}

/// 先読みが無かった (05 §4.5)。
pub const PREFETCH_NONE: &str = "none";

/// リランクした 1 つの往復。`id` は往復を代表するターン (利用者の発話。発話の無い往復は、そのターン)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Reranked {
    pub id: Id,
    pub score: f32,
}

/// 縮退の理由 (05 §3.5)。
pub const DEGRADED_NO_EMBED: &str = "no_embed";
pub const DEGRADED_NO_RERANK: &str = "no_rerank";
pub const DEGRADED_RERANK_SLOW: &str = "rerank_slow";
pub const DEGRADED_EMBED_DEADLINE: &str = "embed_deadline";
pub const DEGRADED_RERANK_DEADLINE: &str = "rerank_deadline";
pub const DEGRADED_RERANK_ERROR: &str = "rerank_error";
pub const DEGRADED_NO_THRESHOLD: &str = "no_threshold";
pub const DEGRADED_NO_INDEX: &str = "no_index";

/// 想起の引用を、今の発話の直前 (同じメッセージの中) に添えた (05 §4.1、v0.2・v0.3)。
pub const RECALL_BEFORE_UTTERANCE: &str = "before_utterance";
/// 想起の引用を、今の発話の直前の別のメッセージとして添えた (05 §4.1、v0.4、OQ-50)。
pub const RECALL_SEPARATE_MESSAGE: &str = "separate_message";

/// 判定点 (05 §3.4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecallDecision {
    /// 添えた。
    Fire,
    /// 候補はあったが、しきい値を超えなかった。
    Abstain,
    /// 索引が無くて引けなかった。
    Skip,
}

/// 応答のときの想起の記録。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecallRecord {
    pub decision: RecallDecision,
    /// 出典の番号の順に、`attached` の中のその出典のターンの数 (出典 n は `attached` の
    /// 先頭から数えた n 番目の区切り)。
    pub sources: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalKind {
    Good,
    Bad,
    Retry,
}

/// `signal` の本体 (v1、13 §2)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalBody {
    pub kind: SignalKind,
    pub target: Id,
    /// `/bad 理由` の理由。
    pub reason: Option<String>,
}

#[cfg(test)]
mod derived_names {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;

    use evorag_types::{Clock, IdGen, ManualClock, Timestamp, UuidV7Gen};

    use super::*;
    use crate::write::{AssistantTurn, Writes};

    /// 関係の名前と副索引の項目 (種類の名前の下の `field`) を、記録の種類ごとに集めたもの。
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    struct Names {
        rels: BTreeSet<String>,
        lookups: BTreeSet<String>,
    }

    impl Names {
        fn extend(&mut self, other: Self) {
            self.rels.extend(other.rels);
            self.lookups.extend(other.lookups);
        }
    }

    /// 1 つの依頼の操作から、記録の種類ごとの名前を集める (関係の持ち主は `from`、副索引の持ち主は `id`)。
    /// 副索引の種類の名前は、持ち主の記録の種類と同じであること。
    fn names_by_kind(ops: &[WriteOp], kinds_of: &BTreeMap<Id, String>) -> BTreeMap<String, Names> {
        let mut out: BTreeMap<String, Names> = BTreeMap::new();
        for op in ops {
            match op {
                WriteOp::PutLink { from, rel, .. } => {
                    let kind = kinds_of.get(from).expect("a link from a written record");
                    out.entry(kind.clone())
                        .or_default()
                        .rels
                        .insert(rel.clone());
                }
                WriteOp::PutLookup {
                    kind, field, id, ..
                } => {
                    let owner = kinds_of.get(id).expect("a lookup of a written record");
                    assert_eq!(kind, owner, "a lookup is filed under its record's kind");
                    out.entry(kind.clone())
                        .or_default()
                        .lookups
                        .insert(field.clone());
                }
                _ => {}
            }
        }
        out
    }

    fn declared(spec: &KindSpec) -> Names {
        Names {
            rels: spec.derived_rels.iter().map(|s| (*s).to_owned()).collect(),
            lookups: spec
                .derived_lookups
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        }
    }

    fn sample(ids: &dyn IdGen, kind: &str, body: &serde_json::Value) -> Envelope {
        let mut e = Envelope::new(ids.next_id(), kind, 1, Timestamp::from_millis(0), "");
        e.set_body(body).expect("sample body");
        e
    }

    fn put_of(ops: &[WriteOp], id: Id) -> Envelope {
        ops.iter()
            .find_map(|op| match op {
                WriteOp::Put(e) if e.id == id => Some(e.clone()),
                _ => None,
            })
            .expect("the written record")
    }

    /// 応答の経路の書き込み (`Writes`) が作る記録の例と、事実・訂正・指示の本体の例 (どの項目も埋める)。依頼ごとの
    /// 操作の列。
    fn samples() -> Vec<Vec<WriteOp>> {
        let clock: Arc<dyn Clock> =
            Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
        let ids = UuidV7Gen::with_seed(clock.clone(), 3);
        let w = Writes {
            clock: &*clock,
            ids: &ids,
        };
        let mut out = Vec::new();
        let (session_id, ops) = w.create_session("題").expect("session");
        let session = put_of(&ops, session_id);
        out.push(ops);
        let (user_id, ops) = w.user_turn(&session, "発話").expect("user turn");
        let user = put_of(&ops, user_id);
        out.push(ops);
        let (answer, ops) = w
            .assistant_turn(
                &session,
                &user,
                AssistantTurn {
                    state: TurnState::Completed,
                    text: "応答".into(),
                    model_key: None,
                    generation: None,
                    attached: Vec::new(),
                    error: None,
                    recall: None,
                    obs: None,
                },
            )
            .expect("assistant turn");
        out.push(ops);
        out.push(w.signal(SignalKind::Good, answer, None).expect("signal").1);
        out.push(
            w.recall_feedback(&RecallFeedbackBody {
                op: "pin".into(),
                answer,
                target: user_id,
                score: None,
                reranker: None,
            })
            .expect("feedback")
            .1,
        );
        let chunk = ChunkBody {
            turn: user_id,
            session: session_id,
            start: 0,
            end: 1,
            rules: 1,
            src_rev: None,
        };
        out.push(w.chunks(&[], &[chunk]).expect("chunks").1);
        let embedding = EmbeddingBody {
            target: user_id,
            unit: "chunk".into(),
            session: session_id,
            model_key: "m".into(),
            dim: 1,
            key: None,
            src_rev: None,
        };
        out.push(w.embedding(&embedding, &[0.0]).expect("embedding").1);
        let note = NoteBody {
            session: session_id,
            exchanges: vec![user_id, answer],
            keys: vec!["鍵".into()],
            model_key: "m".into(),
            rules: 1,
            src_revs: Vec::new(),
            forgets_seen: None,
        };
        out.push(w.note(&[], &[], &note).expect("note").1);
        // 事実・訂正・指示は、本体の項目をすべて埋めた例から `derive` で作る (書き込みの経路も同じ関数
        // (`fact_entries`・`directive_entries`・`derive_correction`) で作る)。
        let vk = "00".repeat(16);
        let examples = [
            sample(
                &ids,
                crate::facts::KIND_FACT,
                &serde_json::json!({
                    "subject": "user", "attribute": "lives_in", "vk": vk,
                    "sources": [{"turn": user_id, "rev": 0, "start": null, "end": null}],
                }),
            ),
            sample(
                &ids,
                crate::correction::KIND_CORRECTION,
                &serde_json::json!({"turn": user_id, "previous": answer}),
            ),
            sample(
                &ids,
                crate::facts::KIND_FACT_DIRECTIVE,
                &serde_json::json!({
                    "op": "forget", "subject": "user", "attribute": "lives_in", "vk": vk,
                    "marks": ["ab"], "target": answer,
                }),
            ),
        ];
        let specs: BTreeMap<&str, KindSpec> = kinds().into_iter().map(|k| (k.name, k)).collect();
        for e in examples {
            let derive = specs[e.kind.as_str()].derive.expect("derive");
            let mut ops = vec![WriteOp::Put(e.clone())];
            ops.extend(derive(&e));
            out.push(ops);
        }
        out
    }

    /// v0.5 の種類の、どの項目も埋めた本体の例 (`private` でない単位と、`private` の単位)。
    fn v05_examples(ids: &dyn IdGen) -> Vec<Envelope> {
        let t = |n: u8| Id::from_bytes([n; 16]);
        let span = serde_json::json!({"turn": t(1), "rev": 0, "start": 0, "end": 3});
        let unit = |private: bool| {
            sample(
                ids,
                crate::unit::KIND_UNIT,
                &serde_json::json!({
                    "entity": "self", "aspect": "residence", "vk": "ab".repeat(32),
                    "private": private,
                    "when": {"from": "2026-01", "to": null, "basis": "stated"},
                    "anchors": [{"turn": t(1)}, {"turn": t(2)}],
                }),
            )
        };
        vec![
            sample(
                ids,
                crate::directive::KIND_DIRECTIVE,
                &serde_json::json!({
                    "op": "forget", "entity": "self", "vks": ["ab", "cd"], "at": span,
                    "target": t(9), "turns": [t(1)], "redactions": [{"turn": t(2)}],
                    "hidden": [span], "turn": t(3),
                }),
            ),
            sample(
                ids,
                crate::segment::KIND_SEGMENT,
                &serde_json::json!({"session": t(8), "pairs": [t(1), t(2)]}),
            ),
            sample(
                ids,
                crate::ledger::KIND_PROPOSAL,
                &serde_json::json!({
                    "segment": t(7), "model_key": "m", "variant": "v1",
                    "sentences": [{"turn": t(1)}], "inputs": {"refs": [{"turn": t(2)}]},
                }),
            ),
            unit(false),
            unit(true),
        ]
    }

    /// v0.5 の種類の `derive` は、宣言したすべての名前を作りうり、宣言の外の名前を作らない。private の単位は `vk` と
    /// `date` の副索引を持たない (03 §3.3)。v0.5 の根は v0.4 だけの種類を持たない。
    #[test]
    fn v05_derived_names_are_declared_on_the_kind() {
        let specs: BTreeMap<&str, KindSpec> =
            kinds_v05().into_iter().map(|k| (k.name, k)).collect();
        for name in V04_ONLY_KINDS {
            assert!(
                !specs.contains_key(name),
                "{name} is not made in a v0.5 root"
            );
        }
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
        let ids = UuidV7Gen::with_seed(clock, 4);
        let mut derived: BTreeMap<String, Names> = BTreeMap::new();
        for e in v05_examples(&ids) {
            let spec = &specs[e.kind.as_str()];
            let kinds_of: BTreeMap<Id, String> = [(e.id, e.kind.clone())].into();
            let names = names_by_kind(&spec.derive.expect("derive")(&e), &kinds_of);
            let got = names.get(&e.kind).cloned().unwrap_or_default();
            #[derive(serde::Deserialize)]
            struct Flag {
                #[serde(default)]
                private: bool,
            }
            if e.kind == crate::unit::KIND_UNIT && e.body_as::<Flag>().is_ok_and(|f| f.private) {
                assert!(
                    !got.lookups.contains(crate::unit::LOOKUP_UNIT_VK)
                        && !got.lookups.contains(crate::unit::LOOKUP_UNIT_DATE),
                    "{got:?}"
                );
            }
            derived.entry(e.kind.clone()).or_default().extend(got);
        }
        for name in [
            crate::directive::KIND_DIRECTIVE,
            crate::segment::KIND_SEGMENT,
            crate::ledger::KIND_PROPOSAL,
            crate::unit::KIND_UNIT,
        ] {
            assert_eq!(derived[name], declared(&specs[name]), "{name}");
        }
        // 退避と書き出しに写さない種類は、ストアの層の一覧と同じ (03 §7.1・§11)。
        assert_eq!(
            evorag_store::backup::UNCOPIED_KINDS,
            [crate::unit::KIND_UNIT]
        );
    }

    /// 書き込みの経路と `derive` が作る関係の名前と副索引の項目は、どれも種類の宣言 (`derived_rels`・
    /// `derived_lookups`) に入る。`derive` は、宣言したすべての名前を作りうる (宣言と `derive` が合っている)。
    /// `derive` の無い種類は何も宣言せず、書き込みの経路も関係と副索引を作らない。`repair` は宣言した名前の行だけを
    /// 消して作り直すため (03 §10)。
    #[test]
    fn written_and_derived_names_are_declared_on_the_kind() {
        let specs: BTreeMap<&str, KindSpec> = kinds().into_iter().map(|k| (k.name, k)).collect();
        let mut written: BTreeMap<String, Names> = BTreeMap::new();
        let mut derived: BTreeMap<String, Names> = BTreeMap::new();
        for ops in samples() {
            let kinds_of: BTreeMap<Id, String> = ops
                .iter()
                .filter_map(|op| match op {
                    WriteOp::Put(e) => Some((e.id, e.kind.clone())),
                    _ => None,
                })
                .collect();
            for (kind, names) in names_by_kind(&ops, &kinds_of) {
                written.entry(kind).or_default().extend(names);
            }
            for op in &ops {
                let WriteOp::Put(e) = op else { continue };
                let Some(derive) = specs[e.kind.as_str()].derive else {
                    continue;
                };
                for (kind, names) in names_by_kind(&derive(e), &kinds_of) {
                    derived.entry(kind).or_default().extend(names);
                }
            }
        }
        for (name, spec) in &specs {
            let want = declared(spec);
            if spec.derive.is_none() {
                assert_eq!(
                    want,
                    Names::default(),
                    "{name} declares names without derive"
                );
                assert!(!written.contains_key(*name), "{name}: {written:?}");
                continue;
            }
            if let Some(w) = written.get(*name) {
                assert!(
                    w.rels.is_subset(&want.rels) && w.lookups.is_subset(&want.lookups),
                    "{name}: written {w:?}, declared {want:?}"
                );
            }
            let d = derived
                .get(*name)
                .unwrap_or_else(|| panic!("no sample of {name}"));
            assert_eq!(d, &want, "{name}: derive and the declaration differ");
        }
    }
}
