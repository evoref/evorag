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
    /// 評価する応答が無い。
    NothingToRate => "error.nothing_to_rate",
    /// セッションを消せなかった。引数: `detail`。
    DeleteFailed => "error.delete_failed",
    /// 空きが足りないので裏の処理を止めた。引数: `free`、`need` (MiB)。
    LowSpace => "notice.low_space",
    /// 引数: `detail`。
    CliUnsafeLocation => "cli.unsafe_location",
    /// 引数: `detail`。
    CliLocationWarning => "cli.location_warning",
    /// 利用者が中断した。
    Interrupted => "response.interrupted",
    // 画面の文言 (07 §2)。
    UiYou => "ui.you",
    UiAssistant => "ui.assistant",
    UiMarkInterrupted => "ui.mark.interrupted",
    UiMarkFailed => "ui.mark.failed",
    UiMarkNoReply => "ui.mark.no_reply",
    UiStatusIdle => "ui.status.idle",
    UiStatusGenerating => "ui.status.generating",
    /// 引数: `title`。
    UiTitleSession => "ui.title.session",
    UiTitleNoSession => "ui.title.no_session",
    UiSessionsTitle => "ui.sessions.title",
    UiSessionsEmpty => "ui.sessions.empty",
    /// 引数: `title`。
    UiDeleteConfirm => "ui.delete.confirm",
    UiHelpTitle => "ui.help.title",
    UiHelpBody => "ui.help.body",
    UiNoticeRatedGood => "ui.notice.rated_good",
    UiNoticeRatedBad => "ui.notice.rated_bad",
    UiNoticeDeleted => "ui.notice.deleted",
    /// 引数: `title`。
    UiNoticeRenamed => "ui.notice.renamed",
    /// 引数: `name`。
    UiUnknownCommand => "ui.error.unknown_command",
    /// 引数: `name`。
    UiNotYet => "ui.error.not_yet",
    UiQuitHint => "ui.quit_hint",
    UiDefaultSessionTitle => "ui.default_session_title",
    // コマンドの行 (07 §5)。
    /// 引数: `url`、`detail`。
    CliLlmUnreachable => "cli.llm_unreachable",
    /// 引数: `detail`。
    CliDataRootUnusable => "cli.data_root_unusable",
    /// 引数: `detail`。
    CliConfigError => "cli.config_error",
    /// 引数: `key`。
    CliConfigIgnored => "cli.config_ignored",
    /// 引数: `recorded`。
    CliPromptLanguageFixed => "cli.prompt_language_fixed",
    /// 引数: `detail`。
    CliError => "cli.error",
    /// 引数: `moved_to`、`reason`。
    CliBroken => "cli.broken",
    CliAskRestore => "cli.ask_restore",
    /// 引数: `from`。
    CliRestored => "cli.restored",
    CliRestoredFromJournal => "cli.restored_from_journal",
    // `evorag doctor` と `evorag backups list` (03 §10)。
    /// 引数: `path`。
    DoctorConfig => "doctor.config",
    /// 引数: `url`, `model`, `n_ctx`, `build`。
    DoctorLlmOk => "doctor.llm_ok",
    /// 引数: `url`, `detail`。
    DoctorLlmNg => "doctor.llm_ng",
    /// 引数: `path`。
    DoctorRoot => "doctor.root",
    DoctorDbMissing => "doctor.db_missing",
    /// 引数: `format`, `writer`。
    DoctorDbOk => "doctor.db_ok",
    /// 引数: `detail`。
    DoctorDbRepair => "doctor.db_repair",
    /// 引数: `detail`。
    DoctorDbBad => "doctor.db_bad",
    /// 引数: `kind`, `count`。
    DoctorRecords => "doctor.records",
    /// 引数: `unreadable`, `corrupt`, `newer`。
    DoctorProblemRecords => "doctor.problem_records",
    /// 引数: `count`。
    DoctorUnknownKinds => "doctor.unknown_kinds",
    /// 引数: `count`。
    DoctorQuarantine => "doctor.quarantine",
    /// 引数: `files`, `lines`, `bad`, `newer`, `partial`, `pending`。
    DoctorJournal => "doctor.journal",
    /// 引数: `count`, `latest`。
    DoctorBackups => "doctor.backups",
    DoctorBackupsNone => "doctor.backups_none",
    /// 引数: `count`, `path`。
    DoctorBroken => "doctor.broken",
    /// 引数: `kind`, `dir`, `seq`。
    BackupsItem => "backups.item",
    BackupsNone => "backups.none",
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
