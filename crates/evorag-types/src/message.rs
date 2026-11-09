//! 文言キー (`MessageKey`)。コアは利用者に見せる文言を文言キーと引数で返し、画面がカタログで訳す
//! (02 §3 の決めごと 1、07 §6)。
//!
//! キーは `message_keys!` の中だけで定義する。`cargo xtask i18n` が `MessageKey::ALL` と
//! `locales/<言語>.toml` を突き合わせ、差分があれば失敗にする (NFR-602)。
//! 値は足すだけにし、既存のキーの文字列の意味を変えない (02 §3 の決めごと 3)。

macro_rules! message_keys {
    ($($(#[$meta:meta])* $variant:ident => $key:literal,)*) => {
        /// 利用者に見せる文言のキー。文字列は `locales/<言語>.toml` の点でつないだ名前。
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[non_exhaustive]
        pub enum MessageKey {
            $($(#[$meta])* $variant,)*
        }

        impl MessageKey {
            /// 定義したすべてのキー (定義の順)。
            pub const ALL: &'static [MessageKey] = &[$(MessageKey::$variant,)*];

            /// カタログでの名前。
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(MessageKey::$variant => $key,)*
                }
            }
        }
    };
}

message_keys! {
    /// 引数: `detail`。
    LlmConnect => "error.llm.connect",
    /// 引数: `status`、`detail`。
    LlmHttp => "error.llm.http",
    /// 引数: `detail`。
    LlmStreamCut => "error.llm.stream_cut",
    /// 引数: `detail`。
    LlmServer => "error.llm.server",
    /// 引数: `detail`。
    LlmProtocol => "error.llm.protocol",
    /// 発話を保存できず、送らなかった (04 §3 の 1)。引数: `detail`。
    SendNotSaved => "error.send.not_saved",
    /// 応答を保存できなかった。引数: `detail`。
    ResponseNotSaved => "error.response.not_saved",
    /// 生成中に、もう 1 つ送ろうとした。
    Busy => "error.busy",
    /// セッションを開いていない。
    NoSession => "error.no_session",
    /// 作り直す・送り直す発話が無い。
    NothingToRetry => "error.nothing_to_retry",
    /// 利用者が中断した。
    Interrupted => "response.interrupted",
    /// 終了のために中断した。
    InterruptedByShutdown => "response.interrupted_by_shutdown",
}

/// 文言キーと引数 (02 §3 の決めごと 1)。画面がカタログで訳す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub key: MessageKey,
    pub args: Vec<(String, String)>,
}

impl Message {
    pub fn new(key: MessageKey) -> Self {
        Self {
            key,
            args: Vec::new(),
        }
    }

    pub fn arg(mut self, name: &str, value: impl Into<String>) -> Self {
        self.args.push((name.to_owned(), value.into()));
        self
    }
}
