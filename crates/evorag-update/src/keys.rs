//! リリースの署名の鍵と、署名の確かめ (12 §5.2、07 §4.2 の `update.trusted_keys`)。

use std::fmt;

use crate::UpdateError;
use crate::base64;

include!(concat!(env!("OUT_DIR"), "/embedded_keys.rs"));

/// minisign の公開鍵の復号した長さ (方式 2 + 鍵の ID 8 + 鍵 32)。
const PUBLIC_KEY_BYTES: usize = 42;
/// minisign の署名の 2 行目の復号した長さ (方式 2 + 鍵の ID 8 + 署名 64)。
const SIGNATURE_BYTES: usize = 74;
/// Ed25519 の方式の印。公開鍵と legacy の署名は `Ed`、prehash の署名は `ED`。
const ALG_ED25519: [u8; 2] = *b"Ed";
const ALG_PREHASHED: [u8; 2] = *b"ED";

/// minisign の鍵の ID。minisign と同じく、8 バイトをリトルエンディアンの数として大文字の 16 進
/// 16 桁で示す (`minisign.pub` の 1 行目の `minisign public key <ID>`)。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyId([u8; 8]);

impl KeyId {
    fn from_slice(bytes: &[u8]) -> Self {
        let mut id = [0u8; 8];
        id.copy_from_slice(bytes);
        Self(id)
    }
}

impl fmt::Display for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016X}", u64::from_le_bytes(self.0))
    }
}

impl fmt::Debug for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyId({self})")
    }
}

/// 鍵の出どころ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeySource {
    /// 本体に埋め込んだ鍵 (`minisign.pub`・`minisign.next.pub`)。
    Embedded,
    /// 設定 `update.trusted_keys` などで足した鍵。この鍵で通ったときは、鍵の ID を示して警告する
    /// (12 §5.2)。
    Added,
}

/// 署名が通った鍵。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Signer {
    key_id: KeyId,
    source: KeySource,
}

impl Signer {
    /// 鍵の ID。
    #[must_use]
    pub const fn key_id(&self) -> KeyId {
        self.key_id
    }

    /// 鍵の出どころ。
    #[must_use]
    pub const fn source(&self) -> KeySource {
        self.source
    }
}

/// `update.trusted_keys` の値が鍵として読めない (07 §4.2。設定の誤り、終了コード 4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum KeyFormatError {
    /// base64 として読めない。
    #[error("the key is not base64")]
    NotBase64,
    /// 復号した長さが 42 バイトでない。
    #[error("the key decodes to {len} bytes, not 42")]
    WrongLength {
        /// 復号した長さ。
        len: usize,
    },
    /// 署名の方式が Ed25519 (`Ed`) でない。
    #[error("the key is not a minisign Ed25519 key")]
    NotEd25519,
}

/// 署名を確かめる鍵 1 本。
#[derive(Clone, Debug)]
pub struct TrustedKey {
    id: KeyId,
    source: KeySource,
    key: minisign_verify::PublicKey,
}

impl TrustedKey {
    /// 設定 `update.trusted_keys` の値 1 つ (minisign の公開鍵の base64 の行) を読む。前後の空白も
    /// 許さない。
    ///
    /// # Errors
    ///
    /// base64 として読めない・復号した長さが 42 バイトでない・方式が `Ed` でないとき (07 §4.2)。
    pub fn from_config(value: &str) -> Result<Self, KeyFormatError> {
        Self::from_base64(value, KeySource::Added)
    }

    /// `minisign.pub` の形 (1 行目が untrusted comment、2 行目が base64) の鍵を、足す鍵として読む。
    ///
    /// # Errors
    ///
    /// 2 行目が [`TrustedKey::from_config`] の条件に合わないとき。
    pub fn from_minisign_pub(text: &str) -> Result<Self, KeyFormatError> {
        let line = text.lines().nth(1).ok_or(KeyFormatError::NotBase64)?;
        Self::from_base64(line.trim(), KeySource::Added)
    }

    pub(crate) fn from_base64(value: &str, source: KeySource) -> Result<Self, KeyFormatError> {
        let bin = base64::decode(value).ok_or(KeyFormatError::NotBase64)?;
        if bin.len() != PUBLIC_KEY_BYTES {
            return Err(KeyFormatError::WrongLength { len: bin.len() });
        }
        if bin[..2] != ALG_ED25519 {
            return Err(KeyFormatError::NotEd25519);
        }
        let key = minisign_verify::PublicKey::from_base64(value)
            .map_err(|_| KeyFormatError::NotBase64)?;
        Ok(Self {
            id: KeyId::from_slice(&bin[2..10]),
            source,
            key,
        })
    }

    /// 鍵の ID。
    #[must_use]
    pub const fn id(&self) -> KeyId {
        self.id
    }

    /// 鍵の出どころ。
    #[must_use]
    pub const fn source(&self) -> KeySource {
        self.source
    }
}

/// 署名が通った結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedSignature {
    signer: Signer,
    trusted_comment: String,
}

impl VerifiedSignature {
    /// 署名が通った鍵。
    #[must_use]
    pub const fn signer(&self) -> Signer {
        self.signer
    }

    /// 全体の署名 (global signature) で守られた trusted comment。
    #[must_use]
    pub fn trusted_comment(&self) -> &str {
        &self.trusted_comment
    }
}

/// 署名を確かめる鍵の集まり。埋め込んだ鍵に、設定の鍵を足して使う (置き換えない)。
#[derive(Clone, Debug, Default)]
pub struct Keyring {
    keys: Vec<TrustedKey>,
}

impl Keyring {
    /// 鍵の無い集まり。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 本体に埋め込んだ鍵 (今の鍵と、置かれていれば次の鍵)。
    ///
    /// # Panics
    ///
    /// 埋め込んだ鍵が読めないとき (build.rs が形を確かめ、試験が読めることを確かめる)。
    #[must_use]
    pub fn embedded() -> Self {
        let keys = EMBEDDED_KEYS
            .iter()
            .map(|(file, line)| {
                TrustedKey::from_base64(line, KeySource::Embedded)
                    .unwrap_or_else(|e| panic!("the embedded key {file} is broken: {e}"))
            })
            .collect();
        Self { keys }
    }

    /// 鍵を足す。
    pub fn push(&mut self, key: TrustedKey) {
        self.keys.push(key);
    }

    /// 鍵を順に返す。
    pub fn iter(&self) -> impl Iterator<Item = &TrustedKey> {
        self.keys.iter()
    }

    /// 鍵の数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// 鍵が無いか。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// `data` の minisign の署名 (`.minisig` のバイト列) を確かめる (12 §5.2)。
    ///
    /// prehash でない署名は鍵を試さずに断る。署名の鍵の ID と同じ ID の鍵を、埋め込んだ鍵から順に
    /// すべて試し、最初に通った鍵を返す。全体の署名 (trusted comment を守る) も確かめる。
    ///
    /// # Errors
    ///
    /// 署名の形でない、legacy の署名、同じ ID の鍵が無い、どの鍵でも通らない、のどれか
    /// (どれも改ざんを疑う理由)。
    pub fn verify(&self, data: &[u8], minisig: &[u8]) -> Result<VerifiedSignature, UpdateError> {
        let text = std::str::from_utf8(minisig).map_err(|_| UpdateError::SignatureMalformed)?;
        let sig = minisign_verify::Signature::decode(text)
            .map_err(|_| UpdateError::SignatureMalformed)?;
        // 方式と鍵の ID は minisign-verify が示さないので、2 行目を自分で読む。
        let bin = text
            .lines()
            .nth(1)
            .and_then(base64::decode)
            .filter(|b| b.len() == SIGNATURE_BYTES)
            .ok_or(UpdateError::SignatureMalformed)?;
        if bin[..2] != ALG_PREHASHED {
            return Err(UpdateError::LegacySignature);
        }
        let key_id = KeyId::from_slice(&bin[2..10]);
        let mut candidates = self.keys.iter().filter(|k| k.id == key_id).peekable();
        if candidates.peek().is_none() {
            return Err(UpdateError::UnknownKey { key_id });
        }
        candidates
            .find(|k| k.key.verify(data, &sig, false).is_ok())
            .map(|k| VerifiedSignature {
                signer: Signer {
                    key_id,
                    source: k.source,
                },
                trusted_comment: sig.trusted_comment().to_owned(),
            })
            .ok_or(UpdateError::BadSignature { key_id })
    }
}

impl Extend<TrustedKey> for Keyring {
    fn extend<I: IntoIterator<Item = TrustedKey>>(&mut self, iter: I) {
        self.keys.extend(iter);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_keys_are_read_and_the_current_key_matches_its_comment() {
        let ring = Keyring::embedded();
        assert!(!ring.is_empty());
        assert!(ring.iter().all(|k| k.source() == KeySource::Embedded));
        // minisign.pub の 1 行目の ID と、2 行目から読んだ ID が同じ。
        let text =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../minisign.pub"))
                .expect("minisign.pub is at the repository root");
        let comment_id = text
            .lines()
            .next()
            .and_then(|l| l.rsplit(' ').next())
            .expect("the first line names the key id");
        assert_eq!(
            ring.iter().next().map(|k| k.id().to_string()).as_deref(),
            Some(comment_id)
        );
        let next = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../minisign.next.pub"
        ));
        assert_eq!(ring.len(), if next.exists() { 2 } else { 1 });
    }

    #[test]
    fn config_keys_must_be_ed25519_minisign_keys() {
        // minisign-verify の試験の公開鍵。
        let good = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
        let key = TrustedKey::from_config(good).expect("a minisign public key");
        assert_eq!(key.source(), KeySource::Added);
        assert_eq!(key.id().to_string(), "E7620F1842B4E81F");
        let err = |v: &str| TrustedKey::from_config(v).err();
        assert_eq!(err("not base64!"), Some(KeyFormatError::NotBase64));
        assert_eq!(err(&format!(" {good}")), Some(KeyFormatError::NotBase64));
        assert_eq!(err(&format!("{good}\n")), Some(KeyFormatError::NotBase64));
        assert_eq!(err("TWFu"), Some(KeyFormatError::WrongLength { len: 3 }));
        // 方式が `ED` (prehash の署名の印) の鍵は、公開鍵としては断る。
        assert_eq!(
            err(&good.replacen("RWQ", "RUQ", 1)),
            Some(KeyFormatError::NotEd25519)
        );
    }

    #[test]
    fn minisign_pub_files_are_read_from_the_second_line() {
        let text = "untrusted comment: minisign public key E7620F1842B4E81F\r\n\
                    RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3\r\n";
        let key = TrustedKey::from_minisign_pub(text).expect("a minisign.pub file");
        assert_eq!(key.id().to_string(), "E7620F1842B4E81F");
        assert!(TrustedKey::from_minisign_pub("one line").is_err());
    }
}
