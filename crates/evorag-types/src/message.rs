//! 文言キー (`MessageKey`)。コアは利用者に見せる文言を文言キーと引数で返し、画面がカタログで訳す
//! (02 §3 の決めごと 1、07 §6)。
//!
//! キーは `message_keys!` の中だけで定義する。`cargo xtask i18n` が `MessageKey::ALL` と
//! `locales/<言語>.toml` を突き合わせ、差分があれば失敗にする (NFR-602)。
//! 値は足すだけにし、既存のキーの文字列の意味を変えない (02 §3 の決めごと 3)。

macro_rules! message_keys {
    ($($(#[$meta:meta])* $variant:ident => $key:literal,)*) => {
        /// 利用者に見せる文言のキー。文字列は `locales/<言語>.toml` の点でつないだ名前。
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[non_exhaustive]
        pub enum MessageKey {
            $($(#[$meta])* $variant,)*
        }

        impl MessageKey {
            /// 定義したすべてのキー (定義の順)。
            pub const ALL: &'static [MessageKey] = &[$(MessageKey::$variant,)*];

            /// カタログでの名前。
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(MessageKey::$variant => $key,)*
                }
            }
        }
    };
}

message_keys! {}
