//! 開くデータ根の形式 (03 §12.4、08 §2.6 の「ステップの分け方」)。
//!
//! 製品 (`evorag` の TUI と CLI) は、v0.5 の根 (形式の版 `0.5`、v0.5 の種類) だけを開いて作る。v0.4 の根 (形式の版
//! `0.3`) は在り場所で移行せず、[`OpenError::NeedsImport`] で断る (TUI は取り込むかを尋ね、ほかのコマンドは
//! `evorag import-v04` を案内する)。v0.5 の根では、旧エンジンの裏の処理 (ノート・事実の抽出と統合・訂正の検証・
//! 事実の表) を動かさない。
//!
//! 旧エンジンは、M5.5 の判定の前まで比較の台 (試験とベンチ) の中だけで動く。その台は [`RootFormat::V04Bench`] で
//! v0.4 の形式の根を作って開く (製品の経路からは選べない)。

use evorag_store::lock::DataRootLock;
use evorag_store::migrate::FormatPlan;
use evorag_store::{KindSpec, OpenError, OpenOptions, OpenReport, Store};

/// 開く (無ければ作る) データ根の形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RootFormat {
    /// v0.5 の根 (形式の版 `0.5`、[`evorag_memory::kinds_v05`])。製品の経路はこれだけを使う。v0.4 の根は
    /// [`OpenError::NeedsImport`] で、正しく閉じていない・DB の無い・読めない v0.4 の根は
    /// [`OpenError::NeedsV04Open`] で断る (どちらも何も書かない、03 §8)。
    #[default]
    V05,
    /// 比較の台と試験 (ベンチを含む) だけ: v0.4 の根 (形式の版 `0.3`、[`evorag_memory::kinds`]) を開いて作り、旧エンジンの
    /// 裏の処理を動かす (08 §2.6)。製品の経路では使わない。旧エンジンを外すとき (M5.5) に、この変種も外す。
    V04Bench,
}

impl RootFormat {
    /// この形式の根の種類 (名前の順) と、書く形式。
    pub(crate) fn kinds_and_plan(self) -> (Vec<KindSpec>, FormatPlan) {
        let (mut kinds, plan) = match self {
            RootFormat::V05 => evorag_memory::v05_root(),
            RootFormat::V04Bench => (evorag_memory::kinds(), FormatPlan::current()),
        };
        kinds.sort_by_key(|k| k.name);
        (kinds, plan)
    }

    /// `opts` の種類をこの形式のものにして、データ根を書き込みで開く (無ければ作る)。`lock` は呼ぶ側が先に取った
    /// データ根のロック (`None` ならストアが取る)。
    ///
    /// # Errors
    /// [`Store::open_with`] と同じ。v0.5 の形式で v0.4 の根を開こうとしたときは [`OpenError::NeedsImport`]。
    pub(crate) fn open(
        self,
        opts: OpenOptions,
        lock: Option<DataRootLock>,
    ) -> Result<(Store, OpenReport), OpenError> {
        let (kinds, plan) = self.kinds_and_plan();
        let opts = OpenOptions { kinds, ..opts };
        match lock {
            Some(lock) => Store::open_locked_with(opts, plan, lock),
            None => Store::open_with(opts, plan),
        }
    }
}
