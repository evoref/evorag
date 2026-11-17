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
    /// 引数: `docs`, `segments`。
    DoctorIndexOk => "doctor.index_ok",
    DoctorIndexMissing => "doctor.index_missing",
    /// 引数: `detail`。
    DoctorIndexRebuild => "doctor.index_rebuild",
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
