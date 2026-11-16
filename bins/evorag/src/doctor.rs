//! `evorag doctor` と `evorag backups list` (03 §10、FR-207、FR-213)。何も書かない (AC-FR-207-1)。

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use evorag_llm::{HttpClient, LlmClient};
use evorag_memory::ModelProbeBody;
use evorag_store::OpenError;
use evorag_store::inspect::{DbState, inspect_collect};
use evorag_store::restore::{BackupKind, list_backups as backups_of};
use evorag_tui::catalog::Catalog;
use evorag_types::MessageKey;

fn say(c: &Catalog, key: MessageKey, args: &[(&str, &str)]) {
    println!("{}", c.with(key, args));
}

pub fn run(
    c: &Catalog,
    config_file: &Path,
    data_root: &Path,
    backup_dir: Option<&Path>,
    url: &str,
    rt: &tokio::runtime::Runtime,
    llm: Arc<HttpClient>,
) -> ExitCode {
    say(
        c,
        MessageKey::DoctorConfig,
        &[("path", &config_file.display().to_string())],
    );
    let llm_state = rt.block_on(async {
        llm.health().await?;
        llm.server_info().await
    });
    match llm_state {
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
    let mut kinds = evorag_memory::kinds();
    kinds.sort_by_key(|k| k.name);
    let report = match inspect_collect(
        data_root,
        kinds,
        backup_dir,
        &[evorag_memory::KIND_MODEL_PROBE],
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
    if newest.is_empty() {
        say(c, MessageKey::DoctorProbeNone, &[]);
    }
    for (role, (_, p)) in &newest {
        for line in probe_lines(c, role, p) {
            println!("{line}");
        }
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
        evorag_memory::ROLE_CHAT => {
            let mut issues = Vec::new();
            if p.thinking_off == Some(false) {
                issues.push(c.key(MessageKey::UiProbeThinkingNotOff));
            }
            if p.structured_ok == Some(false) {
                issues.push(c.key(MessageKey::UiProbeStructuredBroken));
            }
            let issues = if issues.is_empty() {
                c.key(MessageKey::DoctorProbeNoIssue)
            } else {
                issues.join("。") + "。"
            };
            out.push(c.with(
                MessageKey::DoctorProbeChat,
                &[
                    ("prompt", &num(p.prompt_tps.map(|v| v.to_string()))),
                    ("gen", &num(p.gen_tps.map(|v| v.to_string()))),
                    ("issues", &issues),
                ],
            ));
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
    let list = backups_of(data_root, backup_dir);
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
        say(
            c,
            MessageKey::BackupsItem,
            &[
                ("kind", kind),
                ("dir", &b.dir.display().to_string()),
                ("seq", &seq),
            ],
        );
    }
    ExitCode::SUCCESS
}
