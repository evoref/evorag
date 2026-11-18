//! `config.toml` (07 §4)。
//!
//! - 先頭に `version = 1`。
//! - 読めないキー・型の誤り・範囲外の値は、行番号つきのエラーで起動しない (FR-904)。
//! - 後の版で足すキーは、警告を出して無視する (古い版に戻しても起動できるように)。

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// 設定の形式の版 (03 §12.3)。
pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub version: Option<u32>,
    pub ui: Ui,
    pub prompt: Prompt,
    pub llm: Llm,
    pub storage: Storage,
    pub context: Context,
    /// v0.1 のキー (`memory.enabled`) と後の版のキー。中身は使うタスクで読む。
    pub memory: Option<toml::Table>,
    pub retrieval: Option<toml::Table>,
    pub learning: Option<toml::Table>,
    pub update: Option<toml::Table>,
    pub cartridge: Option<toml::Table>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Ui {
    pub language: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Prompt {
    pub language: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Llm {
    pub chat: Chat,
    pub allow_remote: bool,
    /// v0.2 のキー。
    pub embed: Option<toml::Table>,
    pub rerank: Option<toml::Table>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Chat {
    pub url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub generation: GenerationConfig,
    /// v0.1 の Should (`/model`)。
    pub models: Option<Vec<toml::Table>>,
    /// 応答の要求に足す JSON (07 §4.2)。
    pub request_extra: Option<toml::Table>,
    /// v0.2 のキー。
    pub aux_request_extra: Option<toml::Table>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GenerationConfig {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<u32>,
    pub seed: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Storage {
    pub data_root: Option<PathBuf>,
    pub backup_dir: Option<PathBuf>,
    pub allow_unsafe_location: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Context {
    pub reserve_for_output: Option<usize>,
    pub budget: Option<toml::Table>,
}

/// 読んだ設定と、警告 (後の版のキー)。
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub ignored: Vec<String>,
    /// 古い版の設定を書き換えたときの、元のファイルの写し (03 §12.3、FR-807)。
    pub migrated: Option<PathBuf>,
}

fn line_of(text: &str, key: &str) -> usize {
    text.lines()
        .position(|l| l.trim_start().starts_with(key))
        .map_or(0, |i| i + 1)
}

/// 読む。ファイルが無ければ既定の設定 (setup で作る、07 §5)。
///
/// `migrate` なら、古い版の設定 (`version` の無いもの = 版 0) を、元のファイルを
/// `config.toml.bak-v<旧版>` に写してから今の版に書き換える (03 §12.3、FR-807)。
/// 版 0 から 1 への移行は、先頭に `version = 1` を足すだけ (コメントを残す)。
pub fn load(path: &Path, migrate: bool) -> Result<Loaded, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut loaded = parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if migrate && loaded.config.version.is_none() && !text.trim().is_empty() {
        let backup = path.with_file_name(format!(
            "{}.bak-v0",
            path.file_name()
                .map_or("config.toml".into(), |n| n.to_string_lossy())
        ));
        let write = || -> std::io::Result<()> {
            std::fs::copy(path, &backup)?;
            let tmp = path.with_extension("toml.tmp");
            std::fs::write(
                &tmp,
                format!(
                    "version = {CONFIG_VERSION}
{text}"
                ),
            )?;
            std::fs::rename(&tmp, path)
        };
        write().map_err(|e| format!("{}: {e}", path.display()))?;
        loaded.config.version = Some(CONFIG_VERSION);
        loaded.migrated = Some(backup);
    }
    Ok(loaded)
}

/// 古い版の設定を今の版に書き換える ([`load`] の `migrate`)。書き換えたら、元のファイルの写しを返す。
/// TUI の起動は、更新の片付けの後にこれを呼ぶ (動いている evorag が置いてあるものと違えば、設定に触れずに
/// 止めるため、12 §7.3)。
pub fn migrate(path: &Path) -> Result<Option<PathBuf>, String> {
    load(path, true).map(|l| l.migrated)
}

/// `evorag update` が読む設定 (12 §7.2)。設定を書き換えない。
#[derive(Debug)]
pub struct UpdateConfig {
    pub data_root: Option<PathBuf>,
    pub backup_dir: Option<PathBuf>,
    /// `update.trusted_keys` (読むときに確かめた)。
    pub trusted_keys: Vec<evorag_update::TrustedKey>,
    /// 設定の形式の版 (`version`。書いていなければ 0。ファイルが無ければ `None`)。
    pub version: Option<u32>,
    /// 後の版のキー (警告する)。
    pub ignored: Vec<String>,
}

/// `evorag update` の設定を読む。設定は書き換えない (移行しない)。
///
/// `config.toml` がこの版の読めない新しい形式の版 (`version` が [`CONFIG_VERSION`] より大きい) でも、
/// 更新で古い版に戻した後や、新しい版を入れ直すときに使えるように、TOML として読めれば
/// `storage.data_root`・`storage.backup_dir`・`update.trusted_keys` だけを読む。`update.trusted_keys` の
/// 誤りと、ほかの設定の誤りは、これまでどおり誤りにする (終了コード 4)。
pub fn load_for_update(path: &Path) -> Result<UpdateConfig, String> {
    let (text, exists) = match std::fs::read_to_string(path) {
        Ok(t) => (t, true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), false),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let at = |e: String| format!("{}: {e}", path.display());
    match parse(&text) {
        Ok(l) => Ok(UpdateConfig {
            trusted_keys: l.config.trusted_keys(),
            data_root: l.config.storage.data_root,
            backup_dir: l.config.storage.backup_dir,
            version: exists.then(|| l.config.version.unwrap_or(0)),
            ignored: l.ignored,
        }),
        Err(e) => {
            let Ok(table) = toml::from_str::<toml::Table>(&text) else {
                return Err(at(e));
            };
            match table.get("version").and_then(toml::Value::as_integer) {
                Some(v) if v > i64::from(CONFIG_VERSION) => {
                    let version = u32::try_from(v).map_err(|_| {
                        at(format!(
                            "line {}: version = {v} is out of range",
                            line_of(&text, "version")
                        ))
                    })?;
                    newer_for_update(&text, &table, version).map_err(at)
                }
                _ => Err(at(e)),
            }
        }
    }
}

/// 設定が、この版の読めない新しい形式の版 (`version` が [`CONFIG_VERSION`] より大きい) なら、
/// `evorag update` と同じ読み方 ([`load_for_update`]) で読んだもの。ほか (読めない理由が版でない、
/// `evorag update` の読み方でも読めない) は `None`。
pub fn newer_config(path: &Path) -> Option<UpdateConfig> {
    load_for_update(path)
        .ok()
        .filter(|u| u.version.is_some_and(|v| v > CONFIG_VERSION))
}

/// 新しい形式の版の設定から、`evorag update` の使うキーだけを読む ([`load_for_update`])。
///
/// `storage`・`update` が表でないときは、既定の値を使わずに誤りにする (行番号つき)。
fn newer_for_update(text: &str, table: &toml::Table, version: u32) -> Result<UpdateConfig, String> {
    let section = |name: &str| -> Result<Option<&toml::Table>, String> {
        match table.get(name) {
            None => Ok(None),
            Some(toml::Value::Table(t)) => Ok(Some(t)),
            Some(_) => Err(format!(
                "line {}: {name} must be a table",
                line_of(text, name)
            )),
        }
    };
    let storage = section("storage")?;
    let update = section("update")?;
    let path_of = |key: &str| -> Result<Option<PathBuf>, String> {
        match storage.and_then(|s| s.get(key)) {
            None => Ok(None),
            Some(toml::Value::String(s)) => Ok(Some(PathBuf::from(s))),
            Some(_) => Err(format!(
                "line {}: storage.{key} must be a string",
                line_of(text, key)
            )),
        }
    };
    let trusted_keys = match update.and_then(|u| u.get(TRUSTED_KEYS)) {
        None => Vec::new(),
        Some(keys) => trusted_keys(keys).map_err(|e| {
            format!(
                "line {}: update.{TRUSTED_KEYS} {e}",
                line_of(text, TRUSTED_KEYS)
            )
        })?,
    };
    Ok(UpdateConfig {
        data_root: path_of("data_root")?,
        backup_dir: path_of("backup_dir")?,
        trusted_keys,
        version: Some(version),
        ignored: Vec::new(),
    })
}

pub fn parse(text: &str) -> Result<Loaded, String> {
    // toml の誤りは行と桁を含む。
    let config: Config = toml::from_str(text).map_err(|e| e.to_string().trim().to_owned())?;
    if let Some(v) = config.version
        && v != CONFIG_VERSION
    {
        return Err(format!(
            "line {}: version = {v} is not supported (this evorag reads version {CONFIG_VERSION})",
            line_of(text, "version")
        ));
    }
    let g = &config.llm.chat.generation;
    if let Some(t) = g.temperature
        && !(0.0..=2.0).contains(&t)
    {
        return Err(format!(
            "line {}: llm.chat.generation.temperature must be between 0 and 2",
            line_of(text, "temperature")
        ));
    }
    if let Some(p) = g.top_p
        && !(0.0..=1.0).contains(&p)
    {
        return Err(format!(
            "line {}: llm.chat.generation.top_p must be between 0 and 1",
            line_of(text, "top_p")
        ));
    }
    if let Some(l) = &config.prompt.language
        && !matches!(l.as_str(), "ja" | "en")
    {
        return Err(format!(
            "line {}: prompt.language must be \"ja\" or \"en\"",
            line_of(text, "language")
        ));
    }
    let mut ignored = Vec::new();
    for (name, t) in [
        ("llm.embed", &config.llm.embed),
        ("llm.rerank", &config.llm.rerank),
    ] {
        if let Some(t) = t {
            for k in ["url", "model", "api_key", "api_key_env"] {
                if t.get(k).is_some_and(|v| !v.is_str()) {
                    return Err(format!(
                        "line {}: {name}.{k} must be a string",
                        line_of(text, k)
                    ));
                }
            }
            for k in t
                .keys()
                .filter(|k| !["url", "model", "api_key", "api_key_env"].contains(&k.as_str()))
            {
                ignored.push(format!("{name}.{k}"));
            }
        }
    }
    if let Some(u) = &config.update {
        if let Some(keys) = u.get(TRUSTED_KEYS) {
            trusted_keys(keys).map_err(|e| {
                format!(
                    "line {}: update.{TRUSTED_KEYS} {e}",
                    line_of(text, TRUSTED_KEYS)
                )
            })?;
        }
        for k in u.keys().filter(|k| k.as_str() != TRUSTED_KEYS) {
            ignored.push(format!("update.{k}"));
        }
    }
    for (name, t) in [
        ("learning", &config.learning),
        ("cartridge", &config.cartridge),
    ] {
        if t.is_some() {
            ignored.push(name.to_owned());
        }
    }
    if let Some(m) = &config.memory {
        if m.get("enabled").is_some_and(|v| !v.is_bool()) {
            return Err(format!(
                "line {}: memory.enabled must be true or false",
                line_of(text, "enabled")
            ));
        }
        if m.get("sleep_time").is_some_and(|v| !v.is_bool()) {
            return Err(format!(
                "line {}: memory.sleep_time must be true or false",
                line_of(text, "sleep_time")
            ));
        }
        if m.get("idle_seconds")
            .is_some_and(|v| v.as_integer().is_none_or(|n| !(1..=86_400).contains(&n)))
        {
            return Err(format!(
                "line {}: memory.idle_seconds must be an integer from 1 to 86400",
                line_of(text, "idle_seconds")
            ));
        }
        if m.get("on_battery")
            .is_some_and(|v| v.as_str().and_then(evorag_core::OnBattery::parse).is_none())
        {
            return Err(format!(
                "line {}: memory.on_battery must be \"reduce\", \"normal\" or \"stop\"",
                line_of(text, "on_battery")
            ));
        }
        for k in m.keys().filter(|k| {
            !["enabled", "sleep_time", "idle_seconds", "on_battery"].contains(&k.as_str())
        }) {
            ignored.push(format!("memory.{k}"));
        }
    }
    if config
        .retrieval
        .as_ref()
        .and_then(|r| r.get("precompute"))
        .is_some_and(|v| !v.is_bool())
    {
        return Err(format!(
            "line {}: retrieval.precompute must be true or false",
            line_of(text, "precompute")
        ));
    }
    Ok(Loaded {
        config,
        ignored,
        migrated: None,
    })
}

/// `update.trusted_keys` のキー (07 §4.2)。
const TRUSTED_KEYS: &str = "trusted_keys";

/// `update.trusted_keys` の値を、更新の署名を確かめる鍵として読む (07 §4.2)。
fn trusted_keys(value: &toml::Value) -> Result<Vec<evorag_update::TrustedKey>, String> {
    let not_strings = || "must be an array of strings".to_owned();
    value
        .as_array()
        .ok_or_else(not_strings)?
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let s = v.as_str().ok_or_else(not_strings)?;
            evorag_update::TrustedKey::from_config(s).map_err(|e| format!("[{i}]: {e}"))
        })
        .collect()
}

impl Config {
    /// 設定で足した、更新の署名を確かめる鍵 (`update.trusted_keys`、07 §4.2)。値は読むときに確かめた。
    pub fn trusted_keys(&self) -> Vec<evorag_update::TrustedKey> {
        self.update
            .as_ref()
            .and_then(|u| u.get(TRUSTED_KEYS))
            .and_then(|v| trusted_keys(v).ok())
            .unwrap_or_default()
    }

    /// 裏の処理の設定 (`memory.sleep_time`・`idle_seconds`・`on_battery`、07 §4.2)。値は読むときに確かめた。
    pub fn sleep(&self) -> evorag_core::SleepConfig {
        let d = evorag_core::SleepConfig::default();
        let m = self.memory.as_ref();
        let get = |k: &str| m.and_then(|m| m.get(k));
        evorag_core::SleepConfig {
            enabled: get("sleep_time")
                .and_then(toml::Value::as_bool)
                .unwrap_or(d.enabled),
            idle: get("idle_seconds")
                .and_then(toml::Value::as_integer)
                .and_then(|n| u64::try_from(n).ok())
                .map_or(d.idle, std::time::Duration::from_secs),
            on_battery: get("on_battery")
                .and_then(toml::Value::as_str)
                .and_then(evorag_core::OnBattery::parse)
                .unwrap_or(d.on_battery),
        }
    }

    /// `retrieval.precompute` (07 §4.2)。書かないときは、ループバック以外を許した設定では切る (送っていない
    /// 入力が別の機械へ出るため、05 §4.5)。
    pub fn precompute(&self) -> bool {
        self.retrieval
            .as_ref()
            .and_then(|r| r.get("precompute"))
            .and_then(toml::Value::as_bool)
            .unwrap_or(!self.llm.allow_remote)
    }

    /// `memory.enabled` (既定は true、07 §4.2)。
    pub fn memory_enabled(&self) -> bool {
        self.memory
            .as_ref()
            .and_then(|m| m.get("enabled"))
            .and_then(toml::Value::as_bool)
            .unwrap_or(true)
    }
}

/// 既定の置き場所 (03 §2.1)。
pub struct Places {
    pub config_file: PathBuf,
    pub data_root: PathBuf,
    /// 状態のフォルダ (下書きと外部のエディタの一時ファイル、03 §2.1)。
    pub state_dir: PathBuf,
}

/// 環境変数の値 (空は無いものとみなす)。
fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

pub fn default_places() -> Option<Places> {
    let app = "evorag";
    if cfg!(windows) {
        // Roaming (%APPDATA%) は使わない (03 §2.1)。
        let base = env_path("LOCALAPPDATA")?.join(app);
        Some(Places {
            config_file: base.join("config").join("config.toml"),
            data_root: base.join("data"),
            state_dir: base.join("state"),
        })
    } else if cfg!(target_os = "macos") {
        let base = env_path("HOME")?
            .join("Library")
            .join("Application Support")
            .join(app);
        let home = env_path("HOME")?;
        Some(Places {
            config_file: base.join("config").join("config.toml"),
            data_root: base.join("data"),
            state_dir: home.join("Library").join("Caches").join(app),
        })
    } else {
        let home = env_path("HOME");
        let config =
            env_path("XDG_CONFIG_HOME").or_else(|| home.as_ref().map(|h| h.join(".config")))?;
        let data = env_path("XDG_DATA_HOME")
            .or_else(|| home.as_ref().map(|h| h.join(".local").join("share")))?;
        let state = env_path("XDG_STATE_HOME")
            .or_else(|| home.as_ref().map(|h| h.join(".local").join("state")))?;
        Some(Places {
            config_file: config.join(app).join("config.toml"),
            data_root: data.join(app),
            state_dir: state.join(app),
        })
    }
}

/// embed・rerank の役割の接続先の設定 (07 §4.2)。`url` が無ければ、その役割は無い。
pub struct RoleConfig {
    pub url: String,
    pub api_key: Option<String>,
    /// ルーターモードのときのモデルの名前 (06 §2)。
    pub model: Option<String>,
}

impl Config {
    pub fn role(&self, role: &str) -> Option<RoleConfig> {
        let t = match role {
            "embed" => self.llm.embed.as_ref()?,
            "rerank" => self.llm.rerank.as_ref()?,
            _ => return None,
        };
        let s = |k: &str| t.get(k).and_then(|v| v.as_str()).map(str::to_owned);
        let url = s("url").filter(|u| !u.trim().is_empty())?;
        let api_key = s("api_key").or_else(|| s("api_key_env").and_then(|v| std::env::var(v).ok()));
        Some(RoleConfig {
            url,
            api_key,
            model: s("model"),
        })
    }
}

/// v0.2 で使わなくなったキーと、その内容のハッシュ (07 §4.2・§4.1)。読んで無視し、内容が変わった
/// ときだけ知らせる (知らせたかはデータ根に記録する)。
pub fn obsolete(config: &Config) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(models) = &config.llm.chat.models {
        let text = models
            .iter()
            .map(|t| t.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        out.push(("llm.chat.models".to_owned(), fnv(&text)));
    }
    if let Some(t) = config.retrieval.as_ref().and_then(|r| r.get("thresholds")) {
        out.push(("retrieval.thresholds".to_owned(), fnv(&t.to_string())));
    }
    out
}

/// 版をまたいで変わらないハッシュ (FNV-1a、64 ビット)。
fn fnv(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_gives_the_defaults() {
        let l = parse("").unwrap();
        assert!(l.config.llm.chat.url.is_none());
        assert!(l.ignored.is_empty());
    }

    #[test]
    fn reads_the_v0_1_keys() {
        let l = parse(
            r#"
version = 1
[prompt]
language = "en"
[llm]
allow_remote = false
[llm.chat]
url = "http://127.0.0.1:9000"
api_key_env = "KEY"
[llm.chat.generation]
temperature = 0.2
max_tokens = 256
[llm.chat.request_extra]
chat_template_kwargs = { enable_thinking = false }
[memory]
enabled = true
"#,
        )
        .unwrap();
        assert_eq!(
            l.config.llm.chat.url.as_deref(),
            Some("http://127.0.0.1:9000")
        );
        assert_eq!(l.config.llm.chat.generation.max_tokens, Some(256));
        assert_eq!(l.config.prompt.language.as_deref(), Some("en"));
        assert!(l.config.llm.chat.request_extra.is_some());
        assert!(l.ignored.is_empty());
    }

    /// verifies: AC-FR-301-2 (設定の層: memory.sleep_time・idle_seconds・on_battery)
    #[test]
    fn sleep_settings_are_read_and_checked() {
        let d = parse("").unwrap().config.sleep();
        assert_eq!(d, evorag_core::SleepConfig::default());
        let s = parse("[memory]\nsleep_time = false\nidle_seconds = 120\non_battery = \"stop\"\n")
            .unwrap()
            .config
            .sleep();
        assert!(!s.enabled);
        assert_eq!(s.idle, std::time::Duration::from_secs(120));
        assert_eq!(s.on_battery, evorag_core::OnBattery::Stop);
        for bad in [
            "[memory]\non_battery = \"sometimes\"\n",
            "[memory]\nidle_seconds = 0\n",
            "[memory]\nsleep_time = \"yes\"\n",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn keys_of_later_versions_are_ignored_with_a_warning() {
        // v0.2 から llm.embed の url は使う。知らないキーだけを警告して無視する。
        let l = parse(
            "[llm.embed]\nurl = \"http://127.0.0.1:8081\"\nfuture = 1\n[memory]\nsleep_time = true\nfuture = 2\n",
        )
        .unwrap();
        assert_eq!(l.ignored, ["llm.embed.future", "memory.future"]);
        let r = l.config.role("embed").unwrap();
        assert_eq!(r.url, "http://127.0.0.1:8081");
        assert!(l.config.role("rerank").is_none());
    }

    /// `update.trusted_keys` は読むときに確かめ、鍵として読めない値は行番号つきの誤りにする (07 §4.2)。
    #[test]
    fn trusted_keys_are_checked_when_the_config_is_read() {
        let line = include_str!("../../../minisign.pub")
            .lines()
            .nth(1)
            .expect("the second line of minisign.pub");
        let l = parse(&format!(
            "version = 1
[update]
trusted_keys = [\"{line}\"]
future = 1
"
        ))
        .unwrap();
        assert_eq!(l.config.trusted_keys().len(), 1);
        assert_eq!(l.ignored, ["update.future"]);
        assert!(
            parse(
                "[update]
"
            )
            .unwrap()
            .config
            .trusted_keys()
            .is_empty()
        );
        for bad in [
            "[update]
trusted_keys = \"RWQ\"
",
            "[update]
trusted_keys = [1]
",
            "[update]
trusted_keys = [\"not base64!\"]
",
            "[update]
trusted_keys = [\"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"]
",
        ] {
            let e = parse(bad).expect_err(bad);
            assert!(e.starts_with("line 2: update.trusted_keys"), "{e}");
        }
    }

    /// verifies: AC-FR-415-2 (ループバック以外を許した設定では、書かなければ入力が止まったときの処理を切る)
    #[test]
    fn precompute_defaults_to_off_only_when_remote_servers_are_allowed() {
        assert!(parse("").unwrap().config.precompute());
        assert!(
            !parse("[llm]\nallow_remote = true\n")
                .unwrap()
                .config
                .precompute()
        );
        assert!(
            parse("[llm]\nallow_remote = true\n[retrieval]\nprecompute = true\n")
                .unwrap()
                .config
                .precompute()
        );
        assert!(
            !parse("[retrieval]\nprecompute = false\n")
                .unwrap()
                .config
                .precompute()
        );
        let e = parse("[retrieval]\nprecompute = \"yes\"\n").unwrap_err();
        assert!(
            e.contains("line 2") && e.contains("retrieval.precompute"),
            "{e}"
        );
    }

    /// verifies: AC-FR-108-2 (使わなくなったキーを見つける)
    #[test]
    fn obsolete_keys_are_found_with_a_stable_hash() {
        let l =
            parse("[[llm.chat.models]]\nname = \"a\"\nurl = \"http://127.0.0.1:8081\"\n").unwrap();
        let o = obsolete(&l.config);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].0, "llm.chat.models");
        assert_eq!(o, obsolete(&l.config));
        assert!(obsolete(&parse("").unwrap().config).is_empty());
    }

    /// FR-904: 読めないキー・型の誤り・範囲外の値は、行番号つきのエラー。
    /// verifies: AC-FR-904-1
    #[test]
    fn unknown_keys_bad_types_and_ranges_are_errors_with_a_line() {
        let e = parse("[llm.chat]\nurll = \"x\"\n").unwrap_err();
        assert!(e.contains("line 2"), "{e}");
        let e = parse("[llm]\nallow_remote = \"yes\"\n").unwrap_err();
        assert!(e.contains("line 2"), "{e}");
        let e = parse("[llm.chat.generation]\n\ntemperature = 9\n").unwrap_err();
        assert!(e.contains("line 3"), "{e}");
        let e = parse("version = 2\n").unwrap_err();
        assert!(e.contains("line 1"), "{e}");
    }

    /// verifies: AC-FR-807-1
    #[test]
    fn an_old_config_is_backed_up_and_rewritten() {
        let dir = std::env::temp_dir().join(format!("evorag-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let old = "# 自分のメモ
[llm.chat]
url = \"http://127.0.0.1:8081\"
";
        std::fs::write(&path, old).unwrap();

        // 読むだけ (doctor) のときは書き換えない。
        let l = load(&path, false).unwrap();
        assert_eq!((l.config.version, l.migrated), (None, None));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old);

        let l = load(&path, true).unwrap();
        let backup = dir.join("config.toml.bak-v0");
        assert_eq!(l.migrated.as_deref(), Some(backup.as_path()));
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), old);
        let new = std::fs::read_to_string(&path).unwrap();
        assert!(
            new.starts_with(
                "version = 1
"
            ),
            "{new}"
        );
        assert!(new.contains("# 自分のメモ"), "comments are kept");
        assert_eq!(
            l.config.llm.chat.url.as_deref(),
            Some("http://127.0.0.1:8081")
        );
        // 2 回目は何もしない。
        assert_eq!(load(&path, true).unwrap().migrated, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `evorag update` は、この版の読めない新しい形式の設定でも、データ根・退避先・足した鍵だけを読んで
    /// 動く。鍵の誤りと、ほかの設定の誤りは誤りのまま (終了コード 4)。設定は書き換えない。
    #[test]
    fn update_reads_only_its_keys_from_a_newer_config() {
        let dir = evorag_store::testutil::TempDir::new("config-update-newer");
        let path = dir.path().join("config.toml");
        let line = include_str!("../../../minisign.pub")
            .lines()
            .nth(1)
            .expect("the second line of minisign.pub");
        let keys = format!(
            "trusted_keys = [\"{line}\"]
"
        );
        let text = format!(
            "version = 9
future = true
[storage]
data_root = \"d\"
backup_dir = \"b\"
new_key = 1
[update]
{keys}channel = \"x\"
"
        );
        std::fs::write(&path, &text).unwrap();
        assert!(load(&path, false).is_err(), "the normal reader refuses it");
        let u = load_for_update(&path).expect("update can read it");
        assert_eq!(u.data_root.as_deref(), Some(Path::new("d")));
        assert_eq!(u.backup_dir.as_deref(), Some(Path::new("b")));
        assert_eq!(u.version, Some(9));
        assert_eq!(u.trusted_keys.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            text,
            "not rewritten"
        );

        for bad in [
            "version = 9
[update]
trusted_keys = [\"not-a-key\"]
",
            "version = 9
[storage]
data_root = 1
",
            "version = 9
[storage
",
            // 今の版の設定の誤りは、これまでどおり誤り。
            "version = 1
unknown = 1
",
            "version = 1
[update]
trusted_keys = [1]
",
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(load_for_update(&path).is_err(), "{bad}");
        }
        // 古い版 (version の無いもの) は読めて、書き換えない。
        std::fs::write(
            &path,
            "[storage]
data_root = \"d\"
",
        )
        .unwrap();
        let u = load_for_update(&path).expect("v0");
        assert_eq!(u.version, Some(0));
        assert!(!dir.path().join("config.toml.bak-v0").exists());
        // ファイルが無ければ、版も無い。
        let u = load_for_update(&dir.path().join("none.toml")).expect("missing");
        assert_eq!(u.version, None);
    }

    /// `evorag update` の読み方でも、表でない `storage`・`update` と、範囲外の `version` は、既定の値を
    /// 使わずに行番号つきの誤りにする (終了コード 4)。
    #[test]
    fn update_refuses_malformed_sections_of_a_newer_config() {
        let dir = evorag_store::testutil::TempDir::new("config-update-malformed");
        let path = dir.path().join("config.toml");
        for (bad, line, what) in [
            ("version = 9\nstorage = 1\n", 2, "storage must be a table"),
            (
                "version = 9\n\nupdate = \"x\"\n",
                3,
                "update must be a table",
            ),
            ("version = 9\nupdate = [1]\n", 2, "update must be a table"),
            ("version = 99999999999\n", 1, "out of range"),
        ] {
            std::fs::write(&path, bad).unwrap();
            let e = load_for_update(&path).expect_err(bad);
            assert!(e.contains(&format!("line {line}: ")), "{bad}: {e}");
            assert!(e.contains(what), "{bad}: {e}");
            assert!(newer_config(&path).is_none(), "{bad}");
        }
    }

    /// TUI が設定の誤りで止まる前に更新の片付けをするのは、設定がこの版の読めない新しい形式の版のとき
    /// だけ。データ根は `evorag update` と同じ読み方で読む。
    #[test]
    fn only_a_newer_config_is_read_for_the_startup_cleanup() {
        let dir = evorag_store::testutil::TempDir::new("config-newer");
        let path = dir.path().join("config.toml");
        let text = "version = 2\nfuture = 1\n[storage]\ndata_root = \"d\"\n";
        std::fs::write(&path, text).unwrap();
        assert!(load(&path, false).is_err());
        let u = newer_config(&path).expect("a newer config");
        assert_eq!(u.data_root.as_deref(), Some(Path::new("d")));
        assert_eq!(u.version, Some(2));
        for other in [
            "version = 1\nunknown = 1\n",
            "unknown = 1\n",
            "version = 1\n",
            "[storage\n",
        ] {
            std::fs::write(&path, other).unwrap();
            assert!(newer_config(&path).is_none(), "{other}");
        }
        assert!(newer_config(&dir.path().join("none.toml")).is_none());
    }

    /// 配布物に同梱する設定の例が、そのまま読める (12 §5.1)。
    #[test]
    fn the_shipped_example_is_valid() {
        let text = include_str!("../../../dist/config.example.toml");
        let l = parse(text).unwrap();
        assert!(l.ignored.is_empty(), "{:?}", l.ignored);
        assert_eq!(l.config.version, Some(CONFIG_VERSION));
    }
}
