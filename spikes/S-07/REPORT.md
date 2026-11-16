# S-07: 実行ファイルの置き換え (Windows・Linux)

| 項目 | 内容 |
|---|---|
| 仕様 | 08 §3 の S-07、12 §7.2・§7.3 |
| 合格の条件 | 100 回の置き換えと巻き戻しで、起動できない状態が 0 回 |
| 実施 | 2026-11-17 |
| 環境 | Windows 11 Pro (NTFS の C: と exFAT の E:、ウイルス対策は ESET Security が有効、Windows Defender は停止)。WSL の Ubuntu (ext4) |
| 判定 | **合格** (Windows の NTFS・exFAT、Linux の ext4 で、300 回の操作のうち起動できない状態 0 回)。ただし、強制終了を各段に入れると、Windows の手順には起動できなくなる隙間がある (§3.2) |
| 再現 | `spikes/S-07` (`s07-swap`)。evorag の workspace から切り離した、依存の無い独立の crate。コードは `spike/S-07` ブランチにだけ置く (11 §9.1) |

## 1. 作ったもの

`s07-swap` の 1 つの実行ファイルが、2 つの役を兼ねる。

- 置き換えられる側: `app/evorag(.exe)` として置く。
- 試験を回す側。

版は、実行ファイルの末尾に付けた印 (`#S07-VERSION:00000001#`) で見分ける。PE も ELF も、末尾に足したバイトは実行に影響しない。

- `apply <新しいファイル> <方式> [故障の段]`: 自分自身を置き換え、今の自分を `evorag.prev` に残す。
  - 先に、フォルダに書き込めるかを確かめる。一時のファイルを作って消し、書き込めなければ何も変えずに断る (終了コード 10)。
  - ロック (Windows の 5・32・33、Unix の EBUSY・ETXTBSY) は 5 回まで再試行する。待ちは 200・400・600・800 ms で、合わせて 2 秒。
- `rollback <方式> [故障の段]`: `evorag.prev` と入れ替える。
- `cycles`: 置き換え → 巻き戻し → 置き換え を 1 回として、100 回 (300 回の操作)。
  - 操作のたびに `evorag version` で起動でき、版が期待どおりかを見る。
  - `evorag.prev` の版が期待どおりかも見る。
- `faults`: 各段の直後に `abort()` で落ち、その後に起動できるかを見る。起動できれば、同じ操作をやり直せるかも見る。
- `lock`: 別のプロセスが削除の共有なしでファイルを開いている間に置き換える (ウイルス対策のソフトの代わり)。
  - 開くファイルは `prev`・`target`・`new` の 3 通り。
  - 開いておく時間は 1 秒と 5 秒の 2 通り。
- `readonly`: 書き込めないフォルダ (Windows は ACL で書き込みと削除を拒否、Linux は `chmod 555`) で断るかを見る。

### 1.1 方式

| 方式 | 置き換え (apply) | 前の版の残し方 |
|---|---|---|
| `two-step` (12 §7.3 の今の手順) | `evorag.new` を書いて同期 → 古い `evorag.prev` を消す → 自分を `evorag.prev` に改名 → `evorag.new` を自分の名前に改名 (上書きしない改名、`MOVEFILE_WRITE_THROUGH`) | 自分の改名 |
| `movefile` (Windows) | 写しを作り、`MoveFileExW(MOVEFILE_REPLACE_EXISTING)` で自分の上に置く | 自分の写しを `evorag.prev` に上から置く |
| `posix` (Windows) | 写しを作り、`SetFileInformationByHandle(FileRenameInfoEx, REPLACE_IF_EXISTS \| POSIX_SEMANTICS)` で自分の上に置く | 同上 |
| `rename` (Linux) | 写しを作り、`rename(2)` で自分の上に置き、フォルダを同期する | 同上 |

## 2. 方式ごとの可否 (Windows)

| 方式 | NTFS | exFAT |
|---|---|---|
| `two-step` | 可 | 可 |
| `movefile` | **不可**: 動いている自分の上に置けない (ERROR_ACCESS_DENIED)。5 回の再試行の後に失敗し、元の版が残る | 不可 (同じ) |
| `posix` | **不可**: 動いている自分の上に置けない (ERROR_ACCESS_DENIED) | **不可**: POSIX の意味の改名を持たない (ERROR_INVALID_PARAMETER) |

- Windows で動いている実行ファイルを置き換えられるのは、**自分を改名してから置く `two-step` だけ** (12 §7.3 のとおり)。
- 動いていないファイル (`evorag.prev` など) の上に置くことは、`movefile` でも `posix` (NTFS) でもできた。
- exFAT (USB の外付けのドライブなど) にも置かれうる。`two-step` はそこでも動いた。

## 3. 結果

### 3.1 100 回の置き換えと巻き戻し (合格の条件)

| 環境 | 方式 | 操作 | 起動できない | 版の食い違い | ロックの再試行 | 中央値 / 最大 |
|---|---|---|---|---|---|---|
| Windows NTFS (C:) | two-step | 300 | 0 | 0 | 0 | 41 / 138 ms |
| Windows exFAT (E:) | two-step | 300 | 0 | 0 | 0 | 287 / 351 ms |
| Linux ext4 (WSL) | rename | 300 | 0 | 0 | 0 | 17 / 48 ms |
| Linux ext4 (WSL) | two-step | 300 | 0 | 0 | 0 | 13 / 34 ms |

- Windows では ESET Security のリアルタイムの検査が有効だった。新しく書いた実行ファイルへのロックは、300 回の操作で一度も起きなかった。
- Windows Defender での確認はしていない (この機械では止まっている)。

### 3.2 各段での強制終了

| 方式 | 操作 | 落ちた段 | 結果 |
|---|---|---|---|
| two-step (Windows・Linux とも) | apply | 3 (自分を `evorag.prev` に改名した直後) | **起動できない**。`evorag.prev` (前の版) と `evorag.new` (新しい版) だけが残る |
| two-step | rollback | 1 (自分を `evorag.tmp` に改名した直後) | **起動できない**。`evorag.prev` と `evorag.tmp` だけが残る |
| two-step | rollback | 2 | 起動できる。ただし前の版が `evorag.tmp` に残り、`evorag.prev` が無いので、もう巻き戻せない |
| two-step | 上のほか | — | 起動でき、やり直しも通る |
| rename (Linux) | apply・rollback | 1〜4 のどれも | 起動できる。やり直しも通る |
| rename (Linux) | rollback | 3 (自分の名前を前の版にした直後) | 起動できる。やり直すと、もう一度入れ替えるので、`evorag.prev` は新しい版ではなく前の版になる (新しい版は `evorag.tmp` に残る) |

- Windows の `two-step` には、2 つの改名の間に `evorag.exe` が無い時間がある。その間に落ちると起動できない。
  - 動いている自分の上には置けない (§2) ので、Windows ではこの隙間を無くせない。
  - 隙間は 2 つの改名の間だけ。新しいファイルの書き込みと同期は先に済ませてある。
  - それでも、強制終了や電源断が当たれば、利用者は evorag を起動できず、自分では戻せない。
- Linux は `rename` で上から置けるので、隙間が無い。`two-step` を使う理由は無い。

### 3.3 ロック (ウイルス対策のソフトの代わり)

Windows NTFS、two-step。押さえるファイルと時間を変えた結果:

| 押さえるファイル | 1 秒 | 5 秒 |
|---|---|---|
| 古い `evorag.prev` (消す段) | 再試行で成功 | 失敗。版 2 のまま起動できる |
| 動いている自分 (改名の段) | 再試行で成功 | 失敗。版 2 のまま起動できるが、**古い `evorag.prev` は消えている** (消す段は先に通ったため) |
| 書きかけの `evorag.new` (書く段) | 再試行で成功 | 失敗。版 2 のまま起動できる |

- 再試行の 2 秒を超えて押さえられても、起動できない状態にはならない。
- ただし、古い `evorag.prev` を先に消す手順のため、失敗すると巻き戻しの先を失うことがある。

### 3.4 書き込めない場所

| 環境 | 結果 |
|---|---|
| Windows NTFS (ACL で書き込みと削除を拒否) | 何も変えずに断った (終了コード 10)。版 1 のまま起動できる |
| Linux ext4 (`chmod 555`) | 同上 |

- 実際の `C:\Program Files` は試していない (管理者の権限で置く必要がある)。書き込みの確かめ方は同じ。

## 4. 仕様への反映の案 (spec/ ブランチで出す)

1. **Windows の手順は `two-step` のまま** (12 §7.3)。§2 の理由を書き添える。
2. **Windows の隙間への備え**:
   - 2 つの改名の間に、ほかの処理 (書き込み・同期・表示) を入れない。
   - 手作業での戻し方を、同梱の README と `evorag update` の表示に書く。`evorag.exe` が無く `evorag.prev` か `evorag.new` が残っていたら、`evorag.new` (無ければ `evorag.prev`) を `evorag.exe` に改名する。
   - 12 §7.1 (手作業の更新) にも同じ手順を書く。
3. **古い `evorag.prev` を先に消さない**:
   - 先に `evorag.prev.old` へ改名しておき、置き換えが済んでから消す。
   - 置き換えに失敗したら、`evorag.prev.old` を `evorag.prev` に戻す (§3.3)。
4. **残ったファイルの片付けと続き** (次の起動か、次の `update` で):
   - `evorag.prev` が無く `evorag.tmp` があれば、`evorag.tmp` を `evorag.prev` にする (§3.2 の rollback の段 2)。
   - 余った `evorag.new`・`evorag.tmp` は消す。
   - `rollback` は「入れ替え」をそのまま繰り返さない。どちらの版を `evorag.exe` にするかを、始める前に `update/` の状態のファイルへ書く。途中で落ちたら、その記録に従って続きから進める (§3.2 の Linux の段 3)。
5. **Linux (と macOS) の手順**:
   - 12 §7.3 の「rename で置き換える」に、前の版の残し方を足す。
   - 自分を改名するのではなく、写しを `evorag.tmp` に書いて `evorag.prev` に `rename` で置く。
   - その後に、新しいファイルを `rename` で上から置き、フォルダを同期する。
   - こうすれば、どの段で落ちても `evorag` はある (§3.2)。
6. **再試行の待ち**:
   - 12 §7.3 の「5 回まで再試行する」に、待ちの長さ (200・400・600・800 ms、合わせて 2 秒) を書く。
   - ウイルス対策のソフトの検査がもっと長く続く環境もありうる。待ちを延ばすか (例: 合わせて 10 秒) を決める。失敗しても起動できる状態は保てる (§3.3)。
7. **確かめていないもの**: Windows Defender、実際の `C:\Program Files`、macOS。macOS の実行ファイルの置き換えは、v0.6 の macOS 版のときに確かめる (S-06 と同じく G6)。
