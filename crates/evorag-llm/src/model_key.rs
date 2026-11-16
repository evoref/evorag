//! model_key (06 §5、FR-508)。v0.1 で定義を固める。
//!
//! `model_path` のファイル名と、`data[0].meta` の `n_params`・`size`・`n_vocab` を改行で区切って
//! 連結し (数は 10 進)、SHA-256 の先頭 16 バイトを小文字の 16 進で書く。meta の値が欠けるときは、
//! 3 つの値の代わりに `data[0].id` を使う。

use sha2::{Digest, Sha256};

use crate::types::ModelMeta;

/// `model_path` のファイル名 (`/` と `\` のどちらも区切りとみなす)。
pub fn file_name(model_path: &str) -> &str {
    model_path.rsplit(['/', '\\']).next().unwrap_or(model_path)
}

pub fn model_key(model_path: &str, meta: &ModelMeta, model_name: &str) -> String {
    let mut text = file_name(model_path).to_owned();
    match (meta.n_params, meta.size, meta.n_vocab) {
        (Some(p), Some(s), Some(v)) => {
            for n in [p, s, v] {
                text.push('\n');
                text.push_str(&n.to_string());
            }
        }
        _ => {
            text.push('\n');
            text.push_str(model_name);
        }
    }
    let digest = Sha256::digest(text.as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> ModelMeta {
        ModelMeta {
            n_params: Some(8_953_000_000),
            size: Some(5_680_000_000),
            n_vocab: Some(248_320),
            n_ctx_train: Some(262_144),
        }
    }

    /// 定義を固定する (値が変わったら、model_key の定義が変わったということ。03 §4、06 §5)。
    #[test]
    fn known_value_is_stable() {
        let k = model_key("/models/Qwen3.5-9B-Q4_K_M.gguf", &meta(), "qwen");
        let expected = {
            let d = Sha256::digest(b"Qwen3.5-9B-Q4_K_M.gguf\n8953000000\n5680000000\n248320");
            d[..16]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        assert_eq!(k, expected);
        assert_eq!(k.len(), 32);
        assert!(
            k.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
    }

    #[test]
    fn folders_are_ignored_on_both_separators() {
        assert_eq!(
            model_key("C:\\models\\a.gguf", &meta(), "x"),
            model_key("/srv/a.gguf", &meta(), "y")
        );
    }

    #[test]
    fn falls_back_to_the_model_name_without_meta() {
        let none = ModelMeta::default();
        let a = model_key("a.gguf", &none, "name-a");
        let b = model_key("a.gguf", &none, "name-b");
        assert_ne!(a, b);
        let d = Sha256::digest(b"a.gguf\nname-a");
        assert_eq!(
            a,
            d[..16]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        // 値が 1 つでも欠ければ名前を使う。
        let partial = ModelMeta {
            n_vocab: None,
            ..meta()
        };
        assert_eq!(model_key("a.gguf", &partial, "name-a"), a);
    }
}
