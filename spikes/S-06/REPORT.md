# S-06: 純 Rust の TLS (更新の確認と取得)

| 項目 | 内容 |
|---|---|
| 仕様 | 08 §3 の S-06、10 の OQ-05、12 §2・§7.5、02 §4.1・§4.3 (R3・R5) |
| 合格の条件 | 3 つの対象 (Windows x86_64 (MSVC)・Linux x86_64 (musl)・macOS aarch64) で GitHub の Releases から取得でき、`cargo xtask check-deps` が通る |
| 実施 | 2026-11-17 (Windows・Linux)。macOS は作れることだけを確かめ、実機での接続は未実施 |
| 判定 | Windows と Linux は合格。macOS の実行は指示により省いた (v0.6 の macOS 版のときに確かめる。§5) |
| 再現 | `spikes/S-06/ureq` (本命)、`spikes/S-06/reqwest` (比較)、`spikes/S-06/unify` (機能の混ざり方)。どれも evorag の workspace から切り離した、独立の workspace。コードは `spike/S-06` ブランチにだけ置く (11 §9.1) |

## 1. 組んだもの

- **ureq 版** (`s06-ureq`): rustls 0.23.46 (`default-features = false`、`std`・`tls12`・`logging`) + rustls-graviola 0.4.0 (graviola 0.4.5) + ureq 3.5.2 (`rustls-no-provider`・`platform-verifier`・`json`)。暗号の実装は `TlsConfig::unversioned_rustls_crypto_provider` で graviola を渡す。
- 証明書のルートは、引数で 3 通りを切り替える。
  - `webpki`: 同梱の一覧 (webpki-root-certs 1.0.12、112 件)
  - `native`: OS の証明書ストアを読む (rustls-native-certs 0.8.5)
  - `platform`: OS の検証器に任せる (rustls-platform-verifier 0.7.1)
- 手順:
  1. CPU の機能を確かめる (§4)。
  2. `https://api.github.com/repos/evoref/evorag/releases/latest` を読む。
  3. 最も小さい資産 (`SHA256SUMS`) を取得する。取得は `objects.githubusercontent.com` への転送を含む。
- **reqwest 版** (`s06-reqwest`): reqwest 0.13.5 (`rustls-no-provider`・`blocking`・`json`) + rustls-graviola + rustls-platform-verifier。`tls_backend_preconfigured` で rustls の設定を渡す。比較のためだけに作った。

## 2. 結果

### 2.1 接続

| 対象 | 作り方 | webpki | native | platform |
|---|---|---|---|---|
| Windows x86_64 (MSVC) | 手元 (Windows 11) | 可 | 可 (ストアの 54 件) | 可 |
| Linux x86_64 (musl) | WSL の Ubuntu で `--target x86_64-unknown-linux-musl`。static-pie の実行ファイル | 可 | 可 (ストアの 120 件) | 可 |
| macOS aarch64 | `cargo check --target aarch64-apple-darwin` (graviola の aarch64 のコードを含めて通る) | 未実施 | 未実施 | 未実施 |

- どの組み合わせも、API の応答 (タグ `v0.2.0`、公開日) を読めた。資産の取得 (218 バイト。大きさは API の値と一致) もできた。
- reqwest 版も Windows で API を読めた。

### 2.2 C を含む依存と数

`cargo xtask check-deps --manifest-path <spike>/Cargo.toml` の結果 (`deps-policy.toml` の 4 つの対象):

| 版 | 結果 | 推移的な依存の数 |
|---|---|---|
| ureq 版 | ok | Windows 32、Linux (gnu・musl・aarch64 musl) 31 |
| reqwest 版 | ok | 4 つの対象とも 89 |

- **ring・aws-lc-rs・cc は、ビルドの依存の木に入らない** (`cargo tree -e all -i ring` が空)。対象は Windows・Linux musl・macOS aarch64。
  - Cargo.lock には ring と cc が載る (rustls-webpki などの任意の依存)。ビルドされず、check-deps の判定 (`cargo tree`) にも出ない。evorag の今の Cargo.lock にも、ビルドされない cc がすでに載っている。
- **macOS aarch64 の依存の木** (`cargo tree --target aarch64-apple-darwin`):
  - 増えるのは core-foundation・core-foundation-sys・security-framework・security-framework-sys。OS のフレームワークを呼ぶだけで、C を作らない。
  - 禁止の crate は入らない。
  - `deps-policy.toml` の対象には macOS が無いので、check-deps はこの対象を検査しない (§6)。
- **evorag の今の依存の木に対して増える数** (Windows): ureq 版も reqwest 版も 16。
  - reqwest 版で数が増えないのは、reqwest・hyper・tokio をすでに持っているため。
  - ureq 版の 16 は、同梱の一覧と OS のストアの両方を試すための分 (webpki-root-certs・rustls-native-certs) を含む。

### 2.3 ライセンス

リポジトリの `deny.toml` で `cargo deny check licenses bans` を流した:

- 許可リストの外は **webpki-root-certs (CDLA-Permissive-2.0) だけ**。spike 自身の license の欄が無いことの指摘は除く。webpki-roots も同じライセンス。
- rustls-native-certs・rustls-platform-verifier と、その依存 (schannel・security-framework など) は許可リストで通る。
- graviola は Apache-2.0 OR ISC OR MIT-0、rustls-graviola は Apache-2.0 OR ISC OR MIT で、許可リストで通る。

### 2.4 reqwest にすると evorag-llm に TLS が入る

`spikes/S-06/unify` で確かめた。workspace の中に 3 つの crate を置いた。

| crate | 何にあたるか | reqwest の機能 |
|---|---|---|
| `llm` | evorag-llm | `json` |
| `update` | evorag-update | `rustls-no-provider`・`blocking` |
| `app` | 配布物の evorag | 2 つに依存する |

- `cargo tree -p llm -i rustls` は空。check-deps の `tree_only_in` は、この見え方で判定する。
- `cargo tree -p app -e features -i reqwest` では、`rustls-no-provider`・`__rustls`・`__tls`・`blocking` が有効になる。
- 配布物の中の reqwest は 1 つなので、**evorag-llm の reqwest にも TLS が入る**。02 §4.3 の R3 (「`evorag-llm` は TLS を持たない」) に、実質で反する。
- さらに、evorag-update が graviola を既定の提供者として入れると、llama-server の URL に `https://` を書けば繋がってしまう。
- check-deps はこれを見つけられない。

ureq なら、TLS は ureq の中に閉じ、evorag-llm の reqwest は今のまま (TLS なし) で残る。

## 3. 証明書のルート

| 方法 | 長所 | 短所 |
|---|---|---|
| 同梱の一覧 (webpki-roots / webpki-root-certs) | OS に依らず同じ。Linux の最小の環境でも動く | ライセンス (CDLA-Permissive-2.0) が許可リストの外。一覧の更新に evorag の更新が要る |
| OS のストアを読む (rustls-native-certs) | ライセンスが通る。OS の更新で一覧が新しくなる | 検証は webpki が行い、OS の方針 (失効の確認・企業のルート・制限) は使わない |
| OS の検証器 (rustls-platform-verifier) | ライセンスが通る。Windows (CryptoAPI) と macOS (Security.framework) は OS の方針で検証する。rustls の推す方法。ureq の機能 (`platform-verifier`) で使える | Linux は OS のストアを読んで webpki で検証する。`ca-certificates` の無い環境では繋がらない |

## 4. CPU の機能

graviola 0.4.5 の `low/*/cpu.rs` の `is_cpu_supported` で確かめた。

| CPU | 必須の機能 |
|---|---|
| x86_64 | aes・pclmulqdq・bmi1・adx・avx・avx2 |
| aarch64 | neon・aes・pmull・sha2 |

- 足りないと、graviola は最初の暗号の処理で `assert!` により **panic する**。evorag の側で通信の前に確かめることが要る (12 §7.5 のとおり)。
- `is_cpu_supported` は公開されていない。evorag は `is_x86_feature_detected!` / `is_aarch64_feature_detected!` で同じ一覧を確かめる。spike の `cpu_ok` がその形。
- OQ-05・12 §7.5 の表 (AES-NI・SSSE3・AVX・AVX2・ADX・BMI2・PCLMULQDQ) とは違う。違いは 2 つ:
  - bmi2 は必須ではない。
  - 表に無い bmi1 が必須。
  - ssse3 は avx に含まれる。
- graviola の版を上げるときは、この一覧を確かめ直す。

## 5. 判定と残り

- **Windows x86_64 (MSVC)**: 合格。
- **Linux x86_64 (musl)**: 合格。
- **macOS aarch64**: 作れることだけを確かめた (`cargo check`)。実機か、GitHub Actions の macOS のランナーでの接続が残る。
- OQ-05 の採る実装 (rustls + rustls-graviola、C を含む依存の例外を作らない) を見直す理由は、Windows と Linux では見つからなかった。

## 6. 仕様への反映の案 (spec/ ブランチで出す)

1. **HTTP のクライアント**:
   - evorag-update は **ureq 3** (`rustls-no-provider`・`platform-verifier`) を使う。reqwest は evorag-llm だけ。
   - 理由は §2.4 (TLS が evorag-llm に混ざる) と、依存の閉じ方。
   - 直す場所: 02 §4.1 の表と、`deps-policy.toml` の R3 (`direct_only_in`)。
   - `tree_only_in` の TLS の crate の一覧に `ureq` の TLS の部分は入らないので、`reqwest` の機能の混ざり方を検査する方法も要る。例: check-deps で、`evorag` の木の reqwest に `__rustls` が無いことを見る。
2. **証明書のルート**:
   - rustls-platform-verifier (OS の検証器) を使い、同梱の一覧は使わない (ライセンス)。
   - Linux でルートが見つからないときは、通信せずに理由を示し、手作業の更新 (12 §7.1) を案内する。
   - 直す場所: 12 §2 の「S-06 で決める」と、OQ-05 の表の該当の行。
3. **CPU の機能**: 12 §7.5 と OQ-05 の表を、graviola 0.4.5 の一覧 (§4) に直す。
4. **検査の対象**: `deps-policy.toml` と `deny.toml` の対象に `aarch64-apple-darwin` を足すかを決める (配布の対象 12 §5.1 にあるのに、今は検査していない)。
5. **macOS の実行** (§5) が済むまで、OQ-05 は「条件付き確定」のまま置く。
