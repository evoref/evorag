//! 入力欄の下書き (04 §3) と外部のエディタ (07 §2.4、FR-107)。
//!
//! - 下書きは状態のフォルダの `draft.md` (所有者だけの権限) に 5 秒ごとに保存し、次の起動時に戻す。
//!   送ったらすぐ消す。
//! - Ctrl+E では同じファイルを外部のエディタで開き、閉じたら内容を入力欄に戻す。エディタは
//!   `VISUAL` → `EDITOR` → (Windows は) `notepad.exe` の順に選ぶ。

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const DRAFT_FILE: &str = "draft.md";

pub struct Draft {
    path: PathBuf,
    saved: String,
}

/// 所有者だけが読み書きできるファイルに書く。
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(path)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()
}

impl Draft {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join(DRAFT_FILE),
            saved: String::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 前の起動で残った下書き。
    pub fn load(&mut self) -> Option<String> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        self.saved.clone_from(&text);
        (!text.is_empty()).then_some(text)
    }

    /// 入力欄の中身が変わっていれば保存する。空なら消す。
    pub fn save(&mut self, text: &str) -> io::Result<()> {
        if text == self.saved {
            return Ok(());
        }
        if text.is_empty() {
            self.clear();
            return Ok(());
        }
        write_private(&self.path, text)?;
        self.saved = text.to_owned();
        Ok(())
    }

    /// 送った後に消す。
    pub fn clear(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        self.saved.clear();
    }

    /// `text` をファイルに書いて外部のエディタで開き、閉じた後の内容を返す。
    /// 端末を元に戻すのは呼び出し側。
    pub fn edit(&mut self, text: &str) -> io::Result<String> {
        write_private(&self.path, text)?;
        let editor =
            editor_command().ok_or_else(|| io::Error::other("no editor (set VISUAL or EDITOR)"))?;
        let mut parts = editor.split_whitespace();
        let program = parts.next().unwrap_or_default();
        let status = Command::new(program).args(parts).arg(&self.path).status()?;
        if !status.success() {
            return Err(io::Error::other(format!("the editor exited with {status}")));
        }
        let mut out = std::fs::read_to_string(&self.path)?;
        // エディタが足す末尾の改行は取る。
        while out.ends_with('\n') || out.ends_with('\r') {
            out.pop();
        }
        self.saved.clone_from(&out);
        Ok(out)
    }
}

/// 使うエディタの命令 (`VISUAL` → `EDITOR` → Windows は `notepad.exe`)。
pub fn editor_command() -> Option<String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    env("VISUAL")
        .or_else(|| env("EDITOR"))
        .or_else(|| cfg!(windows).then(|| "notepad.exe".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "evorag-draft-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn saves_restores_and_clears() {
        let dir = temp();
        let mut d = Draft::new(&dir);
        assert_eq!(d.load(), None);
        d.save("書きかけ").unwrap();
        let mut again = Draft::new(&dir);
        assert_eq!(again.load().as_deref(), Some("書きかけ"));
        d.save("").unwrap();
        assert!(!d.path().exists(), "an empty input removes the draft");
        d.save("x").unwrap();
        d.clear();
        assert!(!d.path().exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_draft_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp();
        let mut d = Draft::new(&dir);
        d.save("秘密").unwrap();
        let mode = std::fs::metadata(d.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(dir);
    }
}
