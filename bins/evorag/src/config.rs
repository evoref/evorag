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
    Ok(Loaded {
        config,
        ignored,
        migrated: None,
    })
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

/// embed・rerank の役割の接続先の設定 (07 §4.2)。`url` が無ければ、その役割は無い。
pub struct RoleConfig {
    pub url: String,
    pub api_key: Option<String>,
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
        Some(RoleConfig { url, api_key })
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

    #[test]
    fn keys_of_later_versions_are_ignored_with_a_warning() {
        // v0.2 から llm.embed の url は使う。知らないキーだけを警告して無視する。
        let l = parse(
            "[llm.embed]\nurl = \"http://127.0.0.1:8081\"\nfuture = 1\n[memory]\nsleep_time = true\n",
        )
        .unwrap();
        assert_eq!(l.ignored, ["llm.embed.future", "memory.sleep_time"]);
        let r = l.config.role("embed").unwrap();
        assert_eq!(r.url, "http://127.0.0.1:8081");
        assert!(l.config.role("rerank").is_none());
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

    /// 配布物に同梱する設定の例が、そのまま読める (12 §5.1)。
    #[test]
    fn the_shipped_example_is_valid() {
        let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let text = std::fs::read_to_string(root.join("../../dist/config.example.toml")).unwrap();
        let l = parse(&text).unwrap();
        assert!(l.ignored.is_empty(), "{:?}", l.ignored);
        assert_eq!(l.config.version, Some(CONFIG_VERSION));
    }
}
