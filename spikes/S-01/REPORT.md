# S-01 redb の耐久性 (Windows) — 検証の報告

08 §3 の S-01 の行の検証。仕様の該当箇所は 03 §3.4・§6・§7.1・§8・§9、09 §4。

## 結論

| 基準 (08 §3) | 結果 | 合否 |
|---|---|---|
| 1,000 回の強制終了でコミット済みの記録を失わない | NTFS (内蔵 NVMe) と exFAT (USB SSD) の両方で 1,000 回ずつ、失った記録・書きかけ・開けない DB は 0 件 (quick_repair を有効にした 1,000 回も 0 件) | **合格** |
| 10 万件の論理的な写しが 30 秒以内 | 書き手がコミットを続けている間で 7.0〜11.8 秒 (書き手なしで 1.2〜1.4 秒)。写しの件数・中身は写し始めのスナップショットと一致 | **合格** |

加えて、`StorageBackend` の差し替えによる電源断の模擬を実装し、1,000 回 × 2 通り (1 相・2 相のコミット) で失った記録は 0 件。09 §4 の方法は実現できる。

## 1. 検証の環境

| 項目 | 値 |
|---|---|
| OS | Windows 11 Pro 10.0.26300 (x86_64) |
| Rust | rustc 1.99.0 (b940084d7 2026-09-28)、cargo 1.99.0、`stable-x86_64-pc-windows-msvc` |
| redb | 4.5.0 (crates.io の最新。2026-11-05 公開。MSRV 1.90)。推移的依存は `libc` 0.2.191 だけ |
| 置き場所 | C: (NTFS、内蔵 NVMe の SSD) と E: (exFAT、USB 接続の SSD) |
| ビルド | `cargo build --release` |

コードは `src/` (サブコマンドごとに `crash.rs`・`lockt.rs`・`copy.rs`・`sim.rs`)。再現の手順は §10。

## 2. Durability の種類の名前と意味 (検証の項目 1)

redb 4.5.0 のソース (`src/transactions.rs`) と CHANGELOG で確かめた。

| 名前 | 意味 (doc コメントの要旨) |
|---|---|
| `Durability::Immediate` | `commit()` が返った時点で永続する。**`WriteTransaction` の既定**。 |
| `Durability::None` | 後に `Immediate` のコミットが続くまで、ディスクに永続しない。 |

- 種類はこの 2 つだけ。`Durability::Paranoid` は 3.0.0 で削除された (2.x で非推奨、代わりは `WriteTransaction::set_two_phase_commit(true)`)。`Eventual` も無い。
- `set_durability()` は `Result` を返す (3.0.0 から)。永続のセーブポイントを作った・消したトランザクションでは `Immediate` より下げられない。`ConcurrencyMode::SingleWriter` / `MultiWriter` では `None` が `SetDurabilityError::NonDurableCommitUnsupported` で拒否される。既定の `ExclusiveWriter` では使える。
- 耐久性とは別に、コミットごとに次の 2 つを選べる。
  - `set_two_phase_commit(bool)` (既定 off): 1 相のコミットは「予備のスロットを書く → god byte を切り替える → fsync」。2 相は「書く → fsync → 切り替える → fsync」。1 相でもチェックサムで最新の有効なスロットを選ぶ。2 相は、悪意のある入力と書き出しの順序を操れる攻撃者への対策 (doc の Security considerations)。
  - `set_quick_repair(bool)` (既定 off): コミットのたびに割り当ての状態を保存し (2 相も有効になる)、異常終了の後の修復をほぼ一瞬にする。off だと、異常終了の後に開くとき DB 全体を走査して修復する。
- **03 §3.4 の名前は合っている**: 会話の追記は `Durability::Immediate` (既定のまま)、裏の処理は `Durability::None`。
- **`None` の注意 (実測)**: `None` のコミットはプロセスのメモリにだけあり、**プロセスの kill でも失われる** (電源断だけではない)。電源断の模擬の対照実験 (§5) で、会話の追記を `None` にすると、書き切り前の書き込みを「すべて残す」(= kill と同じ) 場合も含めて 50 回中 50 回で記録を失った (`None` だけのコミットではバックエンドへの書き込みが起きなかった)。
- Windows の書き切り: redb の `FileBackend::sync_data` は `File::sync_data` を呼び、std (1.99) の Windows 版は `FlushFileBuffers` を呼ぶ (`library/std/src/sys/fs/windows.rs`)。03 §6 の「Windows は `FlushFileBuffers`」と一致する。

### コミット 1 件 (1 KB の値) の時間 (E:、500 回)

| 設定 | 中央 | p99 | 最大 |
|---|---|---|---|
| `Immediate` | 1.08 ms | 1.95 ms | 2.20 ms |
| `Immediate` + 2 相 | 1.82 ms | 2.65 ms | 4.19 ms |
| `Immediate` + quick_repair | 2.42 ms | 3.80 ms | 7.91 ms |
| `None` | 13.5 µs | 25.2 µs | 93.3 µs |

C: (NTFS) の `Immediate` は中央 1.58 ms、p99 3.06 ms。

## 3. 強制終了の試験 (検証の項目 2)

### 手順 (`crash <dir> 1000 [qr]`)

1. 子プロセス (`child`) が DB を開き、`meta.last` の次の連番から、1 件ずつ 1 トランザクションで書く。1 トランザクションで `turns` (連番 → 64〜1,087 バイトの値)・`turn_sum` (連番 → 値のハッシュ)・`meta` (`last` → 連番) の 3 つの表に書き、`Durability::Immediate` (既定) でコミットする。`commit()` が返ってから、連番を標準出力に出して flush する。
2. 親は子を起動し、乱数の時間 (3 割は 0〜60 ms で起動と修復の最中、7 割は 40〜440 ms でコミットの連続の最中) の後に `Child::kill` (Windows では `TerminateProcess`) で止める。改行で終わった行だけを「子が報告した連番」として採る。
3. 親が `Database::open` で開き直し (修復を含む)、次を確かめる: `turns` の鍵が 1 から連続、値が連番から決まるものと一致、`turn_sum` の件数とハッシュが一致、`meta.last` = 件数 (= 書きかけが無い)。最後の連番 ≥ 子が報告した最後の連番、かつ前回の確認の値以上 (= コミット済みを失わない)。
4. 1,000 回繰り返す。DB は 1 つを使い続ける (回ごとに大きくなる)。

### 結果

| 項目 | C: (NTFS) | E: (exFAT) | E: + quick_repair |
|---|---|---|---|
| 失った・不整合・開けない | **0 / 1,000** | **0 / 1,000** | **0 / 1,000** |
| 最後の連番 (コミットの合計) | 89,074 | 112,537 | 67,262 |
| 1 回あたりのコミット数 (中央 / 最大) | 76 / 254 | 109 / 311 | 65 / 182 |
| 報告の前に kill された回 (起動・修復の最中) | 56 | 40 | 67 |
| DB の最後 > 報告の最後 (コミット済みで、出力の前に kill) | 900 | 903 | 263 |
| kill の後の開き直し (修復を含む): 平均 / 中央 / p99 / 最大 | 45.9 / 45.6 / 104.6 / 208.7 ms | 52.9 / 47.8 / 128.2 / 286.5 ms | 3.8 / 3.6 / 11.3 / 110.9 ms |
| 最後の DB のサイズ | 60.4 MB | 84.5 MB | 49.6 MB |
| 全体の時間 | 378 s | 421 s | 319 s |

- quick_repair を有効にすると、kill の後の開き直しが中央 48 ms → 3.6 ms に縮む (DB 全体の走査が要らない)。代わりにコミットが約 2.2 倍遅くなる (§2)。
- 「DB の最後 > 報告の最後」が 9 割あるのは、コミットの時間の大半が `FlushFileBuffers` の中で、そこで kill されると「OS のキャッシュには入ったが `commit()` は返っていない」ためと考えられる。報告していないコミットが残ることは基準に反しない (ジャーナルとの突き合わせは冪等の取り込みで扱う、03 §6)。

### 限界

- **プロセスの kill は電源断の模擬ではない**。`TerminateProcess` の後も、書き込み済みのデータは OS のキャッシュに残り、いずれディスクへ出る。この試験で分かるのは「redb のコミットのどの時点でプロセスが死んでも、開き直せて、返ったコミットが残り、書きかけが見えない」ことまで。`FlushFileBuffers` が本当にディスク (とドライブのキャッシュ) まで書き切るか、書き切り前の書き込みが失われたり順序が入れ替わったりしたときに redb が正しく戻るかは分からない。
- 前者 (ドライブが書き切りを守るか) は、実機の電源を抜く試験でしか確かめられない。後者は §5 の模擬で確かめた。
- 子は「1 件ずつコミット」だけを試した。退避・compact・移行の最中の kill は試していない (09 §4 の「退避の作成中の強制終了」は別に要る)。

## 4. 別のプロセスが同じファイルを開くとき (検証の項目 3)

### redb の振る舞い (`lock <dir>`)

| 状況 | 開く側 | 結果 |
|---|---|---|
| 親が `Database::create` で開いている | 子の `Database::open` / `Database::create` / `ReadOnlyDatabase::open` / `Builder` (`SingleWriter`) の `open` | どれも `Err(DatabaseError::DatabaseAlreadyOpen)` (Display: "Database already open. Cannot acquire lock.") |
| 同じプロセスの中で 2 回目 | `Database::open` / `ReadOnlyDatabase::open` | `Err(DatabaseAlreadyOpen)` |
| 親が `ReadOnlyDatabase` で開いている | 子の `ReadOnlyDatabase::open` | Ok (読み取り同士は共有できる) |
| 同上 | 子の `Database::open` / `create` / `SingleWriter` | `Err(DatabaseAlreadyOpen)` |
| 親が閉じた後 | 子のすべて | Ok |
| DB を開いていた子を kill した直後 | 親の `Database::open` | 1 回目で Ok (待ち不要) |

- redb 4.5 の既定 `ConcurrencyMode::ExclusiveWriter` は、ファイル全体を std の `File::try_lock` / `try_lock_shared` (Windows では `LockFileEx`) でロックする。redb 自身が二重に開くことを防ぐ。
- 4.3 から `ConcurrencyMode::SingleWriter` (書き手 1 + 別のプロセスの読み取り) と `MultiWriter` がある (バイト範囲のロック。Windows・Linux・macOS で動く)。`SingleWriter` では `Durability::None` が使えない (§2)。

### std の `File::try_lock` (Rust 1.99)

- `File::lock` / `lock_shared` / `try_lock` / `try_lock_shared` / `unlock` は **1.89.0 で安定化** (`#[stable(feature = "file_lock", since = "1.89.0")]`)。1.99 の安定版で feature なしに使えることをビルドで確かめた。
- Windows の実装は `LockFileEx` (排他は `LOCKFILE_EXCLUSIVE_LOCK`、try は `LOCKFILE_FAIL_IMMEDIATELY`) で、範囲はファイル全体 (オフセット 0、長さ `u32::MAX`×2)。

| 状況 | 結果 |
|---|---|
| 別のプロセスがロックを持っているときの `try_lock` / `try_lock_shared` | `Err(TryLockError::WouldBlock)` |
| 同じプロセスの別のハンドルで `try_lock` | `Err(WouldBlock)` (ロックはハンドルごと) |
| ロックを持つ子を kill した後の `try_lock` | 待ち 0 ms で Ok (OS が解放する) |
| **ロックを持たれたファイルを別のプロセスが読む** (`std::fs::read`) | **`Err(os error 33, ERROR_LOCK_VIOLATION)`**「別のプロセスがファイルの一部をロックしています」 |

最後の行が仕様に効く: Windows の `LockFileEx` は**強制ロック**で、ロックした範囲を他のプロセスは読めない。03 §9 の「動いている evorag の PID が分かれば示す」を、ロックのファイルの中身に PID を書いて実現しようとすると、Windows では読めない (§8 の改訂案)。

## 5. 電源断の模擬 (09 §4 の方法の確認)

### 方法 (`powerloss <回数> [2pc|none]`、`src/sim.rs`)

`StorageBackend` (必須は `len`・`read`・`set_len`・`sync_data`・`write`。ロックの関数は既定で `Unsupported` を返し、そのときは `ExclusiveWriter` だけが使える) を自前で実装し、`Builder::create_with_backend` で開く。

1. `write` / `set_len` は「揮発の像」(OS のキャッシュに相当) にだけ当て、未確定の操作として記録する。`sync_data` が呼ばれたら「永続の像」へ移す。
2. 回ごとに、乱数で選んだ操作の数 (1〜300) の後に「電源断」を起こす。以後のすべての操作は失敗する。`commit()` が `Ok` を返したものを「確定済み」とする。
3. 永続の像に、未確定の操作を次のどれかで当てた像を作る: (a) すべて捨てる、(b) 4 KiB 単位でばらばらに半分を残す (順序の入れ替えと書きかけ)、(c) すべて残す (= kill)。
4. その像から開き直し (修復を含む)、§3 と同じ不変条件と「確定済みを失わない」を確かめる。10 回ごとに `check_integrity()` も呼ぶ。像を引き継いで次の回へ。
5. 書き込みの 3 分の 1 は裏の処理に相当する `Durability::None` のコミット (別の表) にする。

### 結果

| 設定 | 回数 | 失った・不整合 | 捨て方 (a / b / c) | 書き切り前に残っていた操作の平均 |
|---|---|---|---|---|
| 1 相のコミット (既定) | 1,000 | **0** | 331 / 355 / 314 | 7.2 |
| 2 相のコミット | 1,000 | **0** | 328 / 344 / 328 | 6.3 |
| 対照: 会話の追記を `None` で書く | 50 | **50** (検出できた) | 18 / 20 / 12 | 0.0 |

対照実験で失うことを検出できたので、模擬は「書き切っていないものを失う」場合を見分けられる。

### 模擬の限界

- 模擬しているのは「`sync_data` の前の書き込みが失われる・一部だけ残る・書きかけになる」ことだけ。ドライブが書き切りを守らない (嘘をつく) 場合、ファイルシステムのメタデータ (ファイルの長さ・rename) の失われ方、4 KiB より細かい書きかけは模擬していない。
- ジャーナル (03 §6) は redb の外のファイルなので、この差し替えでは試せない。`evorag-store` のジャーナルの書き込みにも、同じ形の注入の層 (書き切り前を捨てる) が要る。

## 6. 論理的な写し (検証の項目 4)

### 手順 (`copy <dir> 100000`)

1. 元の DB を作る: `record` (16 バイトの時刻順の鍵 → 1,024 バイトの値) 10 万件、`link` (32 バイトの鍵 → 1 バイト) 10 万件、`meta` 1 件。ファイルは 269 MB。
2. 書き手のスレッドが、`record` と `meta` に 1 件ずつ `Immediate` でコミットし続ける。
3. 200 ms 後に読み取りトランザクションを 1 つ開き、表ごとの件数を控える。`list_tables()` で表の名前を列挙し、すべての表を `&[u8] → &[u8]` として新しい redb のファイル (`backup.redb.tmp`) へ写す。最後のコミットは `Immediate` (書き切る)。閉じた後、書き込みの権限のあるハンドルで `sync_all` し、`backup.redb` へ `rename` する。
4. 写しを開き、表ごとの件数が控えと一致し、中身が読み取りトランザクションの中身と 1 件ずつ一致するかを確かめる。
5. 写し方を 3 通り (書き手なし、書き手ありで 1 トランザクション、書き手ありで 1 万件ごとに `None` でコミットし最後だけ `Immediate`) で、各 2〜3 回。

### 結果 (読み取りトランザクションを開いてから rename までの時間)

| 写し方 | C: (NTFS) | E: (exFAT) | 写しの間の書き手のコミット |
|---|---|---|---|
| 書き手なし・1 トランザクション | (未計測) | 1.43 s、1.18 s | 0 |
| 書き手あり・1 トランザクション | 8.42 s、9.73 s、10.51 s | 7.03 s、7.49 s、8.14 s | 約 4,500〜5,500 件 |
| 書き手あり・1 万件ごとにコミット | 9.98 s、10.98 s、11.82 s | 8.52 s、9.11 s、9.89 s | 約 4,900〜6,100 件 |

- **すべての回で、写しの件数と中身は写し始めのスナップショットと一致**した (書き手は写しの間も元の DB にコミットし続け、元の `record` は写しの間に約 5,000 件増えた)。
- 基準の 30 秒には余裕がある。ただし書き手が連続でコミットすると 6〜8 倍遅くなる (書き手の `FlushFileBuffers` とディスクを取り合うためと見られる。原因は切り分けていない)。実際の evorag の書き手はこれほど連続しないので、v0.1 の起動時・終了時の空き時間なら 1〜2 秒台の見込み。回を重ねるごとに遅くなるのは、元の DB が大きくなるため。
- 写しのファイルは元と同じ 269 MB (値 1 KB の記録はページ 4 KiB に 3 件ほどしか入らない)。退避の前の空きの確認 (03 §8) は「元の DB のファイルの大きさ」以上を見込む必要がある。
- 1 万件ごとにコミットしても速くはならなかった。メモリを抑える目的でだけ使う。

### 気づいた点

- **型を知らずに写すには、表の型が揃っている必要がある**。redb は表を開くときに型の名前を確かめるので、`TableDefinition<[u8;16], _>` で作った表を `&[u8]` で開くと `TableTypeMismatch` になる。03 §3.3 のとおり鍵を `evorag-store` が自前で符号化するなら、すべての表を `&[u8] → &[u8]` にすると、`list_tables()` / `list_multimap_tables()` だけで全表を写せる。
- **Windows では `File::open` (読み取り専用) で開いたファイルに `sync_all` すると `ERROR_ACCESS_DENIED` (os error 5) で失敗する**。`FlushFileBuffers` は書き込みの権限のあるハンドルを要する。最初の実装でこれに当たった。
- std の `fs::rename` は Windows では `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` を呼ぶ (失敗時は `FILE_RENAME_FLAG_POSIX_SEMANTICS` の `SetFileInformationByHandle`)。`MOVEFILE_WRITE_THROUGH` は付けないので、rename そのものの永続は保証されない。Windows にはフォルダの書き切り (Unix の `fsync(dir)`) に当たる手段が無い。

## 7. 空き不足と書き切りの失敗 (検証の項目 5)

実際のディスクを一杯にする試験はしていない。`StorageBackend` の差し替えで、`set_len` / `write` が上限を超えたら `io::ErrorKind::StorageFull` を返すようにして調べた (`diskfull`)。std は Windows の `ERROR_DISK_FULL` (112) と `ERROR_HANDLE_DISK_FULL` (39) を `StorageFull` に対応させる (`library/std/src/sys/io/error/windows.rs`) ので、実機でも同じ種類のエラーになると見込む。

| 段階 | 結果 |
|---|---|
| 空きが尽きたコミット | `Err(Error::Io(kind: StorageFull))`。それまでに確定した 2,366 件は残る |
| 続けて `begin_write` | `Err(TransactionError::Storage(StorageError::PreviousIo))` ("Previous I/O error occurred. Please close and re-open the database.")。以後、その `Database` では書けない |
| 続けて読み取り | Ok (確定した最後の連番を読める) |
| 空きが無いまま開き直す | 開ける。不変条件も満たす。追記は再び `StorageFull` |
| 空きを戻して開き直す | 開ける。追記できる |
| `sync_data` (書き切り) の失敗 | そのコミットは `Err(Io)`、次は `PreviousIo`。永続の像から開くと、最後に書き切れたコミットまでが残る (失敗したコミットは失われうる) |

- redb は I/O のエラーの後、`PreviousIo` で書き込みを止める。**開き直すまで回復しない**。03 §3.4 の「それ以上書き込まずに DB を開き直す」は redb の振る舞いと合う。
- 03 §8 の「予備のファイルを消して使う」は、予備を消した後に **DB を開き直してから** 書く手順にする必要がある (消すだけでは `PreviousIo` のまま)。

## 8. 仕様書に直すべき点

| 節 | 直す内容 |
|---|---|
| 03 §3.4 | 「`Durability::Immediate` 相当。名前と意味は S-01 で確かめる」→「`Durability::Immediate` (redb の既定)」と確定。裏の処理は「`Durability::None`」と名前を書く。`None` のコミットは**プロセスの異常終了でも失われる** (電源断だけでない) ことを明記する。裏の処理の進み具合 (job の状態) を `None` で書くなら、job は最初からやり直しても冪等であること、または進み具合を `Immediate` のコミットに含めることを書く。 |
| 03 §3.4 | redb の `ConcurrencyMode` は既定の `ExclusiveWriter` を使うと明記する (`SingleWriter` / `MultiWriter` では `Durability::None` が拒否される)。quick_repair をどうするかを決める: quick_repair なしの kill 後の開き直しは 60〜85 MB の DB で中央 46〜48 ms・最大 287 ms (DB の大きさに比例して伸びる)、ありで中央 3.6 ms。代わりにコミットが 1.08 → 2.42 ms。会話の追記は 1 ターンに数回なので、有効にするほうが NFR-101 (起動 500 ms) に安全と見る。決めるのは BM-101 で異常終了の後の起動を測ってから。 |
| 03 §3.4 | redb がエラーを返した後は `StorageError::PreviousIo` で書けなくなり、開き直すまで戻らないことを書く (今の「開き直す」の根拠)。 |
| 03 §6 (書き切る手順) | Windows の `FlushFileBuffers` は**書き込みの権限のあるハンドル**が要る (読み取り専用で開いたハンドルでは `ERROR_ACCESS_DENIED`) ことを書く。Windows の rename は `MoveFileExW(MOVEFILE_REPLACE_EXISTING` と `MOVEFILE_WRITE_THROUGH)` (または `ReplaceFileW`) を使う。std の `fs::rename` は `WRITE_THROUGH` を付けない。Windows にはフォルダの書き切りが無いことも書く。§6 の「削除と忘れる」の入れ替えと §7.1 の退避の rename の両方に効く。 |
| 03 §7.1 | 「すべての表を写す」を実現する前提として、**すべての表を `&[u8] → &[u8]` (鍵と値を `evorag-store` が符号化) にする**ことを 03 §3.1 / §3.3 に書く。型の付いた表があると、型を知らずに写せない (`TableTypeMismatch`)。写しは `list_tables()` と `list_multimap_tables()` で列挙する。退避の前の空きの確認は「元の DB のファイルの大きさ以上」と書く (値 1 KB の 10 万件で、写しも 269 MB)。 |
| 03 §8 (空き不足) | 予備のファイルを消した後、**DB を開き直してから** 会話の追記を続ける、と手順を書く。空き不足のエラーは `io::ErrorKind::StorageFull` で見分ける (Windows の `ERROR_DISK_FULL` / `ERROR_HANDLE_DISK_FULL`)。 |
| 03 §9 | Windows の `LockFileEx` は強制ロックで、ロックを持たれたファイルは他のプロセスから読めない (`ERROR_LOCK_VIOLATION`、os error 33)。PID を示すなら、ロックのファイルとは別のファイル (例: `lock.pid`) に書く。std の `File::lock` は全体しか取れないので、範囲を絞るなら `windows-sys` の `LockFileEx` を直に呼ぶことになる (unsafe と依存が増える)。ロックには std の `File::try_lock` (1.89 で安定、Windows は `LockFileEx`) を使えると書き、取れないときは `TryLockError::WouldBlock` で見分ける。加えて redb も DB のファイルをロックし、2 つ目は `DatabaseError::DatabaseAlreadyOpen` で失敗するので、これも終了コード 2 に対応させる。 |
| 03 §9 (表) | 今は `doctor`・`export` などを TUI の動作中に断る。redb 4.3 以降の `ConcurrencyMode::SingleWriter` なら別のプロセスの読み取りを許せる (ただし `Durability::None` が使えない)。v0.1 は今の表のまま (断る) でよいが、将来の選択肢として OQ に残す価値がある。 |
| 09 §4 | 電源断の模擬は「`StorageBackend` を差し替え、`sync_data` 前の書き込みを (a) すべて捨てる・(b) 4 KiB 単位で一部だけ残す・(c) すべて残す の 3 通りで開き直す」と方法を確定できる (S-01 の `src/sim.rs` が実例)。対照実験 (会話の追記を `None` にすると失うことを検出する) を試験に含める。ジャーナルにも同じ形の注入の層が要ると書く。同じ差し替えで空き不足 (`StorageFull`) と書き切りの失敗も注入できる。 |
| 09 §4 | 強制終了の試験は、kill の時点を「起動・修復の最中」と「コミットの連続の最中」の両方に散らす、と書く (S-01 では 1,000 回のうち 40〜67 回が起動・修復の最中に当たった)。 |
| 03 §2.2 | exFAT (と FAT32) はメタデータのジャーナルを持たず、電源断に弱い。S-01 の kill の試験は exFAT でも通ったが、電源断では NTFS より危ない。Windows の検査に「ファイルシステムが exFAT / FAT のときに警告する」を足すかを検討する。 |
| 03 §12.2 / 08 | redb は 2026-11-05 に 4.5.0、5.0.0 が開発中 (`experimental-api-5` に破壊的な変更が溜まっている)。v0.1 は 4.x に固定し、5.0 への移行は §12.2 の手順の対象になると見込む。4.5.0 は公開から日が浅い (検証の時点で 4 日)。 |

## 9. 確かめられなかったこと

- 実機の電源断 (ドライブのキャッシュ、`FlushFileBuffers` が本当に書き切るか)。模擬は §5 のとおり。
- 実際のディスクの空き不足 (小さい VHD を作るには管理者の権限が要るので、しなかった)。§7 は差し替えで調べた。
- 大きな DB (GB 級) の異常終了の後の修復の時間。quick_repair なしで 60〜85 MB に中央 46〜48 ms。大きさにほぼ比例すると見ると 1 GB で 0.5 秒を超えうる。
- 写しが書き手と同時だと遅くなる原因の切り分け。C: の書き手なしの写しの時間。
- 退避・compact・移行の最中の kill。
- Linux・macOS (この検証は Windows だけ)。ネットワークのドライブ・OneDrive の下。

## 10. 再現の手順

```sh
cd spikes/S-01
cargo build --release
B=target/release/s01-redb-durability.exe
$B crash <作業フォルダ> 1000        # 強制終了 (末尾に qr で quick_repair)
$B lock <作業フォルダ>              # ロック
$B copy <作業フォルダ> 100000       # 論理的な写しとコミットの時間
$B powerloss 1000                  # 電源断の模擬 (2pc で 2 相、none で対照実験)
$B diskfull                        # 空き不足と書き切りの失敗
```
