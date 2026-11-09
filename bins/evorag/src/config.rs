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
}

fn line_of(text: &str, key: &str) -> usize {
    text.lines()
        .position(|l| l.trim_start().starts_with(key))
        .map_or(0, |i| i + 1)
}

/// 読む。ファイルが無ければ既定の設定 (setup で作る、07 §5)。
pub fn load(path: &Path) -> Result<Loaded, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    parse(&text).map_err(|e| format!("{}: {e}", path.display()))
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
    if config.llm.embed.is_some() {
        ignored.push("llm.embed".to_owned());
    }
    if config.llm.rerank.is_some() {
        ignored.push("llm.rerank".to_owned());
    }
    if config.llm.chat.aux_request_extra.is_some() {
        ignored.push("llm.chat.aux_request_extra".to_owned());
    }
    for (name, t) in [
        ("learning", &config.learning),
        ("update", &config.update),
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
        for k in m.keys().filter(|k| k.as_str() != "enabled") {
            ignored.push(format!("memory.{k}"));
        }
    }
    Ok(Loaded { config, ignored })
}

impl Config {
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

    #[test]
    fn keys_of_later_versions_are_ignored_with_a_warning() {
        let l =
            parse("[llm.embed]\nurl = \"http://127.0.0.1:8081\"\n[memory]\nsleep_time = true\n")
                .unwrap();
        assert_eq!(l.ignored, ["llm.embed", "memory.sleep_time"]);
    }

    /// FR-904: 読めないキー・型の誤り・範囲外の値は、行番号つきのエラー。
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
}
