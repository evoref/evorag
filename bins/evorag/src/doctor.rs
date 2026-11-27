//! `evorag doctor` と `evorag backups list` (03 §10、FR-207、FR-213)。何も書かない (AC-FR-207-1)。

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use evorag_llm::{Endpoint, HttpClient, LlmClient, RetryPolicy};
use evorag_memory::{ModelBindingBody, ModelProbeBody};
use evorag_store::OpenError;
use evorag_store::inspect::{DbState, inspect_collect};
use evorag_store::restore::BackupKind;
use evorag_tui::catalog::Catalog;
use evorag_types::MessageKey;

fn say(c: &Catalog, key: MessageKey, args: &[(&str, &str)]) {
    println!("{}", c.with(key, args));
}

pub fn run(
    c: &Catalog,
    cfg: &crate::config::Config,
    config_file: &Path,
    data_root: &Path,
    rt: &tokio::runtime::Runtime,
    llm: Arc<HttpClient>,
) -> ExitCode {
    let url = &cfg.chat_url();
    let backup_dir = cfg.storage.backup_dir.as_deref();
    say(
        c,
        MessageKey::DoctorConfig,
        &[("path", &config_file.display().to_string())],
    );
    let llm_state = rt.block_on(async {
        llm.health().await?;
        llm.server_info().await
    });
    match &llm_state {
        Ok(info) => say(
            c,
            MessageKey::DoctorLlmOk,
            &[
                ("url", url),
                ("model", &info.model_name),
                ("n_ctx", &info.n_ctx.to_string()),
                ("build", &info.build_info),
            ],
        ),
        Err(e) => say(
            c,
            MessageKey::DoctorLlmNg,
            &[("url", url), ("detail", &e.to_string())],
        ),
    }
    say(
        c,
        MessageKey::DoctorRoot,
        &[("path", &data_root.display().to_string())],
    );
    for line in crate::reset::leftover_lines(c, data_root) {
        println!("{line}");
    }
    // 種類は根の形式で選ぶ (v0.4 の根は、読むだけで点検し、取り込みを案内する、03 §12.4)。
    // 見分けは、ストアが書き込みで開く前と同じ (修復の要る DB は、データ根の外の写しを直して読む。何も書かない)。
    let v04 = evorag_store::root_probe::probe_root(data_root)
        .ok()
        .and_then(|p| p.v04());
    let mut kinds = if v04.is_some() {
        evorag_memory::kinds()
    } else {
        evorag_memory::kinds_v05()
    };
    kinds.sort_by_key(|k| k.name);
    let report = match inspect_collect(
        data_root,
        kinds,
        backup_dir,
        &[
            evorag_memory::KIND_MODEL_PROBE,
            evorag_memory::KIND_MODEL_BINDING,
        ],
    ) {
        Ok(r) => r,
        Err(OpenError::Lock(evorag_store::lock::LockError::InUse { pid })) => {
            return crate::data_root_in_use(c, data_root, pid);
        }
        Err(e @ OpenError::Lock(_)) => {
            eprintln!(
                "{}",
                c.with(
                    MessageKey::CliDataRootUnusable,
                    &[("detail", &e.to_string())]
                )
            );
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!(
                "{}",
                c.with(MessageKey::CliError, &[("detail", &e.to_string())])
            );
            return ExitCode::from(1);
        }
    };
    match &report.db {
        // v0.4 の根は、この版では修復も作り直しもしない (v0.4 で一度開いて閉じてから取り込む、03 §8)。
        DbState::Missing | DbState::NeedsRepair(_) | DbState::Unreadable(_) if v04.is_some() => {
            let unknown = c.key(MessageKey::CliFormatUnknown);
            let format = v04
                .as_ref()
                .and_then(|v| v.format.clone())
                .unwrap_or(unknown);
            say(c, MessageKey::DoctorNeedsV04Open, &[("format", &format)]);
        }
        DbState::Missing => say(c, MessageKey::DoctorDbMissing, &[]),
        DbState::NeedsRepair(d) => say(c, MessageKey::DoctorDbRepair, &[("detail", d)]),
        DbState::Unreadable(d) => say(c, MessageKey::DoctorDbBad, &[("detail", d)]),
        DbState::Opened => {
            let get = |k: &str| report.meta.get(k).cloned().unwrap_or_default();
            say(
                c,
                MessageKey::DoctorDbOk,
                &[
                    ("format", &get("format.version")),
                    ("writer", &get("last_writer")),
                ],
            );
            if evorag_store::root_probe::older_than_v05(&get("format.version")) {
                say(
                    c,
                    MessageKey::DoctorNeedsImport,
                    &[("format", &get("format.version"))],
                );
            }
            let (mut unreadable, mut corrupt, mut newer) = (0, 0, 0);
            for (kind, n) in &report.census.known {
                say(
                    c,
                    MessageKey::DoctorRecords,
                    &[("kind", kind), ("count", &n.ok.to_string())],
                );
                unreadable += n.unreadable;
                corrupt += n.corrupt;
                newer += n.newer_v;
            }
            if unreadable + corrupt + newer > 0 {
                say(
                    c,
                    MessageKey::DoctorProblemRecords,
                    &[
                        ("unreadable", &unreadable.to_string()),
                        ("corrupt", &corrupt.to_string()),
                        ("newer", &newer.to_string()),
                    ],
                );
            }
            let unknown: u64 = report.census.unknown.values().sum();
            if unknown > 0 {
                say(
                    c,
                    MessageKey::DoctorUnknownKinds,
                    &[("count", &unknown.to_string())],
                );
            }
            say(
                c,
                MessageKey::DoctorQuarantine,
                &[("count", &report.quarantined().to_string())],
            );
            // 空きが足りずに見送った compact も、次の起動でやり直すまで知らせる (03 §8、04 §7.8)。
            if report.compact_pending() {
                say(c, MessageKey::DoctorCompactPending, &[]);
            }
            // 忘れるの指示と合わないターンと、当て直しの残り (04 §7.2)。
            let lang = report
                .meta
                .get(evorag_store::meta::PROMPT_LANGUAGE)
                .cloned();
            let key_ok = report
                .meta
                .get(evorag_store::meta::FORGET_KEY)
                .is_some_and(|k| evorag_memory::ForgetKey::from_hex(k).is_some());
            if let Ok(Some(Some(f))) = evorag_store::inspect::with_view(data_root, |v| {
                evorag_core::export_checks::forget_check(v, lang.as_deref())
            }) {
                if f.mismatched > 0 {
                    say(
                        c,
                        MessageKey::DoctorForgetMismatched,
                        &[("count", &f.mismatched.to_string())],
                    );
                }
                if f.pending {
                    say(c, MessageKey::DoctorForgetPending, &[]);
                }
                if f.directives > 0 && !key_ok {
                    say(c, MessageKey::DoctorForgetKeyMissing, &[]);
                }
                // ジャーナルに残っている伏せ字の前の行 (書き直しの済んでいない忘れる、04 §7.2)。
                if f.directives > 0
                    && let Ok(revs) = evorag_store::inspect::journal_revs(
                        data_root,
                        evorag_memory::KIND_TURN,
                        evorag_memory::TURN_REV_FIELD,
                    )
                    && let Ok(Some(Some(j))) = evorag_store::inspect::with_view(data_root, |v| {
                        evorag_core::export_checks::journal_leftovers(v, &revs)
                    })
                    && j.lines > 0
                {
                    say(
                        c,
                        MessageKey::DoctorForgetJournalLines,
                        &[
                            ("lines", &j.lines.to_string()),
                            ("directives", &j.directives.to_string()),
                        ],
                    );
                }
            }
        }
    }
    let j = &report.journal;
    say(
        c,
        MessageKey::DoctorJournal,
        &[
            ("files", &j.files.to_string()),
            ("lines", &j.lines.to_string()),
            ("bad", &j.bad.to_string()),
            ("newer", &j.newer.to_string()),
            ("partial", &j.partial.to_string()),
            ("pending", &j.pending.to_string()),
        ],
    );
    match evorag_core::inspect_index(data_root) {
        evorag_core::IndexState::Missing => say(c, MessageKey::DoctorIndexMissing, &[]),
        evorag_core::IndexState::Ok { segments, docs } => say(
            c,
            MessageKey::DoctorIndexOk,
            &[
                ("docs", &docs.to_string()),
                ("segments", &segments.to_string()),
            ],
        ),
        evorag_core::IndexState::NeedsRebuild(d) => {
            say(c, MessageKey::DoctorIndexRebuild, &[("detail", &d)]);
        }
    }
    show_probes(c, &report);
    show_memory(c, cfg, rt, &report, llm_state.as_ref().ok());
    match report.backups.first() {
        Some(b) => say(
            c,
            MessageKey::DoctorBackups,
            &[
                ("count", &report.backups.len().to_string()),
                ("latest", &b.dir.display().to_string()),
            ],
        ),
        None => say(c, MessageKey::DoctorBackupsNone, &[]),
    }
    if let Some(first) = report.broken.first() {
        say(
            c,
            MessageKey::DoctorBroken,
            &[
                ("count", &report.broken.len().to_string()),
                ("path", &first.display().to_string()),
            ],
        );
    }
    ExitCode::SUCCESS
}

/// モデルの点検の結果 (06 §3.3)。役割ごとに、いちばん新しいもの。
fn show_probes(c: &Catalog, report: &evorag_store::inspect::Inspection) {
    let newest = newest_probes(report);
    if newest.is_empty() {
        say(c, MessageKey::DoctorProbeNone, &[]);
    }
    for (role, p) in &newest {
        for line in probe_lines(c, role, p) {
            println!("{line}");
        }
    }
}

/// 役割ごとに、いちばん新しい点検の結果。
fn newest_probes(
    report: &evorag_store::inspect::Inspection,
) -> std::collections::BTreeMap<String, ModelProbeBody> {
    let mut newest: std::collections::BTreeMap<String, (evorag_types::Timestamp, ModelProbeBody)> =
        Default::default();
    for e in report
        .collected
        .get(evorag_memory::KIND_MODEL_PROBE)
        .into_iter()
        .flatten()
    {
        if let Ok(b) = e.body_as::<ModelProbeBody>()
            && newest
                .get(&b.role)
                .is_none_or(|(at, _)| *at <= e.updated_at)
        {
            newest.insert(b.role.clone(), (e.updated_at, b));
        }
    }
    newest.into_iter().map(|(k, (_, b))| (k, b)).collect()
}

/// 役割・model_key・`build_info` の、いちばん新しい点検の結果。
fn probe_for(
    report: &evorag_store::inspect::Inspection,
    role: &str,
    model_key: &str,
    build_info: &str,
) -> Option<ModelProbeBody> {
    report
        .collected
        .get(evorag_memory::KIND_MODEL_PROBE)
        .into_iter()
        .flatten()
        .filter_map(|e| Some((e.updated_at, e.body_as::<ModelProbeBody>().ok()?)))
        .filter(|(_, b)| b.role == role && b.model_key == model_key && b.build_info == build_info)
        .max_by_key(|(at, _)| *at)
        .map(|(_, b)| b)
}

/// 点検の結果が、今の要求の付け足し `extra` で点検したもので、型縛りの補助の呼び出し (区切りの呼び出し) に
/// 使えないか (06 §3.3。memory_role.rs と同じ判定。点検し直す結果は、まだ分からないとして数えない)。
fn probe_unusable(p: &ModelProbeBody, extra: &evorag_llm::RequestExtra) -> bool {
    evorag_core::probe::is_current(p, extra)
        && !(p.structured_ok == Some(true) && p.thinking_off != Some(false))
}

/// memory の役割と縮退 (06 §2.1)。設定した memory のサーバに繋いで、記録と点検の結果と比べる。chat の点検の結果は、
/// 今繋がっている chat のモデル (`chat`) のものだけを見る。
fn show_memory(
    c: &Catalog,
    cfg: &crate::config::Config,
    rt: &tokio::runtime::Runtime,
    report: &evorag_store::inspect::Inspection,
    chat: Option<&evorag_llm::ServerInfo>,
) {
    let chat_extra = cfg.chat_aux_request_extra();
    let chat_unusable = chat
        .and_then(|s| {
            probe_for(
                report,
                evorag_memory::ROLE_CHAT,
                &s.model_key,
                &s.build_info,
            )
        })
        .is_some_and(|p| probe_unusable(&p, &chat_extra));
    let Some(r) = cfg.memory_role() else {
        say(c, MessageKey::DoctorMemoryDefault, &[]);
        if chat_unusable {
            say(c, MessageKey::DoctorMemoryStopped, &[]);
        }
        return;
    };
    let recorded = report
        .collected
        .get(evorag_memory::KIND_MODEL_BINDING)
        .into_iter()
        .flatten()
        .filter_map(|e| e.body_as::<ModelBindingBody>().ok())
        .find(|b| b.role == evorag_memory::ROLE_MEMORY)
        .map(|b| b.current);
    let connected = Endpoint::new(&r.url, r.api_key.clone(), cfg.llm.allow_remote)
        .and_then(|e| HttpClient::new(e, RetryPolicy { delays: Vec::new() }))
        .map(|client| client.with_model(r.model.clone()));
    let info = match connected {
        Ok(client) => rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                client.health().await?;
                client.server_info().await
            })
            .await
            .unwrap_or_else(|_| Err(evorag_llm::LlmError::Connect("timed out".into())))
        }),
        Err(e) => Err(e),
    };
    let fallback = match info {
        Err(e) => {
            say(
                c,
                MessageKey::DoctorMemoryUnreachable,
                &[("url", &r.url), ("detail", &e.to_string())],
            );
            true
        }
        Ok(info) => {
            let now = evorag_core::origin(&info);
            match &recorded {
                None => {
                    say(
                        c,
                        MessageKey::DoctorMemoryNotRecorded,
                        &[("url", &r.url), ("file", &now.file_name)],
                    );
                    false
                }
                Some(rec) if rec.model_key != now.model_key => {
                    say(
                        c,
                        MessageKey::DoctorMemoryMismatch,
                        &[("recorded", &rec.file_name), ("connected", &now.file_name)],
                    );
                    true
                }
                Some(_) => {
                    let failed = probe_for(
                        report,
                        evorag_memory::ROLE_MEMORY,
                        &now.model_key,
                        &now.build_info,
                    )
                    .is_some_and(|p| probe_unusable(&p, &cfg.memory_request_extra()));
                    if failed {
                        say(c, MessageKey::DoctorMemoryProbeFailed, &[]);
                    } else {
                        say(
                            c,
                            MessageKey::DoctorMemoryOk,
                            &[("url", &r.url), ("file", &now.file_name)],
                        );
                    }
                    failed
                }
            }
        }
    };
    if fallback && chat_unusable {
        say(c, MessageKey::DoctorMemoryStopped, &[]);
    }
}

/// 点検の結果を、利用者に見せる行にする (`doctor` と `setup` で使う)。
pub fn probe_lines(c: &Catalog, role: &str, p: &ModelProbeBody) -> Vec<String> {
    let num = |v: Option<String>| v.unwrap_or_else(|| "-".to_owned());
    let mut out = Vec::new();
    if let Some(e) = &p.error {
        out.push(c.with(
            MessageKey::DoctorProbeFailed,
            &[("role", role), ("detail", e)],
        ));
    }
    match role {
        evorag_memory::ROLE_CHAT | evorag_memory::ROLE_MEMORY => {
            let memory = role == evorag_memory::ROLE_MEMORY;
            let mut issues = Vec::new();
            if p.thinking_off == Some(false) {
                issues.push(c.key(if memory {
                    MessageKey::UiProbeMemoryThinkingNotOff
                } else {
                    MessageKey::UiProbeThinkingNotOff
                }));
            }
            if p.structured_ok == Some(false) {
                issues.push(c.key(MessageKey::UiProbeStructuredBroken));
            }
            let issues = if issues.is_empty() {
                c.key(MessageKey::DoctorProbeNoIssue)
            } else {
                c.with(
                    MessageKey::DoctorProbeIssues,
                    &[("issues", &issues.join(&c.key(MessageKey::UiSepSentence)))],
                )
            };
            out.push(c.with(
                if memory {
                    MessageKey::DoctorProbeMemory
                } else {
                    MessageKey::DoctorProbeChat
                },
                &[
                    ("prompt", &num(p.prompt_tps.map(|v| v.to_string()))),
                    ("gen", &num(p.gen_tps.map(|v| v.to_string()))),
                    ("issues", &issues),
                ],
            ));
            // 点検が思考を切る指定を自動で足した (06 §3.3、OQ-52)。
            if p.aux_thinking_off == Some(true) {
                out.push(c.key(if memory {
                    MessageKey::DoctorProbeMemoryAuxThinkingOff
                } else {
                    MessageKey::DoctorProbeAuxThinkingOff
                }));
            }
            // 要求の指定を足しても思考を切れないモデルには、llama-server の `--reasoning-budget 0` を案内する
            // (06 §2.1・§3.3、S-17 の 5)。
            if memory && p.thinking_off == Some(false) && p.aux_thinking_off != Some(true) {
                out.push(c.key(MessageKey::DoctorProbeReasoningBudget));
            }
        }
        evorag_memory::ROLE_EMBED if p.error.is_none() => out.push(c.with(
            MessageKey::DoctorProbeEmbed,
            &[
                ("dims", &num(p.dims.map(|v| v.to_string()))),
                ("ms", &num(p.embed_ms.map(|v| v.to_string()))),
            ],
        )),
        evorag_memory::ROLE_RERANK if p.error.is_none() => out.push(c.with(
            MessageKey::DoctorProbeRerank,
            &[("ms", &num(p.rerank6_ms.map(|v| v.to_string())))],
        )),
        _ => {}
    }
    out
}

pub fn list_backups(c: &Catalog, data_root: &Path, backup_dir: Option<&Path>) -> ExitCode {
    // この根の退避と、ID の分からない退避 (`storage.backup_dir` を共有するほかの根の退避は出さない、03 §7.1)。
    let owner = evorag_store::restore::BackupOwner::of(data_root, None);
    let list = evorag_store::restore::restore_candidates(data_root, backup_dir, &owner);
    if list.is_empty() {
        say(c, MessageKey::BackupsNone, &[]);
    }
    for b in list {
        let kind = match b.kind {
            BackupKind::Daily => "daily",
            BackupKind::Weekly => "weekly",
            BackupKind::PreOp => "pre-op",
        };
        let seq = b
            .journal_seq
            .map_or_else(|| "-".to_owned(), |s| s.to_string());
        let key = if b.data_root_id.is_some() {
            MessageKey::BackupsItem
        } else {
            MessageKey::BackupsItemUnknownRoot
        };
        say(
            c,
            key,
            &[
                ("kind", kind),
                ("dir", &b.dir.display().to_string()),
                ("seq", &seq),
            ],
        );
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// memory のモデルの点検の行 (06 §3.3): 思考を切れないモデルには `--reasoning-budget 0` を案内し、点検が思考を
    /// 切る指定を足したときは、`llm.memory.request_extra` に書く形を示す。chat の行には案内を出さない。
    #[test]
    fn memory_probe_lines_guide_to_the_reasoning_budget() {
        for lang in ["ja", "en"] {
            let c = Catalog::load(lang);
            let never_off = ModelProbeBody {
                role: evorag_memory::ROLE_MEMORY.into(),
                thinking_off: Some(false),
                structured_ok: Some(false),
                aux_thinking_off: Some(false),
                ..ModelProbeBody::default()
            };
            let lines = probe_lines(&c, evorag_memory::ROLE_MEMORY, &never_off).join("\n");
            assert!(lines.contains("--reasoning-budget 0"), "{lines}");
            assert!(lines.contains("llm.memory.request_extra"), "{lines}");
            let chat = probe_lines(&c, evorag_memory::ROLE_CHAT, &never_off).join("\n");
            assert!(!chat.contains("--reasoning-budget"), "{chat}");

            let added = ModelProbeBody {
                thinking_off: Some(true),
                structured_ok: Some(true),
                aux_thinking_off: Some(true),
                ..never_off
            };
            let lines = probe_lines(&c, evorag_memory::ROLE_MEMORY, &added).join("\n");
            assert!(!lines.contains("--reasoning-budget"), "{lines}");
            assert!(lines.contains("[llm.memory.request_extra]"), "{lines}");
        }
    }
}
