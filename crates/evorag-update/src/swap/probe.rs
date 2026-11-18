//! 実行ファイルを `--version` で動かして版を読む (12 §7.2 の apply の 10 と rollback の 2)。

use std::io::{Read, Seek};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::{DISPLAY_MAX_BYTES, Version};

/// 読む標準出力の上限 (バイト、12 §7.2 の apply の 10)。
pub const PROBE_MAX_OUTPUT: u64 = 4096;

/// 子のプロセスを待つ上限 (12 §7.2 の apply の 10)。
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// 終わるのを見る間隔。
const POLL: Duration = Duration::from_millis(20);

/// 実行ファイルを `--version` で動かす。試験では偽物に差し替える。
pub trait VersionProbe {
    /// `exe` (絶対パス) を `--version` を付けて動かし、標準出力の先頭 ([`PROBE_MAX_OUTPUT`] まで) を返す。
    ///
    /// # Errors
    ///
    /// 動かせない、時間切れ、失敗の終了コードで終わった。
    fn run_version(&self, exe: &Path) -> Result<String, ProbeError>;
}

/// `--version` を動かせなかった理由。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProbeError {
    /// 起動できない・出力を受けられない。
    #[error("cannot run it: {0}")]
    Spawn(#[source] std::io::Error),
    /// 時間内に終わらなかった (止めて、終わるのを待った)。
    #[error("it did not finish within {} seconds", .0.as_secs())]
    TimedOut(Duration),
    /// 失敗の終了コードで終わった。
    #[error("it exited with {0}")]
    Failed(std::process::ExitStatus),
}

/// 子のプロセスで `--version` を動かす。標準出力は一時のフォルダのファイルに受け (スレッドを使わず、
/// 子が出力で詰まらないように)、終わるのを短い間隔で見る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessProbe {
    timeout: Duration,
}

impl Default for ProcessProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessProbe {
    /// 待つ上限が [`PROBE_TIMEOUT`] のもの。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            timeout: PROBE_TIMEOUT,
        }
    }

    /// 待つ上限を変える (試験のため)。
    #[must_use]
    pub const fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl VersionProbe for ProcessProbe {
    fn run_version(&self, exe: &Path) -> Result<String, ProbeError> {
        let exe = std::path::absolute(exe).map_err(ProbeError::Spawn)?;
        let (mut out, out_path) = output_file().map_err(ProbeError::Spawn)?;
        let result = run(&exe, &out, self.timeout).and_then(|()| {
            out.rewind().map_err(ProbeError::Spawn)?;
            let mut bytes = Vec::new();
            (&mut out)
                .take(PROBE_MAX_OUTPUT)
                .read_to_end(&mut bytes)
                .map_err(ProbeError::Spawn)?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        });
        drop(out);
        // 片付けに失敗しても、結果は変わらない (一時のフォルダのファイル)。
        let _ = std::fs::remove_file(&out_path);
        result
    }
}

fn run(exe: &Path, out: &std::fs::File, timeout: Duration) -> Result<(), ProbeError> {
    let stdout = out.try_clone().map_err(ProbeError::Spawn)?;
    let mut child = Command::new(exe)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::null())
        .spawn()
        .map_err(ProbeError::Spawn)?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(ProbeError::Spawn)? {
            return if status.success() {
                Ok(())
            } else {
                Err(ProbeError::Failed(status))
            };
        }
        if start.elapsed() >= timeout {
            // 止めてから、終わるのを待つ (12 §7.2 の apply の 10)。
            let _ = child.kill();
            let _ = child.wait();
            return Err(ProbeError::TimedOut(timeout));
        }
        std::thread::sleep(POLL);
    }
}

/// 子の標準出力を受けるファイルを、一時のフォルダに排他で作る。
fn output_file() -> std::io::Result<(std::fs::File, std::path::PathBuf)> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let path = std::env::temp_dir().join(format!(
        "evorag-version-{}-{nanos}-{}.out",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let file = evorag_store::fsutil::create_new_file(&path, 0o600)?;
    Ok((file, path))
}

/// `--version` の出力から読んだもの (07 §5)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VersionOutput {
    pub version: Version,
    pub db_format: Option<String>,
}

/// 1 行目 (表示用に整えたもの、改行と CR を除く)。
pub(crate) fn first_line(output: &str) -> String {
    let line = output.split('\n').next().unwrap_or_default();
    crate::sanitize_for_display(line.trim_end_matches('\r'), DISPLAY_MAX_BYTES)
}

/// 1 行目が `evorag <版>` ならその版と、`db format: <x.y>` の行を読む。
pub(crate) fn parse_version_output(output: &str) -> Option<VersionOutput> {
    let mut lines = output.split('\n').map(|l| l.trim_end_matches('\r'));
    let version = lines.next()?.strip_prefix("evorag ")?.parse().ok()?;
    let db_format = lines
        .find_map(|l| l.strip_prefix("db format: "))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    Some(VersionOutput { version, db_format })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_lines_are_parsed() {
        let out = "evorag 0.3.1\r\ncommit: abc\ndb format: 0.3\n";
        assert_eq!(
            parse_version_output(out),
            Some(VersionOutput {
                version: Version::new(0, 3, 1),
                db_format: Some("0.3".to_owned())
            })
        );
        assert_eq!(
            parse_version_output("evorag 0.2.0\n").map(|o| o.db_format),
            Some(None)
        );
        assert_eq!(parse_version_output("evorag v0.2.0\n"), None);
        assert_eq!(parse_version_output("other 0.2.0\n"), None);
        assert_eq!(first_line("evorag 1.2.3\r\nx"), "evorag 1.2.3");
        assert_eq!(first_line("a\u{1b}[31mb"), "a[31mb");
    }

    /// 書いたスクリプトを `--version` で動かす (Windows は .cmd、Unix は sh)。
    fn script(dir: &Path, body_unix: &str, body_windows: &str) -> std::path::PathBuf {
        if cfg!(windows) {
            let p = dir.join("fake.cmd");
            std::fs::write(&p, format!("@echo off\r\n{body_windows}\r\n")).expect("write");
            p
        } else {
            let p = dir.join("fake.sh");
            std::fs::write(&p, format!("#!/bin/sh\n{body_unix}\n")).expect("write");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
            p
        }
    }

    #[test]
    fn the_process_probe_reads_the_first_bytes_of_stdout() {
        let dir = evorag_store::testutil::TempDir::new("probe-ok");
        let exe = script(dir.path(), "echo \"evorag $1\"", "echo evorag %1");
        let out = ProcessProbe::new().run_version(&exe).expect("runs");
        assert_eq!(first_line(&out), "evorag --version");
    }

    #[test]
    fn the_process_probe_caps_the_output() {
        let dir = evorag_store::testutil::TempDir::new("probe-cap");
        let exe = script(
            dir.path(),
            "i=0; while [ $i -lt 2000 ]; do echo 0123456789; i=$((i+1)); done",
            "for /L %%i in (1,1,2000) do @echo 0123456789",
        );
        let out = ProcessProbe::new().run_version(&exe).expect("runs");
        assert_eq!(out.len() as u64, PROBE_MAX_OUTPUT);
    }

    #[test]
    fn the_process_probe_reports_failures_and_timeouts() {
        let dir = evorag_store::testutil::TempDir::new("probe-fail");
        let exe = script(dir.path(), "exit 3", "exit /b 3");
        assert!(matches!(
            ProcessProbe::new().run_version(&exe),
            Err(ProbeError::Failed(_))
        ));
        let slow = script(dir.path(), "sleep 20", "ping -n 20 127.0.0.1 >nul");
        let start = Instant::now();
        let r = ProcessProbe::with_timeout(Duration::from_millis(500)).run_version(&slow);
        assert!(matches!(r, Err(ProbeError::TimedOut(_))), "{r:?}");
        assert!(start.elapsed() < Duration::from_secs(10));
        assert!(matches!(
            ProcessProbe::new().run_version(&dir.path().join("missing")),
            Err(ProbeError::Spawn(_))
        ));
    }

    /// Windows: 拡張子が `.exe` でない名前 (`evorag.prev`) の実行ファイルも動かせる (rollback の 2)。
    #[test]
    fn an_executable_named_evorag_prev_can_be_started() {
        let dir = evorag_store::testutil::TempDir::new("probe-prev");
        let prev = dir.path().join("evorag.prev");
        std::fs::copy(std::env::current_exe().expect("exe"), &prev).expect("copy");
        // 試験の実行ファイルは `--version` を知らないので失敗で終わるが、起動はできる。
        match ProcessProbe::new().run_version(&prev) {
            Ok(_) | Err(ProbeError::Failed(_)) => {}
            Err(e) => panic!("cannot start evorag.prev: {e}"),
        }
    }
}
