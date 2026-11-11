//! `evorag setup` でのモデルの記録と切り替え (06 §3.2、FR-513、ADR-0018)。
//!
//! setup は TUI を閉じてから DB に書く (03 §9)。データ根のロックを取ってストアを開き、役割ごとの
//! 記録を読み、確かめた役割だけを書き換える。書き換えは 1 つの役割ごとに 1 つのコミット。
//! 作り直しの手順 (埋め込みの作り直しは M2.5、しきい値の求め直しは M2.3) は、それぞれの版で
//! 同じコミットに job として足す。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use evorag_memory::{ModelOrigin, Writes};
use evorag_store::{OpenError, OpenOptions, Store, WriteRequest, Writer};
use evorag_types::{Clock, IdGen};

/// 1 つの役割の、記録と繋がっているモデルの違い。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleDiff {
    /// 記録が無い (記録する)。
    New,
    /// 記録と同じ。
    Same,
    /// 記録と違う。`maybe_same` は、ファイル名だけが違い `meta` が同じ (同じモデルとして扱える候補)。
    Different {
        recorded: Box<ModelOrigin>,
        maybe_same: bool,
    },
}

/// 記録と繋がっているモデルを比べる。リランカーは「同じモデルとして扱う」を出さない
/// (しきい値は同梱の事例から求め直せるので、06 §3.2 の 7)。
pub fn diff(role: &str, recorded: Option<&ModelOrigin>, connected: &ModelOrigin) -> RoleDiff {
    match recorded {
        None => RoleDiff::New,
        Some(r) if r.model_key == connected.model_key => RoleDiff::Same,
        Some(r) => RoleDiff::Different {
            recorded: Box::new(r.clone()),
            maybe_same: role != evorag_memory::ROLE_RERANK && crate::same_meta(r, connected),
        },
    }
}

/// setup が開いたデータ根 (ロックを持つ)。
pub struct DataRootModels {
    store: Arc<Store>,
    handle: evorag_store::WriterHandle,
    thread: Option<std::thread::JoinHandle<()>>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
}

impl DataRootModels {
    /// データ根を開く。TUI が動いていれば、ロックが取れずに `OpenError::Lock` になる。
    pub fn open(
        data_root: &Path,
        evorag_version: &str,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGen>,
    ) -> Result<Self, OpenError> {
        let (store, _) = Store::open(OpenOptions {
            data_root: data_root.to_path_buf(),
            evorag_version: evorag_version.to_owned(),
            kinds: evorag_memory::kinds(),
            clock: clock.clone(),
            ids: ids.clone(),
            config_file: None,
        })?;
        let store = Arc::new(store);
        let (writer, handle) =
            Writer::new(store.clone()).map_err(|e| OpenError::Unreadable(e.to_string()))?;
        let thread = std::thread::Builder::new()
            .name("evorag-setup-writer".into())
            .spawn(move || writer.run())
            .map_err(|e| OpenError::Unreadable(e.to_string()))?;
        Ok(Self {
            store,
            handle,
            thread: Some(thread),
            clock,
            ids,
        })
    }

    /// 役割ごとの記録したモデル。
    pub fn recorded(&self) -> BTreeMap<String, ModelOrigin> {
        self.store
            .read()
            .ok()
            .and_then(|r| evorag_memory::load_bindings(&r).ok())
            .map(|b| {
                b.by_role
                    .into_iter()
                    .map(|(k, v)| (k, v.body.current))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 役割のモデルを記録する (前のモデルは履歴に残る)。
    pub fn bind(&self, role: &str, origin: ModelOrigin) -> Result<(), String> {
        let bindings = self
            .store
            .read()
            .map_err(|e| e.to_string())
            .and_then(|r| evorag_memory::load_bindings(&r).map_err(|e| e.to_string()))?;
        let w = Writes {
            clock: &*self.clock,
            ids: &*self.ids,
        };
        let ops = w
            .bind_model(&bindings, role, origin)
            .map_err(|e| e.to_string())?;
        self.handle
            .submit_wait(WriteRequest::conversation(ops))
            .map(drop)
            .map_err(|e| e.to_string())
    }
}

impl Drop for DataRootModels {
    fn drop(&mut self) {
        self.handle.shutdown();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
