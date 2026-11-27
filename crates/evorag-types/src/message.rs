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
    /// 引数: `name`。
    ModelSwitchInSetup => "error.model_switch_in_setup",
    ModelChanged => "error.model_changed",
    CliModelMismatch => "cli.model_mismatch",
    CliModelMismatchSameMeta => "cli.model_mismatch_same_meta",
    CliModelRecorded => "cli.model_recorded",
    CliModelRecordedUnusual => "cli.model_recorded_unusual",
    CliConfigObsolete => "cli.config_obsolete",
    UiModelRole => "ui.notice.model_role",
    CliRoleRecorded => "cli.role_recorded",
    CliSetupTuiRunning => "cli.setup.tui_running",
    CliSetupRoleFound => "cli.setup.role_found",
    CliSetupRoleMissing => "cli.setup.role_missing",
    CliSetupEmbedExample => "cli.setup.embed_example",
    CliSetupRerankExample => "cli.setup.rerank_example",
    CliSetupExtraChat => "cli.setup.extra_chat",
    CliSetupModelRecorded => "cli.setup.model_recorded",
    CliSetupModelDiff => "cli.setup.model_diff",
    CliSetupRedoChat => "cli.setup.redo_chat",
    CliSetupRedoEmbed => "cli.setup.redo_embed",
    CliSetupRedoRerank => "cli.setup.redo_rerank",
    CliSetupAskSame => "cli.setup.ask_same",
    CliSetupAskSwitch => "cli.setup.ask_switch",
    CliSetupSwitched => "cli.setup.switched",
    CliSetupKeptChat => "cli.setup.kept_chat",
    CliSetupKept => "cli.setup.kept",
    CliSetupNeedsConfirm => "cli.setup.needs_confirm",
    /// 引数: `url`, `models`。
    CliSetupRouterFound => "cli.setup.router_found",
    /// 引数: `model`。
    CliSetupRouterChecking => "cli.setup.router_checking",
    /// 引数: `role`, `choices`。
    CliSetupRouterChoose => "cli.setup.router_choose",
    CliSetupPresetExample => "cli.setup.preset_example",
    /// 引数: `role`。
    CliSetupProbing => "cli.setup.probing",
    CliRoleMismatch => "cli.role_mismatch",
    CliRoleWaiting => "cli.role_waiting",
    /// 引数: `url`, `models`。
    CliRouterNeedsModel => "cli.router_needs_model",
    /// 引数: `url`, `model`, `detail`。
    CliRouterModel => "cli.router_model",
    UiRoleReady => "ui.notice.role_ready",
    UiRoleStatusUnset => "ui.role_status.unset",
    UiRoleStatusWaiting => "ui.role_status.waiting",
    UiRoleStatusReady => "ui.role_status.ready",
    UiRoleStatusMismatch => "ui.role_status.mismatch",
    /// モデルの点検で使えない機能が見つかった (06 §3.3)。引数: `role`, `reasons`。
    UiProbeIssues => "ui.notice.probe_issues",
    UiProbeThinkingNotOff => "probe.thinking_not_off",
    UiProbeStructuredBroken => "probe.structured_broken",
    /// 裏の処理で事実が変わった (04 §5.3 の「知らせ」)。引数: `parts` (下の 3 つを並べたもの)。
    UiFactsChanged => "ui.notice.facts.changed",
    /// 引数: `n`。
    UiFactsAdded => "ui.notice.facts.added",
    /// 引数: `n`。
    UiFactsReplaced => "ui.notice.facts.replaced",
    /// 引数: `n`。
    UiFactsContested => "ui.notice.facts.contested",
    /// 引数: `current`、`model`、`n_ctx`、`choices`。
    /// 出典が見つからない。引数: `n`。
    SourceNotFound => "error.source_not_found",
    StyleUnknownValue => "error.style_unknown_value",
    BandNotLatest => "error.band_not_latest",
    BandUnknown => "error.band_unknown",
    FeedbackNotSaved => "error.feedback_not_saved",
    UiStyleInfo => "ui.notice.style",
    /// 引数: `title`。
    UiBandSelected => "ui.notice.band_selected",
    UiIrrelevantMarked => "ui.notice.irrelevant_marked",
    UiIrrelevantCleared => "ui.notice.irrelevant_cleared",
    UiBandRelated => "ui.band.related",
    UiBandNearby => "ui.band.nearby",
    /// 引数: `title`, `at`, `head`。
    UiBandItem => "ui.band.item",
    UiBandMarked => "ui.band.marked",
    /// 引数: `title`, `at`, `excerpt`。
    UiBandDetail => "ui.band.detail",
    /// 引数: `n`。
    UiDetailSource => "ui.band.detail_source",
    UiActionAttach => "ui.band.action_attach",
    UiActionIrrelevant => "ui.band.action_irrelevant",
    UiActionIrrelevantUndo => "ui.band.action_irrelevant_undo",
    UiStyleChanged => "ui.notice.style_changed",
    UiStyleTitle => "ui.style.title",
    UiStyleLanguage => "ui.style.language",
    UiStyleLength => "ui.style.length",
    UiStyleFormat => "ui.style.format",
    UiStyleHint => "ui.style.hint",
    UiSourcesOn => "ui.notice.sources_on",
    UiSourcesOff => "ui.notice.sources_off",
    /// 引数: `n`。
    UiSourceSelected => "ui.notice.source_selected",
    /// 引数: `n`、`title`、`at`、`excerpt`。
    UiSourceDetail => "ui.notice.source",
    UiNoSources => "ui.notice.no_sources",
    /// `/memory` (FR-310)。
    UiMemoryHeader => "ui.memory.header",
    UiMemoryNoAnswer => "ui.memory.no_answer",
    UiMemoryNothing => "ui.memory.nothing",
    /// 引数: `n`・`title`・`head`・`score`。
    UiMemoryAttached => "ui.memory.attached",
    /// 引数: `title`・`head`・`score`。
    UiMemoryBand => "ui.memory.band",
    UiMemoryBelow => "ui.memory.below",
    /// 引数: `delta`・`raw`。
    UiMemoryScore => "ui.memory.score",
    /// 引数: `raw`。
    UiMemoryScoreRaw => "ui.memory.score_raw",
    UiMemorySources => "ui.memory.sources",
    /// 引数: `n`・`title`・`head`。
    UiMemorySource => "ui.memory.source",
    UiMemoryDegradedNoEmbed => "ui.memory.degraded_no_embed",
    UiMemoryDegradedEmbedDeadline => "ui.memory.degraded_embed_deadline",
    UiMemoryDegradedNoRerank => "ui.memory.degraded_no_rerank",
    UiMemoryDegradedRerankSlow => "ui.memory.degraded_rerank_slow",
    UiMemoryDegradedRerankDeadline => "ui.memory.degraded_rerank_deadline",
    UiMemoryDegradedRerankError => "ui.memory.degraded_rerank_error",
    UiMemoryDegradedNoThreshold => "ui.memory.degraded_no_threshold",
    UiMemoryDegradedNoIndex => "ui.memory.degraded_no_index",
    /// 引数: `reason`。
    UiMemoryDegradedOther => "ui.memory.degraded_other",
    // 事実の一覧と事実への操作 (04 §7.1・§7.4〜§7.6、07 §3)。
    /// 指した行が、一覧を表示した後に変わった (実行しない)。
    FactsOutdated => "error.facts.list_outdated",
    /// 操作の前提が合わない (private でない行への `/private off` など)。
    FactsNotAllowed => "error.facts.not_allowed",
    /// `memory.forget_key` が使えないので、事実への操作ができない。
    FactsDisabled => "error.facts.disabled",
    /// v0.5 の根を開いた開発中の版なので、事実への操作はまだできない (08 §2.6 の M5.1〜M5.3)。
    FactsDevelopmentBuild => "error.facts.development_build",
    /// 戻す値が忘れた値 (再び覚えない印に当たる)。
    FactsBlocked => "error.facts.blocked",
    /// 引数: `detail`。
    FactsNotSaved => "error.facts.not_saved",
    /// 伏せる値 (private・忘れた値) を確かめられないので、往復の本文を出さなかった (04 §7.5)。
    MaskUnavailable => "error.mask_unavailable",
    /// 番号の一覧をまだ出していない。引数: `list` (先に打つコマンド)。
    FactsNoList => "error.facts.no_list",
    /// 引数: `n`。
    FactsBadNumber => "error.facts.bad_number",
    UiFactsTitle => "ui.facts.title",
    UiFactsHeader => "ui.facts.header",
    UiFactsEmpty => "ui.facts.empty",
    UiFactsSelf => "ui.facts.self",
    /// 引数: `n`・`subject`・`attr`・`value`・`marks`。
    UiFactsRow => "ui.facts.row",
    /// 引数: `n`・`subject`・`attr`・`history`。
    UiFactsVacant => "ui.facts.vacant",
    /// 引数: `n`・`subject`・`attr`。
    UiFactsRuleOnly => "ui.facts.rule_only",
    UiFactsMarkContested => "ui.facts.mark_contested",
    UiFactsMarkPlanned => "ui.facts.mark_planned",
    UiFactsMarkChanged => "ui.facts.mark_changed",
    UiFactsMarkPrivateSuccessors => "ui.facts.mark_private_successors",
    UiFactsMarkPrivateValueOnly => "ui.facts.mark_private_value_only",
    UiFactsMarkPrivateSuccessor => "ui.facts.mark_private_successor",
    UiFactsMarkPrivateKept => "ui.facts.mark_private_kept",
    /// 引数: `n`。
    UiFactsMarkHistory => "ui.facts.mark_history",
    UiFactsNoteSleepOff => "ui.facts.note_sleep_off",
    UiFactsNoteProbe => "ui.facts.note_probe",
    UiFactsNoteKey => "ui.facts.note_key",
    UiFactsNoteDevelopmentBuild => "ui.facts.note_development_build",
    UiFactsHint => "ui.facts.hint",
    /// 引数: `subject`・`attr`。
    UiFactsHistoryTitle => "ui.facts.history_title",
    /// 引数: `n`・`value`・`at`・`marks`。
    UiFactsHistoryRow => "ui.facts.history_row",
    UiFactsHistoryEmpty => "ui.facts.history_empty",
    UiFactsHistoryHint => "ui.facts.history_hint",
    UiFactsMarkPrivate => "ui.facts.mark_private",
    UiFactsForgottenTitle => "ui.facts.forgotten_title",
    /// 引数: `n`・`subject`・`attr`・`at`・`values`・`how`・`sessions`。
    UiFactsForgottenRow => "ui.facts.forgotten_row",
    UiFactsForgottenRedacted => "ui.facts.forgotten_redacted",
    UiFactsForgottenKept => "ui.facts.forgotten_kept",
    /// 引数: `n`。
    UiFactsForgottenDeleted => "ui.facts.forgotten_deleted",
    UiFactsForgottenEmpty => "ui.facts.forgotten_empty",
    UiFactsForgottenHint => "ui.facts.forgotten_hint",
    UiFactsDonePrivate => "ui.facts.done_private",
    UiFactsDonePrivateOff => "ui.facts.done_private_off",
    UiFactsDoneAccept => "ui.facts.done_accept",
    UiFactsDoneRevert => "ui.facts.done_revert",
    UiFactsDoneUnblock => "ui.facts.done_unblock",
    UiFactsSlowNext => "ui.facts.slow_next",
    UiPanelHint => "ui.panel.hint",
    UiPrivateTitle => "ui.private.title",
    /// 引数: `label` (主語・属性・値)。
    UiPrivateBody => "ui.private.body",
    UiPrivateScopeSuccessors => "ui.private.scope_successors",
    UiPrivateScopeValueOnly => "ui.private.scope_value_only",
    UiPrivateMulti => "ui.private.multi",
    UiPrivateRemote => "ui.private.remote",
    UiPrivateHint => "ui.private.hint",
    UiPrivateOffTitle => "ui.private_off.title",
    /// 引数: `label` (主語・属性・値)。
    UiPrivateOffBody => "ui.private_off.body",
    UiPrivateOffOthers => "ui.private_off.others",
    /// 引数: `n`。
    UiPrivateOffHistory => "ui.private_off.history",
    UiPrivateOffHint => "ui.private_off.hint",
    // 忘れる (`/forget`、04 §7.1〜§7.3・§7.8、07 §3)。
    /// どれかのセッションで応答を生成しているので、忘れるを受け付けない。
    ForgetBusy => "error.forget.busy",
    /// 見本の後に計画が変わった (実行せずに新しい見本を出す)。
    ForgetOutdated => "error.forget.outdated",
    /// 伏せ字にできないので中止した (理由は、一緒に返す見本の警告)。
    ForgetAborted => "error.forget.aborted",
    /// 伏せ字にできない見本で Enter を押した。
    ForgetBlocked => "error.forget.blocked",
    /// 見本がまだ届いていない。
    ForgetCounting => "error.forget.counting",
    /// 忘れるを実行している間に、発話・作り直しを送ろうとした。
    ForgetInProgress => "error.forget.in_progress",
    /// 忘れるを書いた後に、事実の一覧を読み直せなかった (忘れるは済んでいる)。
    UiForgetListUnreadable => "ui.forget.list_unreadable",
    /// 忘れるの確認の画面を閉じたが、実行は続いている。
    UiForgetContinues => "ui.forget.continues",
    /// 確認の画面の外で忘れるが済み、`broken/` に置き場所が残っている。引数: `n`。
    UiForgetBrokenLater => "ui.forget.broken_later",
    /// 引数: `detail`。
    ForgetNotSaved => "error.forget.not_saved",
    UiForgetTitle => "ui.forget.title",
    /// 引数: `label` (主語・属性・値)。
    UiForgetTarget => "ui.forget.target",
    UiForgetCounting => "ui.forget.counting",
    UiForgetSamplesRedact => "ui.forget.samples_redact",
    UiForgetSamplesHide => "ui.forget.samples_hide",
    /// 引数: `who`・`text`。
    UiForgetSampleBefore => "ui.forget.sample_before",
    /// 引数: `text`。
    UiForgetSampleAfter => "ui.forget.sample_after",
    UiForgetNoSamples => "ui.forget.no_samples",
    /// 引数: `turns`・`matches`。
    UiForgetOthers => "ui.forget.others",
    /// 引数: `n`。
    UiForgetHistory => "ui.forget.history",
    /// 引数: `n`。
    UiForgetSkipped => "ui.forget.skipped",
    /// 引数: `n`。
    UiForgetUnredacted => "ui.forget.unredacted",
    /// 引数: `n`。
    UiForgetSourceOutside => "ui.forget.source_outside",
    /// 引数: `n`。
    UiForgetWhole => "ui.forget.whole",
    /// 引数: `n`。
    UiForgetMismatched => "ui.forget.mismatched",
    UiForgetAffected => "ui.forget.affected",
    /// 引数: `reason`。
    UiForgetWarning => "ui.forget.warning",
    UiForgetWarnUnreadable => "ui.forget.warn_unreadable",
    UiForgetWarnStale => "ui.forget.warn_stale",
    UiForgetWarnValue => "ui.forget.warn_value",
    UiForgetWarnMismatched => "ui.forget.warn_mismatched",
    UiForgetKeptNote => "ui.forget.kept_note",
    /// 引数: `summary`。
    UiForgetChoicesFolded => "ui.forget.choices_folded",
    UiForgetChoiceRedact => "ui.forget.choice_redact",
    UiForgetChoiceHistory => "ui.forget.choice_history",
    UiForgetChoiceEcho => "ui.forget.choice_echo",
    UiForgetChoiceOthers => "ui.forget.choice_others",
    UiForgetPromiseFolded => "ui.forget.promise_folded",
    UiForgetPromise => "ui.forget.promise",
    UiForgetHint => "ui.forget.hint",
    UiForgetHintOpen => "ui.forget.hint_open",
    UiForgetSending => "ui.forget.sending",
    /// 引数: `n`。
    UiForgetDone => "ui.forget.done",
    UiForgetDoneKept => "ui.forget.done_kept",
    /// 引数: `n`。
    UiForgetBroken => "ui.forget.broken",
    /// 引数: `n`。
    UiForgetBrokenRemoved => "ui.forget.broken_removed",
    /// 引数: `n`。
    UiForgetBrokenFailed => "ui.forget.broken_failed",
    /// 本文が忘れるの指示と合わないので表示しないターンの、本文の代わりの文 (04 §7.2)。
    UiMarkUnverified => "ui.mark.unverified",
    /// 伏せ字にしないで忘れた値の範囲を、表示するときに置き換える印 (04 §7.3)。
    UiMarkHidden => "ui.mark.hidden",
    /// 伏せ字の範囲を表示するときの印 (UI の言語の印。本文の印はプロンプトの言語のまま、04 §7.3)。
    UiMarkRedacted => "ui.mark.redacted",
    /// 引数: `count`。
    UiStatusForgetMismatched => "ui.status.forget_mismatched",
    UiStatusForgetJournal => "ui.status.forget_journal",
    /// 利用者が中断した。
    Interrupted => "response.interrupted",
    // 画面の文言 (07 §2)。
    UiYou => "ui.you",
    UiAssistant => "ui.assistant",
    UiMarkInterrupted => "ui.mark.interrupted",
    UiMarkFailed => "ui.mark.failed",
    UiMarkNoReply => "ui.mark.no_reply",
    UiStatusIdle => "ui.status.idle",
    UiStatusLimited => "ui.status.limited",
    UiStatusWorking => "ui.status.working",
    UiStatusPending => "ui.status.pending",
    UiStatusBattery => "ui.status.paused_battery",
    UiStatusLowSpace => "ui.status.paused_low_space",
    UiRecallStandard => "ui.status.recall_standard",
    UiRecallSimple => "ui.status.recall_simple",
    UiRecallCandidates => "ui.status.recall_candidates",
    UiRecallCalibrating => "ui.status.recall_calibrating",
    UiRecallFailed => "ui.status.recall_failed",
    /// 引数: `percent`。
    UiRecallRebuilding => "ui.status.recall_rebuilding",
    UiCalibrated => "ui.notice.calibrated",
    UiCalibrationFailed => "ui.notice.calibration_failed",
    /// 引数: `done`, `total`。
    CliSetupCalibrating => "cli.setup.calibrating",
    /// 引数: `threshold`, `band`, `rank_ok`, `rank_n`, `value_ok`, `value_n`。
    CliSetupCalibrated => "cli.setup.calibrated",
    /// 引数: `rank_ok`, `rank_n`, `value_ok`, `value_n`。
    CliSetupCalibrationFailed => "cli.setup.calibration_failed",
    UiStatusGenerating => "ui.status.generating",
    UiStatusThinking => "ui.status.thinking",
    UiThinking => "ui.thinking",
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
    /// 引数: `count`。
    UiNoticeUnreadableTurns => "ui.notice.unreadable_turns",
    /// 引数: `name`。
    UiUnknownCommand => "ui.error.unknown_command",
    /// 引数: `name`。
    UiNotYet => "ui.error.not_yet",
    UiQuitHint => "ui.quit_hint",
    UiDefaultSessionTitle => "ui.default_session_title",
    UiDraftRestored => "ui.notice.draft_restored",
    /// 引数: `detail`。
    UiEditorFailed => "ui.error.editor",
    // コマンドの行 (07 §5)。
    /// 引数: `url`、`detail`。
    CliLlmUnreachable => "cli.llm_unreachable",
    /// 引数: `detail`。
    CliDataRootUnusable => "cli.data_root_unusable",
    /// 引数: `detail`。
    CliConfigError => "cli.config_error",
    /// 設定が、この版の読めない新しい形式の版 (12 §7.2・§7.3)。
    CliConfigNewerHint => "cli.config_newer_hint",
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
    /// 引数: `backup`・`removed`・`docs`。
    CliRebuilt => "cli.rebuilt",
    /// 引数: `backup`・`docs`。
    CliRebuiltIndex => "cli.rebuilt_index",
    /// 引数: `detail`。
    CliRebuildFailed => "cli.rebuild_failed",
    /// `rebuild --facts` で抽出を始める。
    CliRebuildingFacts => "cli.rebuilding_facts",
    /// 引数: `backup`・`removed`・`docs`・`utterances`・`facts`・`old`・`skipped`・`filtered`・`too_long`・`mismatch`・
    /// `corrections`。
    CliRebuiltFacts => "cli.rebuilt_facts",
    /// 抽出できずに飛ばした発話がある (作り直しが部分的)。引数: `lost`・`truncated`・`invalid`。
    CliRebuiltFactsPartial => "cli.rebuilt_facts_partial",
    /// 引数: `url`・`detail`。
    CliRebuildFactsUnreachable => "cli.rebuild_facts_unreachable",
    /// 引数: `recorded`・`connected`。
    CliRebuildFactsModelMismatch => "cli.rebuild_facts_model_mismatch",
    CliRebuildFactsNoProbe => "cli.rebuild_facts_no_probe",
    CliRebuildFactsProbeFailed => "cli.rebuild_facts_probe_failed",
    /// 引数: `n_ctx`。
    CliRebuildFactsContextTooSmall => "cli.rebuild_facts_context_too_small",
    /// 引数: `detail`。
    CliRebuildFactsKeyUnusable => "cli.rebuild_facts_key_unusable",
    CliRebuildFactsForgetPending => "cli.rebuild_facts_forget_pending",
    /// 新しい版の evorag が書いた事実がある。引数: `count`。
    CliRebuildFactsNewer => "cli.rebuild_facts_newer",
    /// 抽出の要求を断られた (4xx)。引数: `url`・`detail`。
    CliRebuildFactsRejected => "cli.rebuild_facts_rejected",
    /// 締切切れか 5xx が続いた。引数: `url`・`attempts`・`detail`。
    CliRebuildFactsFailed => "cli.rebuild_facts_failed",
    /// 事実を入れ替えた後の派生データの作り直しに失敗した。引数: `backup`・`detail`。
    CliRebuildFailedAfterFacts => "cli.rebuild_failed_after_facts",
    /// 値の鍵が使えない理由 (`cli.rebuild_facts_key_unusable` の `detail` に入れる)。
    CliFactsKeyConflict => "cli.facts_key.conflict",
    CliFactsKeyMissing => "cli.facts_key.missing",
    CliFactsKeyMalformed => "cli.facts_key.malformed",
    CliFactsKeyReadOnly => "cli.facts_key.read_only",
    CliRestoredFromJournal => "cli.restored_from_journal",
    /// `repair` の結果。引数: `backup`・`replayed`・`deletes`・`restored`・`lower`・`quarantined`・`metas`・
    /// `bad_lines`・`links_added`・`links_removed`・`lookups_added`・`lookups_removed`。
    CliRepaired => "cli.repaired",
    /// `repair` の忘れるの当て直し。引数: `patched`・`retracted`・`deleted`・`mismatched`。
    CliRepairedForgets => "cli.repaired_forgets",
    /// ジャーナルの伏せ字の前の行を除けなかった。
    CliRepairJournalPending => "cli.repair_journal_pending",
    /// 値の鍵が使えないので、忘れた値の事実の取り消しを当てていない。
    CliRepairKeyMissing => "cli.repair_key_missing",
    /// 引数: `detail`。
    CliRepairFailed => "cli.repair_failed",
    /// ストアの層をコミットした後に失敗した。引数: `restored`・`links_added`・`links_removed`・`lookups_added`・
    /// `lookups_removed`・`detail`・`backup`。
    CliRepairFailedAfterStore => "cli.repair_failed_after_store",
    /// DB のファイルが壊れていたので、`repair` ではなく `restore` で戻す案内。
    CliRepairBrokenHint => "cli.repair_broken_hint",
    /// データ根のフォルダが無い (何も作らずに断った)。引数: `path`。
    CliDataRootMissing => "cli.data_root_missing",
    /// データ根が新しい形式なので書き換えない。引数: `min_write`。
    CliReadOnlyNewerFormat => "cli.read_only_newer_format",
    /// 古い形式のデータ根を移行できず、読むだけで開いた。引数: `reason`。
    CliReadOnlyMigrationFailed => "cli.read_only_migration_failed",
    /// llama-server が API キーを拒んだ。引数: `url`、`status`、`env`。
    CliLlmUnauthorized => "cli.llm_unauthorized",
    /// データ根をほかの evorag が使っている。引数: `path`、`pid`。
    CliDataRootInUse => "cli.data_root_in_use",
    /// 引数: `count`、`path`、`kinds`。
    CliExported => "cli.exported",
    /// 引数: `path`、`backup`。
    CliConfigMigrated => "cli.config_migrated",
    /// 引数: `tried`。
    CliSetupGuide => "cli.setup_guide",
    /// 引数: `url`。
    CliSetupNeedsKey => "cli.setup_needs_key",
    /// 引数: `url`、`model`、`n_ctx`、`build`。
    CliSetupFound => "cli.setup_found",
    /// 引数: `default` (Enter で選ぶ言語。UI の言語)。
    CliSetupAskLanguage => "cli.setup_ask_language",
    /// 引数: `path`、`backup`。
    CliSetupAskOverwrite => "cli.setup_ask_overwrite",
    /// 引数: `path`。
    CliSetupWritten => "cli.setup_written",
    /// 引数: `path`。
    CliSetupUnchanged => "cli.setup_unchanged",
    /// 引数: `path`。
    CliSetupNotWritten => "cli.setup_not_written",
    /// 引数: `newer`、`corrupt`。
    CliExportSkipped => "cli.export_skipped",
    /// 引数: `path`。
    CliExportNotEmpty => "cli.export_not_empty",
    /// 取り込んでいないジャーナルの行があるので書き出さない (03 §11)。引数: `count`。
    CliExportRefusedJournal => "cli.export_refused_journal",
    /// 当て直しの済んでいない忘れるがあるので書き出さない (03 §11)。
    CliExportRefusedForget => "cli.export_refused_forget",
    /// 空きが足りないので起動時の compact を見送った (03 §8)。引数: `free`、`need` (MiB)。
    CliCompactLowSpace => "cli.compact_low_space",
    /// 起動時の compact に失敗した (元のファイルのまま)。引数: `detail`。
    CliCompactFailed => "cli.compact_failed",
    /// 忘れるの指示と合わないターンがある (想起・裏の処理・書き出しから除く、04 §7.2)。引数: `count`。
    CliForgetMismatched => "cli.forget_mismatched",
    /// ジャーナルに伏せ字の前の行が残っている (書き直しの入れ替えに失敗した。sleep-time に試し直す、04 §7.8)。
    CliForgetJournalPending => "cli.forget_journal_pending",
    /// 再び覚えない印の鍵が使えないので、忘れた値に当たる事実の取り消しを当てていない (03 §3.1.1、04 §7.2)。
    CliForgetKeyMissing => "cli.forget_key_missing",
    // `evorag update` と、起動の更新の片付け (12 §7.2・§7.3)。
    /// 引数: `current`。
    CliUpdateUpToDate => "cli.update.up_to_date",
    /// 引数: `version`・`date`。
    CliUpdateAvailable => "cli.update.available",
    /// 引数: `version`・`date`。
    CliUpdateTarget => "cli.update.target",
    CliUpdateNotesHeader => "cli.update.notes_header",
    /// 引数: `url`。
    CliUpdateNotesPage => "cli.update.notes_page",
    /// 引数: `build`。
    CliUpdateLlamaMinBuild => "cli.update.llama_min_build",
    /// 引数: `min_write`・`current`。
    CliUpdateDbMinWrite => "cli.update.db_min_write",
    /// 引数: `key_id`。
    CliUpdateAddedKey => "cli.update.added_key",
    CliUpdateApplyHint => "cli.update.apply_hint",
    /// 引数: `current`・`upgrade_from`。
    CliUpdateUpgradeFirst => "cli.update.upgrade_first",
    /// 引数: `name`・`size`。
    CliUpdateDownloading => "cli.update.downloading",
    /// 引数: `dir`。
    CliUpdateGapApply => "cli.update.gap_apply",
    /// 引数: `dir`。
    CliUpdateGapRollback => "cli.update.gap_rollback",
    /// 引数: `op`。
    CliUpdateResumedCompleted => "cli.update.resumed_completed",
    /// 引数: `op`。
    CliUpdateResumedReverted => "cli.update.resumed_reverted",
    /// 引数: `files`。
    CliUpdateResumeUntouched => "cli.update.resume_untouched",
    /// 引数: `files`。
    CliUpdateResumeMissing => "cli.update.resume_missing",
    /// 引数: `files`。
    CliUpdateCleaned => "cli.update.cleaned",
    /// 引数: `from`・`to`。
    CliUpdateApplied => "cli.update.applied",
    /// 引数: `files`。
    CliUpdateLeft => "cli.update.left",
    /// 引数: `from`・`to`。
    CliUpdateRolledBack => "cli.update.rolled_back",
    CliUpdateDataUsable => "cli.update.data_usable",
    /// 引数: `format`・`min_write`。
    CliUpdateDataReadOnly => "cli.update.data_read_only",
    /// 引数: `format`・`min_read`。
    CliUpdateDataUnopenable => "cli.update.data_unopenable",
    CliUpdateDataUnknown => "cli.update.data_unknown",
    /// 引数: `path`。
    CliUpdateDataBackup => "cli.update.data_backup",
    CliUpdateDataJournal => "cli.update.data_journal",
    CliUpdateDataNoBackup => "cli.update.data_no_backup",
    /// 引数: `path`。
    CliUpdateConfigBackup => "cli.update.config_backup",
    /// 引数: `channel`。
    CliUpdateNotZip => "cli.update.not_zip",
    /// 引数: `found`・`expected`。
    CliUpdateExeName => "cli.update.exe_name",
    CliUpdateBusy => "cli.update.busy",
    /// 引数: `path`・`pid`。
    CliUpdateDataRootInUse => "cli.update.data_root_in_use",
    /// 引数: `path`。
    CliUpdateDataRootLock => "cli.update.data_root_lock",
    /// 引数: `requested`・`current`。
    CliUpdateNotNewer => "cli.update.not_newer",
    /// 引数: `version`。
    CliUpdateBadVersion => "cli.update.bad_version",
    /// 引数: `missing`・`url`。
    CliUpdateCpu => "cli.update.cpu",
    /// 引数: `arch`・`url`。
    CliUpdateArch => "cli.update.arch",
    /// 引数: `url`。
    CliUpdateNoRoots => "cli.update.no_roots",
    /// 引数: `reason`・`url`。
    CliUpdateNetwork => "cli.update.network",
    /// 引数: `reset`。
    CliUpdateRateLimited => "cli.update.rate_limited",
    CliUpdateRateLimitedUnknown => "cli.update.rate_limited_unknown",
    /// 引数: `version`・`what`。
    CliUpdateMissingFile => "cli.update.missing_file",
    /// 引数: `url`。
    CliUpdateListUnreadable => "cli.update.list_unreadable",
    /// 引数: `reason`。
    CliUpdateTampered => "cli.update.tampered",
    /// 引数: `reason`。
    CliUpdateUnusable => "cli.update.unusable",
    /// 引数: `url`。
    CliUpdateUnknownSchema => "cli.update.unknown_schema",
    /// 引数: `reason`。
    CliUpdateFailed => "cli.update.failed",
    /// 引数: `reason`。
    CliUpdateReverted => "cli.update.reverted",
    /// 引数: `reason`。
    CliUpdateNotRecorded => "cli.update.not_recorded",
    /// 引数: `reason`。
    CliUpdateRevertedNotRecorded => "cli.update.reverted_not_recorded",
    /// 引数: `dir`。
    CliUpdateForeignPlan => "cli.update.foreign_plan",
    CliUpdateNoPrev => "cli.update.no_prev",
    /// 引数: `line`。
    CliUpdatePrevNotEvorag => "cli.update.prev_not_evorag",
    /// 引数: `name`。
    CliUpdateLeftover => "cli.update.leftover",
    /// 引数: `name`。
    CliUpdateUnrelatedLeftover => "cli.update.unrelated_leftover",
    /// 引数: `dir`・`advice`。
    CliUpdateNotWritable => "cli.update.not_writable",
    /// 引数: `path`・`advice`。
    CliUpdateOthersCanWrite => "cli.update.others_can_write",
    CliUpdateNoStateDir => "cli.update.no_state_dir",
    /// 引数: `state`。
    CliUpdateManual => "cli.update.manual",
    CliUpdateStartupRestart => "cli.update.startup_restart",
    /// 引数: `reason`。
    CliUpdateStartupFailed => "cli.update.startup_failed",
    /// 続きが途中で失敗し、動いている evorag がインストール先のものと違う (12 §7.3)。引数: `reason`。
    CliUpdateStartupFailedRestart => "cli.update.startup_failed_restart",
    /// 引数: `files`。
    CliUpdateStartupKept => "cli.update.startup_kept",
    /// SOCKS のプロキシ。引数: `var`・`url`。
    CliUpdateUnsupportedProxySocks => "cli.update.unsupported_proxy_socks",
    /// http・https のほかのスキーム。引数: `var`・`url`。
    CliUpdateUnsupportedProxyScheme => "cli.update.unsupported_proxy_scheme",
    /// URL として読めない。引数: `var`・`url`。
    CliUpdateUnsupportedProxyInvalid => "cli.update.unsupported_proxy_invalid",
    /// 引数: `op`。
    CliUpdateRunAgain => "cli.update.run_again",
    /// 引数: `command`。
    CliUpdateReinstall => "cli.update.reinstall",
    CliUpdateReinstallSteps => "cli.update.reinstall_steps",
    /// 引数: `channel`・`version`。
    CliUpdateReinstallChannel => "cli.update.reinstall_channel",
    /// 引数: `current`・`upgrade_from`。
    CliUpdateUpgradeFirstReinstall => "cli.update.upgrade_first_reinstall",
    /// 引数: `path`・`advice`。
    CliUpdateOwnedByOther => "cli.update.owned_by_other",
    /// 引数: `path`・`advice`。
    CliUpdateUnsafeOwner => "cli.update.unsafe_owner",
    CliUpdateAdviceWindows => "cli.update.advice_windows",
    CliUpdateAdviceUnix => "cli.update.advice_unix",
    /// 引数: `detail`。
    CliUpdateDetail => "cli.update.detail",
    /// 引数: `cause`・`reason`。
    CliUpdateRevertFailed => "cli.update.revert_failed",
    /// 引数: `cause`・`reason`。
    CliUpdateRevertedNotRecordedCause => "cli.update.reverted_not_recorded_cause",
    // 理由の句と操作の名前 (置き換え・検証・通信の失敗を、英語の文言を差し込まずに示す、FR-907)。
    CliUpdateOpOpen => "cli.update.op.open",
    CliUpdateOpCreate => "cli.update.op.create",
    CliUpdateOpLock => "cli.update.op.lock",
    CliUpdateOpRead => "cli.update.op.read",
    CliUpdateOpWrite => "cli.update.op.write",
    CliUpdateOpRename => "cli.update.op.rename",
    CliUpdateOpRemove => "cli.update.op.remove",
    CliUpdateOpSync => "cli.update.op.sync",
    CliUpdateOpOther => "cli.update.op.other",
    /// 引数: `found`・`expected`。
    CliUpdateReasonExeName => "cli.update.reason.exe_name",
    CliUpdateReasonBusy => "cli.update.reason.busy",
    CliUpdateReasonStateUnreadable => "cli.update.reason.state_unreadable",
    /// 引数: `dir`。
    CliUpdateReasonForeignPlan => "cli.update.reason.foreign_plan",
    CliUpdateReasonPlanPending => "cli.update.reason.plan_pending",
    CliUpdateReasonUnknownRole => "cli.update.reason.unknown_role",
    /// 引数: `name`。
    CliUpdateReasonForeignFile => "cli.update.reason.foreign_file",
    CliUpdateReasonNothingToRestore => "cli.update.reason.nothing_to_restore",
    /// 引数: `name`。
    CliUpdateReasonLeftover => "cli.update.reason.leftover",
    /// 引数: `name`。
    CliUpdateReasonUnrelatedLeftover => "cli.update.reason.unrelated_leftover",
    /// 引数: `path`。
    CliUpdateReasonBlockingFile => "cli.update.reason.blocking_file",
    /// 引数: `path`。
    CliUpdateReasonOthersCanWrite => "cli.update.reason.others_can_write",
    /// 引数: `path`。
    CliUpdateReasonOwnedByOther => "cli.update.reason.owned_by_other",
    /// 引数: `dir`。
    CliUpdateReasonNotWritable => "cli.update.reason.not_writable",
    /// 引数: `name`。
    CliUpdateReasonNotRegularFile => "cli.update.reason.not_regular_file",
    /// 引数: `path`。
    CliUpdateReasonUnsafeOwner => "cli.update.reason.unsafe_owner",
    CliUpdateReasonNoPrev => "cli.update.reason.no_prev",
    /// 引数: `line`。
    CliUpdateReasonPrevNotEvorag => "cli.update.reason.prev_not_evorag",
    /// 引数: `reason`。
    CliUpdateReasonPrevProbe => "cli.update.reason.prev_probe",
    /// 引数: `name`。
    CliUpdateReasonChanged => "cli.update.reason.changed",
    CliUpdateReasonSameFile => "cli.update.reason.same_file",
    CliUpdateReasonWrittenDigest => "cli.update.reason.written_digest",
    /// 引数: `name`。
    CliUpdateReasonCopiedDigest => "cli.update.reason.copied_digest",
    /// 引数: `reason`。
    CliUpdateReasonReverted => "cli.update.reason.reverted",
    /// 引数: `reason`。
    CliUpdateReasonNotRecorded => "cli.update.reason.not_recorded",
    /// 引数: `reason`。
    CliUpdateReasonRevertedNotRecorded => "cli.update.reason.reverted_not_recorded",
    /// 引数: `cause`・`reason`。
    CliUpdateReasonRevertFailed => "cli.update.reason.revert_failed",
    /// 引数: `name`・`op`。
    CliUpdateReasonFile => "cli.update.reason.file",
    /// 引数: `path`・`op`。
    CliUpdateReasonIo => "cli.update.reason.io",
    /// 引数: `reason`。
    CliUpdateReasonState => "cli.update.reason.state",
    /// 引数: `line`・`expected`。
    CliUpdateReasonCheckVersion => "cli.update.reason.check_version",
    CliUpdateReasonProbeSpawn => "cli.update.reason.probe_spawn",
    /// 引数: `seconds`。
    CliUpdateReasonProbeTimeout => "cli.update.reason.probe_timeout",
    /// 引数: `status`。
    CliUpdateReasonProbeFailed => "cli.update.reason.probe_failed",
    CliUpdateReasonProbeOutput => "cli.update.reason.probe_output",
    CliUpdateReasonUnknown => "cli.update.reason.unknown",
    CliUpdateVerifySignatureMalformed => "cli.update.verify.signature_malformed",
    CliUpdateVerifyLegacySignature => "cli.update.verify.legacy_signature",
    /// 引数: `key_id`。
    CliUpdateVerifyUnknownKey => "cli.update.verify.unknown_key",
    /// 引数: `key_id`。
    CliUpdateVerifyBadSignature => "cli.update.verify.bad_signature",
    CliUpdateVerifyTrustedComment => "cli.update.verify.trusted_comment",
    CliUpdateVerifyUnknownSchema => "cli.update.verify.unknown_schema",
    CliUpdateVerifyMalformed => "cli.update.verify.malformed",
    CliUpdateVerifyNameMismatch => "cli.update.verify.name_mismatch",
    /// 引数: `found`・`source`・`expected`。
    CliUpdateVerifyVersionMismatch => "cli.update.verify.version_mismatch",
    CliUpdateVerifySourceComment => "cli.update.verify.source_comment",
    CliUpdateVerifySourceTag => "cli.update.verify.source_tag",
    CliUpdateVerifySourceRequested => "cli.update.verify.source_requested",
    /// 引数: `version`・`current`。
    CliUpdateVerifyNotNewer => "cli.update.verify.not_newer",
    /// 引数: `date`・`current`。
    CliUpdateVerifyDateTooOld => "cli.update.verify.date_too_old",
    /// 引数: `upgrade_from`。
    CliUpdateVerifyBelowUpgradeFrom => "cli.update.verify.below_upgrade_from",
    /// 引数: `target`。
    CliUpdateVerifyNoArtifact => "cli.update.verify.no_artifact",
    /// 引数: `target`・`count`。
    CliUpdateVerifyAmbiguousArtifact => "cli.update.verify.ambiguous_artifact",
    /// 引数: `what`・`limit`。
    CliUpdateVerifyTooLarge => "cli.update.verify.too_large",
    CliUpdateVerifyArchive => "cli.update.verify.archive",
    CliUpdateVerifyBuildDate => "cli.update.verify.build_date",
    /// 引数: `actual`・`expected`。
    CliUpdateVerifyArchiveSize => "cli.update.verify.archive_size",
    /// 引数: `size`。
    CliUpdateVerifyArchiveLonger => "cli.update.verify.archive_longer",
    CliUpdateVerifyArchiveDigest => "cli.update.verify.archive_digest",
    CliUpdateVerifyArchiveUnreadable => "cli.update.verify.archive_unreadable",
    CliUpdateVerifyExeMissing => "cli.update.verify.exe_missing",
    CliUpdateVerifyExeNotRegular => "cli.update.verify.exe_not_regular",
    CliUpdateVerifyDuplicateEntries => "cli.update.verify.duplicate_entries",
    /// 引数: `limit`。
    CliUpdateVerifyExeTooLarge => "cli.update.verify.exe_too_large",
    /// 引数: `limit`。
    CliUpdateVerifyUnpackedTooLarge => "cli.update.verify.unpacked_too_large",
    CliUpdateVerifyExeDigest => "cli.update.verify.exe_digest",
    CliUpdateNetRefused => "cli.update.net.refused",
    CliUpdateNetTooManyRedirects => "cli.update.net.too_many_redirects",
    CliUpdateNetBadRedirect => "cli.update.net.bad_redirect",
    CliUpdateNetConnect => "cli.update.net.connect",
    /// 引数: `seconds`。
    CliUpdateNetTimedOut => "cli.update.net.timed_out",
    /// 引数: `limit`。
    CliUpdateNetTooLarge => "cli.update.net.too_large",
    CliUpdateNetNotFound => "cli.update.net.not_found",
    /// 引数: `status`。
    CliUpdateNetStatus => "cli.update.net.status",
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
    /// 引数: `prompt`, `gen`, `issues`。
    DoctorProbeChat => "doctor.probe_chat",
    DoctorProbeNoIssue => "doctor.probe_no_issue",
    DoctorProbeAuxThinkingOff => "doctor.probe_aux_thinking_off",
    /// 引数: `dims`, `ms`。
    DoctorProbeEmbed => "doctor.probe_embed",
    /// 引数: `ms`。
    DoctorProbeRerank => "doctor.probe_rerank",
    /// 引数: `role`, `detail`。
    DoctorProbeFailed => "doctor.probe_failed",
    DoctorProbeNone => "doctor.probe_none",
    /// 引数: `count`, `path`。
    DoctorBroken => "doctor.broken",
    /// 次の起動で compact する (`compact.pending`、03 §8)。
    DoctorCompactPending => "doctor.compact_pending",
    /// 忘れるの指示と合わないターンの数 (04 §7.2)。引数: `count`。
    DoctorForgetMismatched => "doctor.forget_mismatched",
    /// 当て直しの済んでいない忘れるがある (次の起動で当て直す、04 §7.2)。
    DoctorForgetPending => "doctor.forget_pending",
    /// ジャーナルに伏せ字の前の行が残っている。引数: `lines`・`directives`。
    DoctorForgetJournalLines => "doctor.forget_journal_lines",
    /// 忘れるの指示があるのに再び覚えない印の鍵が使えない (鍵に頼る当て直しを止めている、03 §3.1.1)。
    DoctorForgetKeyMissing => "doctor.forget_key_missing",
    /// 引数: `docs`, `segments`。
    DoctorIndexOk => "doctor.index_ok",
    DoctorIndexMissing => "doctor.index_missing",
    /// 引数: `detail`。
    DoctorIndexRebuild => "doctor.index_rebuild",
    /// 引数: `kind`, `dir`, `seq`。
    BackupsItem => "backups.item",
    BackupsNone => "backups.none",
    /// `backups purge` の確認。引数: `count`。
    BackupsPurgeAsk => "backups.purge_ask",
    /// 確かめなかったので消さなかった。
    BackupsPurgeDeclined => "backups.purge_declined",
    /// 引数: `count`。
    BackupsPurged => "backups.purged",
    /// 消せなかった退避がある。引数: `count`・`detail`。
    BackupsPurgeFailed => "backups.purge_failed",
    /// `import-v04` の確認 (03 §12.4、07 §5)。引数: `old`・`renamed`・`root`。
    ImportV04Ask => "import_v04.ask",
    /// `import-v04` の確認に足す、前の取り込みを消してやり直すこと。引数: `new`。
    ImportV04AskRedo => "import_v04.ask_redo",
    /// 照合を終えて止まった入れ替えを続けるかの確認。引数: `new`・`old`・`renamed`・`root`。
    ImportV04AskContinue => "import_v04.ask_continue",
    /// 確認で y 以外を答えた。
    ImportV04Declined => "import_v04.declined",
    /// 取り込みを始めた (書き始める前)。
    ImportV04Started => "import_v04.started",
    /// 取り込んで入れ替えた。引数: `root`・`renamed`・`copied`・`directives`・`excluded`。
    ImportV04Done => "import_v04.done",
    /// 止まった入れ替えを続けて終えた。引数: `root`・`renamed`。
    ImportV04Continued => "import_v04.continued",
    /// 取り込める根でない。引数: `reason`・`next`。
    ImportV04Refused => "import_v04.refused",
    /// 断った理由: データ根でない。引数: `path`。
    ImportV04ReasonNotDataRoot => "import_v04.reason.not_data_root",
    /// 断った理由: v0.4 で書き込みで開いていない。引数: `format`・`writer`。
    ImportV04ReasonNotOpenedByV04 => "import_v04.reason.not_opened_by_v04",
    /// 断った理由: もう新しい形式。引数: `format`。
    ImportV04ReasonNewerFormat => "import_v04.reason.newer_format",
    /// 断った理由: 忘れるの当て直しが残る。
    ImportV04ReasonForgetPending => "import_v04.reason.forget_pending",
    /// 断った理由: ジャーナルに取り込んでいない行・書きかけの末尾・書き直しの跡がある。引数: `lines`。
    ImportV04ReasonJournalPending => "import_v04.reason.journal_pending",
    /// 断った理由: 忘れるの鍵が使えない。
    ImportV04ReasonForgetKey => "import_v04.reason.forget_key",
    /// 断った理由: 前の v0.4 が正しく閉じていない。引数: `detail`。
    ImportV04ReasonUnfinished => "import_v04.reason.unfinished",
    /// 断った理由: 読めない。引数: `detail`。
    ImportV04ReasonUnreadable => "import_v04.reason.unreadable",
    /// 次にすること: 取り込み元のパスを確かめる。
    ImportV04NextCheckPath => "import_v04.next.check_path",
    /// 次にすること: v0.4 で一度起動して閉じる。
    ImportV04NextOpenOnceWithV04 => "import_v04.next.open_once_with_v04",
    /// 次にすること: v0.4 の doctor・repair で確かめる。
    ImportV04NextRepairWithV04 => "import_v04.next.repair_with_v04",
    /// 次にすること: 取り込みは要らない。
    ImportV04NextNothing => "import_v04.next.nothing",
    /// 照合が合わなかった (新しい根は消した)。引数: `detail`。
    ImportV04Mismatch => "import_v04.mismatch",
    /// 残った `.v05-import` を続けることも片付けることもできない。引数: `path`。
    ImportV04Leftover => "import_v04.leftover",
    /// 照合を終えた新しい根の古い根が無い。引数: `new`・`path`。
    ImportV04OldRootMissing => "import_v04.old_root_missing",
    /// 入れ替えの改名ができなかった。引数: `detail`。
    ImportV04RenameFailed => "import_v04.rename_failed",
    /// 入れ替えの改名の隙に、ほかのプロセスが根を開いた・書いた (改名は元に戻した)。引数: `detail`。
    ImportV04RenameGap => "import_v04.rename_gap",
    /// 作りかけの新しい根を消せなかった。引数: `path`・`detail`。
    ImportV04NotRemoved => "import_v04.not_removed",
    /// 断った理由: 古い根のパスがリンク。引数: `path`。
    ImportV04ReasonLink => "import_v04.reason.link",
    /// 断った理由: 古い根がマウントポイント。引数: `path`。
    ImportV04ReasonMountPoint => "import_v04.reason.mount_point",
    /// 断った理由: パスが Unicode として読めない。引数: `path`。
    ImportV04ReasonNonUnicode => "import_v04.reason.non_unicode",
    /// 次にすること: リンクの先の実際のフォルダを指す。
    ImportV04NextRealFolder => "import_v04.next.real_folder",
    /// 次にすること: マウントポイントの下のフォルダへ移す。
    ImportV04NextMoveIntoFolder => "import_v04.next.move_into_folder",
    /// 改名を元に戻せなかった (根の残った場所を示す)。引数: `left`・`detail`。
    ImportV04RenameNotUndone => "import_v04.rename_not_undone",
    /// 元の名前の根が無いので、`.v05-import` に触れずに止めた。引数: `root`・`new`。
    ImportV04OriginalMissing => "import_v04.original_missing",
    /// 元の名前の根と改名の後の古い根の両方がある。引数: `root`・`renamed`。
    ImportV04BothRoots => "import_v04.both_roots",
    /// 元の名前の根が、取り込み元とは別の根。引数: `root`・`new`。
    ImportV04OtherRoot => "import_v04.other_root",
    /// 元の名前の根を読めない。引数: `root`。
    ImportV04OriginalUnreadable => "import_v04.original_unreadable",
    /// `.v05-import` が壊れている (DB を読めない・ジャーナルが無い)。引数: `path`。
    ImportV04DamagedLeftover => "import_v04.damaged_leftover",
    /// 照合の後に新しい根へ書いたのに、古い根も変わっている。引数: `new`。
    ImportV04NewRootUsed => "import_v04.new_root_used",
    /// `.v05-import` の取り込み元の記録が、この根のものでない。引数: `new`。
    ImportV04ForeignSource => "import_v04.foreign_source",
    /// データ根が無く、入れ替えが途中 (根を作らずに断った)。引数: `new`。
    CliImportPending => "cli.import_pending",
    /// v0.4 の根なので、在り場所では開かずに断った (何も書いていない、03 §12.4)。引数: `path`・`format`。
    CliNeedsImport => "cli.needs_import",
    /// 前回正しく閉じていない v0.4 の根か、DB が無い・読めない v0.4 の根なので、修復も作り直しもせずに断った (何も
    /// 書いていない、03 §8)。引数: `path`・`format`。
    CliNeedsV04Open => "cli.needs_v04_open",
    /// 形式の版が分からないときの `format` の引数。
    CliFormatUnknown => "cli.format_unknown",
    /// v0.5 の根へ、v0.4 以前の形式の退避を戻そうとした (何も変えていない)。引数: `backup`・`format`。
    CliRestoreBackupV04 => "cli.restore_backup_v04",
    /// `rebuild --facts` は v0.5 では受け付けない (03 §10、OQ-55 の 5)。
    CliRebuildFactsV05 => "cli.rebuild_facts_v05",
    /// TUI の起動で v0.4 の根を開こうとした (取り込むかを尋ねる前の説明、03 §12.4)。引数: `path`・`format`。
    ImportV04AtStartup => "import_v04.at_startup",
    /// TUI の起動の取り込みの確認で y 以外を答えた (何も変えずに 2 で終わる)。
    ImportV04DeclinedAtStartup => "import_v04.declined_at_startup",
    /// `doctor`: v0.4 の根 (v0.5 では取り込みが要る)。引数: `format`。
    DoctorNeedsImport => "doctor.needs_import",
    /// `doctor`: 前回正しく閉じていない (か DB が無い・読めない) v0.4 の根。引数: `format`。
    DoctorNeedsV04Open => "doctor.needs_v04_open",
    /// `backups list` の、ID の分からない退避の行。引数: `kind`, `dir`, `seq`。
    BackupsItemUnknownRoot => "backups.item_unknown_root",
    /// 起動の前に、止まった入れ替えを続けて終えた。引数: `root`・`renamed`。
    ImportV04ResumedAtStartup => "import_v04.resumed_at_startup",
    /// 起動の前に、作りかけの新しい根を消した。引数: `new`。
    ImportV04RemovedUnfinishedAtStartup => "import_v04.removed_unfinished_at_startup",
    /// 起動の前に、古い根が変わっていたので、取り込んだ新しい根を消して古い根を元の名前に戻した。引数: `root`。
    ImportV04RemovedStaleAtStartup => "import_v04.removed_stale_at_startup",
    /// `reset` の確認: 今のデータ根を消す (03 §10.2)。引数: `path`・`backups` (中の退避の数)。
    ResetAskRoot => "reset.ask_root",
    /// `reset --old` の確認: 古い根を消す。引数: `path`・`backups`。
    ResetAskOldRoot => "reset.ask_old_root",
    /// `reset --old` の確認: 古い根がもう無い (外の退避を消して処分を記録するだけ)。引数: `path`。
    ResetAskOldMissing => "reset.ask_old_missing",
    /// `reset` の確認: 前の reset の消しかけのフォルダの続きも消す。引数: `path`。
    ResetAskLeftover => "reset.ask_leftover",
    /// `reset` の確認: `storage.backup_dir` の中の、対象の根の退避も消す。引数: `dir`・`count`。
    ResetAskBackups => "reset.ask_backups",
    /// `reset --old` の確認: 取り込みの後に忘れた値が古い根に残っていること。引数: `forgets`・`excluded`。
    ResetAskOldForgets => "reset.ask_old_forgets",
    /// `reset` の確認: 取り込みの前の古い根が残っている (一緒に消すことを勧める)。引数: `path`。
    ResetAskKeptOld => "reset.ask_kept_old",
    /// `reset` の確認の結び (元に戻せない・OS のバックアップと書き出しには残りうる・設定は残す)。
    ResetAskConfirm => "reset.ask_confirm",
    /// 確認で y 以外を答えた。
    ResetDeclined => "reset.declined",
    /// 今のデータ根を消した。引数: `path`・`backups` (消した外の退避の数)。
    ResetDone => "reset.done",
    /// 古い根を消して処分を記録した。引数: `path`・`backups`。
    ResetDoneOld => "reset.done_old",
    /// 消せないものが残った。引数: `paths`・`command`。
    ResetLeft => "reset.left",
    /// データ根でない。引数: `path`。
    ResetNotDataRoot => "reset.not_data_root",
    /// 今の根が import-v04 で作った根でない。引数: `root`。
    ResetNotImported => "reset.not_imported",
    /// 古い根はもう処分してある。
    ResetAlreadyDisposed => "reset.already_disposed",
    /// 古い根の ID が import.source と違う。引数: `path`。
    ResetIdMismatch => "reset.id_mismatch",
    /// 対象が今の根・設定・状態のフォルダと重なる。引数: `path`・`other`。
    ResetOverlaps => "reset.overlaps",
    /// 対象がリンク。引数: `path`。
    ResetLink => "reset.link",
    /// 対象がマウントポイント。引数: `path`。
    ResetMountPoint => "reset.mount_point",
    /// 古い根が取り込みの後に使われた (手順の (0))。引数: `path`・`lines`。
    ResetOldChanged => "reset.old_changed",
    /// 消しかけに見えないフォルダがある。引数: `path`。
    ResetUnknownLeftover => "reset.unknown_leftover",
    /// 改名の隙にほかのプロセスが開いた (2)。引数: `detail`。
    ResetRenameGap => "reset.rename_gap",
    /// 改名できなかった。引数: `detail`。
    ResetRenameFailed => "reset.rename_failed",
    /// 改名を元に戻せなかった。引数: `left`・`detail`。
    ResetRenameNotUndone => "reset.rename_not_undone",
    /// 古い根を消したが、処分を記録できなかった。引数: `detail`。
    ResetMarkFailed => "reset.mark_failed",
    /// `doctor`: reset の消しかけのフォルダが残っている。引数: `path`。
    ResetLeftoverFound => "reset.leftover_found",
    /// 終了のために中断した。
    InterruptedByShutdown => "response.interrupted_by_shutdown",
    /// 並べた語の区切り (`ja` は読点)。セッションの題名・点検の理由・書き出した記録の種類を並べる。
    UiSepComma => "ui.sep.comma",
    /// 並べた短い項目の区切り (`ja` は中黒)。事実の変化の件数・忘れるの選択を並べる。
    UiSepDot => "ui.sep.dot",
    /// 並べた文の区切り (`ja` は句点)。
    UiSepSentence => "ui.sep.sentence",
    /// 事実の行の主語と属性 (確認の画面・ほかの行の表示)。引数: `subject`、`attr`。
    UiFactsLabel => "ui.facts.label",
    /// `doctor` のモデルの点検の問題 (`ui.sep.sentence` で並べた理由)。引数: `issues`。
    DoctorProbeIssues => "doctor.probe_issues",
    /// API キーの環境変数を設定していない (`cli.llm_unauthorized` の `env`)。
    CliEnvUnset => "cli.env_unset",
    /// `evorag setup` が書く設定ファイルの説明 (TOML のコメント、`# ` で始める)。
    CliSetupConfigHeader => "cli.setup.config_header",
    CliSetupConfigPromptLanguage => "cli.setup.config_prompt_language",
    CliSetupConfigRouterModel => "cli.setup.config_router_model",
    CliSetupConfigApiKeyEnv => "cli.setup.config_api_key_env",
    /// `--help` の文 (07 §5)。clap の命令に、UI の言語のカタログから当てる。
    CliHelpAbout => "cli.help.about",
    CliHelpDataRoot => "cli.help.data_root",
    CliHelpConfig => "cli.help.config",
    CliHelpSetup => "cli.help.setup",
    CliHelpSetupUrl => "cli.help.setup_url",
    CliHelpSetupApiKeyEnv => "cli.help.setup_api_key_env",
    CliHelpSetupYes => "cli.help.setup_yes",
    CliHelpDoctor => "cli.help.doctor",
    CliHelpBackups => "cli.help.backups",
    CliHelpBackupsList => "cli.help.backups_list",
    CliHelpBackupsPurge => "cli.help.backups_purge",
    CliHelpBackupsPurgeYes => "cli.help.backups_purge_yes",
    CliHelpExport => "cli.help.export",
    CliHelpExportOut => "cli.help.export_out",
    CliHelpRebuild => "cli.help.rebuild",
    CliHelpRebuildIndex => "cli.help.rebuild_index",
    CliHelpRebuildFacts => "cli.help.rebuild_facts",
    CliHelpRestore => "cli.help.restore",
    CliHelpRestoreBackup => "cli.help.restore_backup",
    CliHelpRepair => "cli.help.repair",
    CliHelpUpdate => "cli.help.update",
    CliHelpUpdateCheck => "cli.help.update_check",
    CliHelpUpdateApply => "cli.help.update_apply",
    CliHelpUpdateApplyVersion => "cli.help.update_apply_version",
    CliHelpUpdateRollback => "cli.help.update_rollback",
    CliHelpImportV04 => "cli.help.import_v04",
    CliHelpImportV04From => "cli.help.import_v04_from",
    CliHelpReset => "cli.help.reset",
    CliHelpResetOld => "cli.help.reset_old",
    CliHelpResetYes => "cli.help.reset_yes",
}

/// 漢字・仮名・全角の文字を含むか。`ja` 以外の文言カタログの検査 (`cargo xtask i18n`) と、英語の UI の試験で
/// 使う (英語の画面に出てはいけない文字、AC-FR-908-1)。
pub fn has_cjk(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(c,
            '\u{3000}'..='\u{30ff}'     // 全角の記号・ひらがな・カタカナ
            | '\u{31f0}'..='\u{31ff}'   // カタカナの小書き
            | '\u{3200}'..='\u{33ff}'   // 囲み文字・CJK の互換の記号
            | '\u{3400}'..='\u{4dbf}'   // 漢字 (拡張 A)
            | '\u{4e00}'..='\u{9fff}'   // 漢字
            | '\u{f900}'..='\u{faff}'   // 互換漢字
            | '\u{fe30}'..='\u{fe4f}'   // CJK の互換の形
            | '\u{ff00}'..='\u{ffef}'   // 全角の英数・半角カナ
            | '\u{20000}'..='\u{2fa1f}' // 漢字 (拡張 B 以降と互換の補助)
        )
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_covers_kana_kanji_and_full_width_forms() {
        for text in ["あ", "ア", "ㇰ", "㈱", "㎡", "漢", "︰", "Ａ", "、", "𠀋"] {
            assert!(has_cjk(text), "{text}");
        }
        for text in ["", "ASCII", "café · → … ▶ ↑↓"] {
            assert!(!has_cjk(text), "{text}");
        }
    }
}
