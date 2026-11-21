# evorag user guide

evorag is a chat and long-term memory tool that runs only against your own llama-server (llama.cpp). Conversations never leave your PC. It finds related remarks in your earlier conversations and attaches them to its answers, with source numbers. From v0.2, adding llama-servers for embeddings (closeness of meaning) and a reranker (judging how relevant something is) makes it robust to rewording as well.

This guide is bundled with the download (zip / tar.gz). The Japanese version is `README.md`. The specification is in `docs/` at <https://github.com/evoref/evorag> (in Japanese).

## Contents

1. [Requirements](#1-requirements)
2. [Installing](#2-installing)
3. [First setup (evorag setup)](#3-first-setup-evorag-setup)
4. [Usage](#4-usage)
5. [Where data is kept, and backups](#5-where-data-is-kept-and-backups)
6. [Updating and rolling back](#6-updating-and-rolling-back)
7. [Troubleshooting](#7-troubleshooting)
8. [Known limitations](#8-known-limitations)
9. [Uninstalling](#9-uninstalling)

## 1. Requirements

- Windows 11 (x86_64) or Linux (x86_64).
- A terminal: [Windows Terminal](https://aka.ms/terminal) on Windows, GNOME Terminal on Linux (it also works inside tmux). Japanese input has been tested with Microsoft IME (new and old), Google Japanese Input and ATOK on Windows, and with fcitx5 and ibus on Linux.
- llama-server (llama.cpp) and a model in GGUF format. evorag does not install llama-server.
- Free disk space (for the conversation data, the indexes and the automatic backups).

### Recommended setup (three llama-servers)

| Role | Example | Without it |
|---|---|---|
| Replies (chat) | The model you chat with (such as Qwen3.5-9B) | Not usable (required) |
| Embeddings (embed) | bge-m3 (Q8_0, about 0.6 GB) | Earlier conversations are found by shared characters only |
| Reranker (rerank) | bge-reranker-v2-m3 (Q8_0, about 0.6 GB) | Whether to attach is decided by the same rules as v0.1 |

- GPU memory: on top of the reply model and its context, allow about 2 GB in total for the embedding model and the reranker. If that is not enough, the embedding model and the reranker also work on the CPU (replies take a little longer to start).
- evorag works without the embedding model and the reranker. With them, it also finds rewordings (such as "coffee" written in different scripts) and attaches less to unrelated questions.

## 2. Installing

**Windows**

1. From [Releases](https://github.com/evoref/evorag/releases), download `evorag-<version>-x86_64-pc-windows-msvc.zip`, together with `SHA256SUMS`, `SHA256SUMS.minisig`, `release.json` and `release.json.minisig`.
2. Check that the download is genuine ([6.1](#61-verifying-the-download)).
3. Extract the zip to any folder (for example `%LOCALAPPDATA%\Programs\evorag`). No administrator rights are needed.
4. (Optional) Add that folder to the `PATH` environment variable to start `evorag` from anywhere.

> The first time you start it, Windows SmartScreen may warn "Windows protected your PC" (code signing will be considered before v1.0). After verifying the download, choose "More info" → "Run anyway".

**Linux**

1. Download `evorag-<version>-x86_64-unknown-linux-musl.tar.gz` and the same `SHA256SUMS` and other files as above, and verify them ([6.1](#61-verifying-the-download)).
2. Extract it and put the executable somewhere on your `PATH`. It is statically linked, so no other libraries are needed.

   ```sh
   tar -xzf evorag-<version>-x86_64-unknown-linux-musl.tar.gz
   install -m 755 evorag-<version>-x86_64-unknown-linux-musl/evorag ~/.local/bin/evorag
   ```

## 3. First setup (evorag setup)

1. Start llama-server. We recommend `--host 127.0.0.1`, so that it accepts connections only from your own PC, and an API key. An example running the three roles on separate ports (PowerShell; the arguments are the same on Linux):

   ```powershell
   # Replies
   llama-server -m <reply model>.gguf --host 127.0.0.1 --port 8080 -c 8192 -np 2 --jinja --api-key <a long random string>
   # Embeddings
   llama-server -m bge-m3-q8_0.gguf --host 127.0.0.1 --port 8081 --embedding -c 8192 -b 4096 -ub 4096 --api-key <the same string>
   # Reranker
   llama-server -m bge-reranker-v2-m3-Q8_0.gguf --host 127.0.0.1 --port 8082 --reranking -c 8192 -b 4096 -ub 4096 --api-key <the same string>
   ```

   - `-np 2` (two slots) on the reply llama-server keeps background work (making search keys for notes) from getting in the way of replies. One slot also works.
   - Router mode (presets), where one llama-server runs all three roles, also works. When `evorag setup` finds one, it shows an example preset.

2. Put the same string in an environment variable and run `evorag setup`.

   ```powershell
   $env:EVORAG_API_KEY = "<the same string as above>"
   evorag setup --api-key-env EVORAG_API_KEY
   ```

   - `setup` looks at ports 8080, 8081, 8082 and so on of `127.0.0.1`, tells the roles (replies, embeddings, reranker) apart through the API, and writes them to the configuration. For another port, add `--url http://127.0.0.1:<port>`. For a role it cannot find, it shows an example of starting one.
   - It records the models in use for the data root and checks what they can do (speed, whether thinking can be turned off, whether structured output works). For the reranker, it computes the value that decides whether to attach (the threshold) from bundled examples (about a minute).
   - The first time, it asks for the language of the system prompt (`ja` / `en`). **This language cannot be changed later** (to change it, start with a new data root). Replies follow the language you write in.
   - To change only the language of replies, use `/style language`.
   - Environment variables disappear when the terminal is closed. Set it each time, or register it as a user environment variable.

3. For every key of the configuration file, see the bundled `config.example.en.toml` (the same example with English comments; `config.example.toml` is the Japanese version).
   - The language of the screens is chosen with `ui.language` in the configuration (`ja` (default) / `en`). It can be changed at any time and takes effect from the next start. It is separate from the language of the system prompt. For English screens, write this in `config.toml`:

     ```toml
     [ui]
     language = "en"
     ```

## 4. Usage

Start it with `evorag`. If it cannot connect to the reply llama-server, it shows the reason and exits. If it cannot connect to the embedding server or the reranker, it runs without them and reconnects every 30 seconds.

### 4.1 Keys

| Key | Action |
|---|---|
| Enter | Send |
| Shift+Enter / Alt+Enter | New line |
| Esc | Stop generating / close a window |
| Ctrl+E | Write in an external editor (`VISUAL` → `EDITOR` → Notepad on Windows) |
| Tab | Choose a source number of a reply or a "possibly related record" (Enter for details) |
| PageUp / PageDown | Scroll the conversation |
| Ctrl+C | Stops generating. On an empty input, press twice to quit |
| Ctrl+Q | Quit |

### 4.2 Commands

| Command | Description |
|---|---|
| `/help` | List of commands |
| `/new [title]` | Create a session |
| `/sessions` | List and resume sessions |
| `/rename <title>` | Rename the current session |
| `/delete` | Delete the current session (asks first; also removed from the index and the automatic backups) |
| `/retry` | Regenerate the last reply (the previous reply is kept) |
| `/good`, `/bad [reason]` | Rate the last reply |
| `/sources [on\|off]` | Whether to attach earlier conversations to replies in this session |
| `/memory` | Earlier conversations attached to the last reply (source numbers), possibly related records, rerank scores and their difference from the threshold, reasons for degraded recall and what to do next, and the sources of this session. From v0.3, also the list of facts (numbered like `F1`) |
| `/memory accept <number>` | Make a `Contested` (conflicting) value in the list of facts the current value |
| `/memory history <number>`, `/memory revert <history number>` | The replacement history of that fact, and making a value from the history the current value again |
| `/memory forgotten`, `/memory unblock <number>` | The list of forgotten facts (values are not shown), and removing the mark that keeps a fact from being learned again |
| `/forget <number>` | Forget a fact (asks first; you can also choose to redact the value in the source conversations) |
| `/private <number>`, `/private off <number>` | Keep a fact out of replies, or undo that (asks first; nothing is deleted) |
| `/style [item value]` | Default answer style (language, length, format). Without a value, opens a picker (applies to all sessions) |
| `/model` | Show the recorded models and the connected models (switch with `evorag setup`) |
| `/quit` | Quit |

To send text that starts with `/` as it is, start it with `//`.

### 4.3 Recalling earlier conversations

- evorag looks for earlier conversations (in other sessions) related to what you sent, and attaches the relevant exchanges (a message and its reply) to the reply. It attaches the sentences in the exchange that match your question, with the sentences around them (so it reaches answers near the end of a long reply). When several exchanges are relevant, it attaches them together within a fixed amount. What it attaches is shown in the reply with a number such as `[1]`.
- How it searches: it gathers candidates both by shared characters (lexical) and by closeness of meaning (embeddings), measures relevance with the reranker, and attaches only what is at or above the threshold. Nothing is attached to unrelated questions.
- Exchanges just below the threshold are shown faintly under the reply as "Possibly related records (not used for this reply)". Choose one with Tab and open the details with Enter; there, `a` attaches it and regenerates, and `x` marks it "irrelevant" (the arrow keys and Enter work too).
- For each topic of a conversation, search keys (notes) are made in the background. The keys are used only for searching; what is attached is the original conversation.
- **What is attached is not always right.** Check the source for anything important (Tab → Enter). Earlier conversations are passed as "quoted references", and instructions written in them are not followed.
- The "Recall:" field of the status line: `standard` (both embeddings and the reranker), `simple` (degraded; works on shared characters and the like), `candidates only` (no threshold; nothing is attached automatically), `rebuilding N%`. `/memory` and `/model` show the reason and what to do next.
- You can turn it off per session with `/sources off`, and by default with `memory.enabled = false` in the configuration.

### 4.4 Background work and work done ahead

- While you are not using it (60 seconds by default, `memory.idle_seconds`), it embeds earlier conversations and makes search keys for notes, and when it has been running past midnight, it takes the automatic backup. A key press stops it within a second; it continues where it left off at the next chance. The status line shows "Organizing".
- Right after writing a reply, and right after you open a session, it has llama-server process the context for your next message ahead of time (so the next reply starts sooner).
- When the text in the input box has not changed for about 0.5 seconds, it searches earlier conversations with that text ahead of time and has llama-server process the quotes it would attach (prefetch). If you send that text as it is, the reply starts right away. If you change it before sending, it searches again. **Text you have not sent also goes to the configured llama-server** (it does not leave your PC, and it is not saved). To turn this off, write `retrieval.precompute = false` in the configuration. With `llm.allow_remote = true`, it is off unless you write it. Text you have not sent is never sent to a llama-server outside your PC (judged per role).
- "Organizing (3 left)" in the status line is the number of background steps still to do. If you close evorag right after talking, steps such as extracting facts may not finish.
- On battery, it follows `memory.on_battery` (`reduce`: stops the work when input pauses, and waits five times longer for background work / `normal` / `stop`: stops both). `memory.sleep_time = false` stops background work. Even then, conversations are saved and recall works.

### 4.5 Other subcommands

| Command | Description |
|---|---|
| `evorag setup` | Find llama-server, write the configuration, record and check the models. Models are also switched here |
| `evorag doctor` | Check the configuration, connection, model records and check results, data, journal, indexes and backups, without changing anything |
| `evorag backups list` | List the backups |
| `evorag backups purge [--yes]` | Delete all backups (asks first; `--yes` skips the question) |
| `evorag restore <backup>` | Restore from a backup and add the later conversations from the journal |
| `evorag rebuild [--index \| --facts]` | Rebuild derived data (chunks, notes, embeddings, indexes, check results) from the conversations. Backs up first. `--index` rebuilds only the indexes. `--facts` also extracts the facts from the conversations again (uses llama-server) |
| `evorag repair` | Rebuild secondary indexes and relations from the records, restore records that are in the journal but not in the database, and re-apply forgets and deletions. Backs up first |
| `evorag export <folder>` | Export the records as JSON Lines (the configuration is copied without API keys) |
| `evorag update check` / `apply [version]` / `rollback` | Check for a new version, update, roll back ([6](#6-updating-and-rolling-back)) |
| `evorag --version` | Version and build information |

Common options: `--data-root <path>` (change where the data is kept), `--config <path>`.

## 5. Where data is kept, and backups

| What | Windows (default) | Linux (default) |
|---|---|---|
| Configuration | `%LOCALAPPDATA%\evorag\config\config.toml` | `~/.config/evorag/config.toml` |
| Data root (conversation database, journal, indexes, backups) | `%LOCALAPPDATA%\evorag\data\` | `~/.local/share/evorag/` |
| Drafts of the input box, logs (7 days; conversation text and API keys are not written) and the update record (`update/`) | `%LOCALAPPDATA%\evorag\state\` | `~/.local/state/evorag/` |

On Linux, `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `XDG_STATE_HOME` are used when set.

- Conversations are written to two places: the database (`evorag.redb`) and a separate journal (`journal/`). Confirmed conversations are not lost even after a forced exit or a power failure. Chunks, notes, embeddings and indexes can be rebuilt from the conversations, so they are not written to the journal.
- At most once a day, a copy of the database is made in `backups/daily/` (7 days, plus 4 weekly copies). A backup is also taken before migrations, rebuilds and restores (`backups/pre-op/`).
- **Do not put the data root in a sync folder (such as OneDrive) or on a network drive.** By default, evorag refuses to start there.
- If the database is broken at start, the broken file is moved to `broken/` (not deleted), and you are asked whether to restore from the backups and the journal.

## 6. Updating and rolling back

From v0.3, `evorag update` checks for a new version, updates, and goes back to the previous version. evorag connects only to the configured llama-server, and to GitHub when you run `evorag update check` or `evorag update apply`. It does not check at start or periodically (`evorag update rollback` does not connect at all). The OS may still connect to verify certificates ([6.5](#65-network-access-and-proxies)).

| Command | Description |
|---|---|
| `evorag update check` | If a new version exists, show its version, release date and what changed. Changes nothing |
| `evorag update apply [version]` | Download the package, verify its signature, then replace the executable. The current executable is kept as `evorag.prev`. Without a version, installs the newest one |
| `evorag update rollback` | Go back to `evorag.prev`, and show whether the data can be used by the version you went back to |

When `evorag update` cannot be used (the CPU lacks a feature or there are no root certificates, there is no network access, the format of `release.json` cannot be read, and so on), update by hand as described in [6.6](#66-updating-by-hand).

### 6.1 Verifying the download

When you install or update by hand, check that the download is genuine (`evorag update` does the same automatically).

1. Check the hash. If the value shown matches the line in `SHA256SUMS`, the file is not damaged.

   ```powershell
   Get-FileHash .\evorag-<version>-x86_64-pc-windows-msvc.zip -Algorithm SHA256
   Get-Content .\SHA256SUMS
   ```

   On Linux, you can use `sha256sum -c --ignore-missing SHA256SUMS`.

2. Check the signatures with [minisign](https://jedisct1.github.io/minisign/). The public key is `minisign.pub` at the root of the repository. Check that the key ID at the end of its first line is `A3B00C77E1EA828E`.

   ```powershell
   minisign -Vm SHA256SUMS -p minisign.pub
   minisign -Vm release.json -p minisign.pub
   ```

   If `Signature and comment signature verified` is shown, it is genuine. The trusted comment of the `release.json` signature holds the version and the SHA-256 of `release.json`.

### 6.2 Updating with evorag update

1. Run `evorag update check` to see whether a new version exists. It shows the version, the release date and what changed (nothing is changed).
2. **Quit evorag** (while an evorag is using the data root, `apply` refuses with exit code 2).
3. Run `evorag update apply`. To choose a version, write it like `evorag update apply 0.4.0`.
   - It downloads the signed `release.json` and verifies the signature with the key built into evorag, then downloads the package and checks its size and SHA-256. If anything does not match, it refuses without changing anything (exit code 5 when tampering is suspected).
   - After putting the new executable in place, it checks with `--version` that it runs. If it does not, the replacement is reverted.
   - The current executable is kept as `evorag.prev` (do not delete it; it is used to roll back).
4. Start the new version. A version that changes the data format backs up automatically at its first start and then migrates. If the migration fails, it starts read-only on the original data and suggests `evorag update rollback`.

- `apply` cannot install the same or an older version. Going back to the previous version is done only with `rollback`.
- For a version that cannot be updated to directly from the current one (`upgrade_from`), you are told which version to install first (run `evorag update apply <that version>` first).
- Install into a folder that only you can write to (on Windows, such as `%LOCALAPPDATA%\Programs\evorag`). evorag refuses without changing anything if it cannot write to the folder (such as Program Files), if other users can write to it, or if the executable has been renamed from `evorag(.exe)`.
- **If you installed with cargo** (`installed via: cargo` in `evorag --version`): `apply` and `rollback` cannot be used. When `evorag update check` finds a new version, it shows the command to reinstall (`cargo install --locked --git https://github.com/evoref/evorag --tag v<version> evorag`). When another version has to be installed first, that command is shown first.

| Exit code | Meaning |
|---|---|
| 0 | Done (also 0 when there is no new version) |
| 1 | Refused or failed (no network access, rate limit, an unusable version, conditions on the install folder, reverted because the check after the replacement failed, and so on) |
| 2 | evorag is running (it is using the data root) |
| 4 | An error in `update.trusted_keys` in the configuration |
| 5 | Refused because tampering is suspected (the signature does not verify, the version or tag does not match, the size or SHA-256 of the package does not match, and so on) |

### 6.3 Rolling back (evorag update rollback)

1. Quit evorag.
2. Run `evorag update rollback`. It puts `evorag.prev` back as `evorag(.exe)` and keeps the current version as `evorag.prev` (running `rollback` again swaps them back).
3. It shows whether the data can be used by the version you went back to.
   - If the new version has migrated the data, the old version may start **read-only**, or may not be able to open the data (so as not to break the new version's data). In that case it shows the path of the backup taken before the migration (`backups/pre-op/`) and the command to restore it (`evorag restore <backup>`, run with the version you went back to). Conversations after the backup are added from the journal.
   - If you do not restore, the data stays writable by the new version (go back to it with `evorag update apply`).
   - If the configuration file was migrated, the path of its copy (`config.toml.bak-v<old version>`) is shown as well.

- A data root used by v0.2 can be opened by v0.1.2, and its sessions can be opened. However, **for sessions deleted in v0.1.2, what v0.2 made from them (chunks, notes, embeddings, indexes) and the backups that contain them remain until the data root is opened by v0.2 again** (because v0.1.2 does not know about them). When v0.2 opens it again, they are removed from the database, the index and the backups.

### 6.4 If it stops partway

Even after a forced exit or a power failure in the middle of a replacement, the next `evorag update apply` or `rollback`, or the next start of evorag, finishes or reverts it according to the state file (`update/state.json` in the state folder).

**The gap on Windows**: on Windows, the running `evorag.exe` is renamed before the new file is put in place, so for a moment there is no `evorag.exe`. If it stops right there and `evorag.exe` is missing, put it back as follows, then start it (`apply` and `rollback` also show this before they begin).

- During `apply`: rename `evorag.new` to `evorag.exe` if it exists, or otherwise rename `evorag.prev` to `evorag.exe`.
- During `rollback`: rename `evorag.prev` to `evorag.exe`.

Either way, the next `evorag update` operation or the next start of evorag tidies up the rest.

**To recover by hand** (when you are told that the state file cannot be read, or that the files cannot be told apart):

1. Exit evorag.
2. Run each of `evorag(.exe)`, `evorag.new`, `evorag.tmp`, `evorag.prev`, `evorag.prev.old` and `evorag.rejected` in the install folder with `--version` to see its version (on Windows, for a file without an extension, make a copy named `evorag.exe` in another folder and run that).
3. Rename the file of the version you want to use to `evorag(.exe)` and the file of the previous version to `evorag.prev`, and delete the rest.
4. Delete `update/state.json` in the state folder (`%LOCALAPPDATA%\evorag\state\update\state.json` on Windows, `~/.local/state/evorag/update/state.json` on Linux). The record for rollback is deleted with it, so the next `rollback` gives only general guidance.

### 6.5 Network access and proxies

- The only hosts contacted are `api.github.com` (the list of releases) and `github.com` (`release.json`, signatures and packages); downloads of packages may be redirected to `*.githubusercontent.com`. Redirects anywhere else are not followed.
- All that is sent is the evorag version and the OS type (User-Agent) and the name of the package to download. No cookies or credentials are sent.
- Whether a download is genuine is decided by the minisign signature, not by TLS. Even if something is altered in transit, a package whose signature does not match is not installed.
- **Proxies**: the environment variables `HTTPS_PROXY`, `ALL_PROXY` and `HTTP_PROXY` are read in this order (for each, the upper-case name first, then the lower-case `https_proxy`, `all_proxy`, `http_proxy`), and the proxy in the first non-empty value is used with HTTP CONNECT.
  - Only proxies starting with `http://` or `https://` are supported. If the first value is a SOCKS proxy such as `socks5://`, another scheme, a value without a scheme such as `proxy:8080`, or a value that cannot be read, evorag does not connect directly; it names the environment variable and stops before connecting (exit code 1). Fix that environment variable.
  - Hosts listed in `NO_PROXY` (or `no_proxy` when it is missing or empty) do not go through the proxy. The list is comma-separated, and `github.com`, `.github.com` and `*.github.com` all match `github.com` and the hosts under it (such as `api.github.com`). `*` matches every host.
  - Windows "Proxy settings" (WinINET) and automatic configuration scripts (PAC) are not read.

  ```powershell
  $env:HTTPS_PROXY = "http://proxy.example.com:8080"
  evorag update check
  ```

- For proxies that need authentication, only Basic authentication written in the URL (`http://user:pass@proxy:8080`) is supported. **NTLM and other authentication methods are not supported**. In that case, update by hand (6.6).
- Anonymous calls to the GitHub API are rate limited (60 per hour). When the limit is hit, evorag shows when it is lifted and exits.
- On Linux, the OS certificates (such as `ca-certificates`) are needed. If none are found, evorag stops without connecting.
- Certificates are verified by the OS verifier. Depending on the OS settings, the OS may connect to the certificate authorities' revocation services (OCSP, CRL) or fetch intermediate certificates (for example on Windows; this does not follow `HTTPS_PROXY`). evorag cannot stop this. What is sent is information about GitHub's certificates, not your data. Whether a download is genuine is decided by the signature, so evorag does not rely on the result of this verification.

### 6.6 Updating by hand

Use these steps when `evorag update` cannot be used (no network access, a proxy that needs authentication, the CPU lacks a feature or there are no root certificates, and so on).

1. On another PC, for example, download the package of the new version and `SHA256SUMS`, `SHA256SUMS.minisig`, `release.json` and `release.json.minisig` from [Releases](https://github.com/evoref/evorag/releases), and verify them (6.1).
2. Quit evorag.
3. Rename the current executable (`evorag.exe` / `evorag`) to `evorag.prev` and put the new one in its place (do not delete `evorag.prev`; it is used to roll back).
4. Start the new version. A version that changes the data format backs up automatically at start and then migrates (the release notes say so).

To roll back by hand, quit evorag, delete the current executable and rename `evorag.prev` back to the original name. If the data has been migrated, restore the backup taken before the migration, as in 6.3.

```powershell
evorag backups list
evorag restore "<data root>\backups\pre-op\<date and time>"
```

### 6.7 If you are updating from v0.1

- **The reply model is recorded**: at the first start, the connected reply model is recorded for the data root. From then on, if llama-server runs a model that differs from the record, evorag does not open its screen; it shows the difference and what you can do (restart with the recorded model, switch with `evorag setup`, or try with another data root) and exits (exit code 6).
- **`/model` now only shows**: models are switched with `evorag setup`. `[[llm.chat.models]]` in the configuration is read and ignored, and you are told once (you can remove it unless you plan to go back to v0.1).
- **Embeddings and the reranker**: run `evorag setup` again and it finds the llama-servers on 8081 and 8082 and writes them to the configuration. Embeddings of v0.1 conversations are made in the background after the update (this can take a while depending on how much you have talked; meanwhile the status line shows "Recall: rebuilding N%", and searches use what has been made and shared characters).
- **Input before sending**: the search done ahead when input pauses sends text you have not sent to your own llama-server (4.4). To turn it off, use `retrieval.precompute = false`.
- **The index is rebuilt**: the lexical index of v0.1 is not used; it is rebuilt at the first start (the conversation data does not change).
- v0.1 and v0.2 do not have `evorag update`. Update to v0.3 by hand (6.6).

### 6.8 If you are updating from v0.3

- **The data format does not change**: a v0.3 data root opens as it is (only more observation fields are recorded).
- **Recall attaches more**: it attaches several relevant exchanges together, with the sentences around the ones that match your question (4.3). Because it attaches more, the reply can take a little longer to start when the prefetch misses.
- **Prefetch**: when input pauses, it has llama-server process the quotes it would attach ahead of time (4.4).
- **Fact extraction with a thinking model**: in v0.3, fact extraction stopped when the configuration had no setting to turn thinking off. v0.4 tries it in the model check. A model whose v0.3 check could not turn thinking off is checked again at the first start of v0.4 (in the background, about a minute).

### 6.9 Restoring from a backup

To put the database back to an earlier state (after a mistaken action, for example), quit evorag and then:

```powershell
evorag backups list
evorag restore "<data root>\backups\daily\<date>"
```

The database from before the restore is copied to `backups/pre-op/`, as before a migration (a database that cannot be read is moved to `broken/`).

## 7. Troubleshooting

- First, run `evorag doctor`. It shows the state of the configuration, connection, model records and check results, data root, journal, indexes and backups, without changing anything.
- **Cannot connect to llama-server**: check that llama-server is running and that it matches `llm.chat.url` in `config.toml`, then run `evorag setup` again.
- **It does not start because of a "model that differs from the record" (exit code 6)**: restart llama-server with the recorded model, or switch with `evorag setup` (changing the embedding model rebuilds all embeddings, and changing the reranker recomputes the threshold).
- **Recall: simple / candidates only**: see the reason with `/memory` and `/model`, and set up the embedding server and the reranker with `evorag setup`. Reranker calibration runs in the background after it connects (about a minute).
- **"The data root (…) is in use by another evorag (pid …)"**: another evorag is using the same data root. Quit the evorag that was started first, then run it again.
- **`evorag update` cannot connect**: if you use a proxy, set `HTTPS_PROXY` ([6.5](#65-network-access-and-proxies)). If it still cannot connect, update by hand ([6.6](#66-updating-by-hand)).
- **An update stopped partway and `evorag.exe` is missing**: put it back as described in [6.4](#64-if-it-stops-partway).
- **Facts do not appear in `/memory`**: if the status line shows "left", background work has not finished. Leave evorag idle for a while (longer on battery). If the model check in `evorag doctor` says thinking cannot be turned off, helper calls (fact extraction) are stopped. From v0.4, the check tries the setting that turns thinking off by itself (for a thinking model, `evorag doctor` says it adds the setting).
- **Replies take long to start**: a thinking model shows "Thinking" while it thinks. To turn thinking off, write the following in `config.toml` (from v0.4, the model check turns thinking off for helper calls by itself; to write it in the configuration, put the same under `[llm.chat.aux_request_extra]`).

  ```toml
  [llm.chat.request_extra]
  chat_template_kwargs = { enable_thinking = false }
  ```

- **The recall index**: if it is broken, it is rebuilt from the conversations at the next start (no conversation data is lost). You can also rebuild it with `evorag rebuild --index`.
- Report bugs in Issues, and vulnerabilities through the contact in `SECURITY.md`. **Do not paste conversation text or API keys into reports.**

## 8. Known limitations

- The supported operating systems are Windows 11 (x86_64) and Linux (x86_64). macOS will be released once a real machine is available for testing.
- Japanese input has been tested with the combinations listed in 1. On the legacy Windows console (conhost), the display may be garbled.
- Rewordings that share no characters at all become candidates through closeness of meaning, but often do not reach the reranker threshold and are not attached (they may appear as "possibly related records").
- There is no automatic update. evorag updates only when you run `evorag update`, and proxy authentication other than Basic in the URL (such as NTLM) is not supported (see 6).
- Compatibility of the data format is promised from v0.5. Until then, too, a new version migrates the data of the previous version automatically and backs up before migrating.
- When the configuration file is rewritten (migrating an older version's configuration, or rewriting by `setup`), comments may be lost. The original file is kept as `config.toml.bak-*`.
- The external editor is started by splitting the value of `VISUAL` or `EDITOR` at spaces (for an editor whose path contains spaces, give a short path or a place that needs no quotes).

## 9. Uninstalling

1. Quit evorag.
2. Delete the extracted folder (on Linux, the executable you put in place).
3. To delete the data too, delete `%LOCALAPPDATA%\evorag\` on Windows, or `~/.config/evorag/`, `~/.local/share/evorag/` and `~/.local/state/evorag/` on Linux (all conversations, backups and settings are deleted; if needed, export them first with `evorag export`).

## License

evorag is licensed under the Apache License 2.0 (`LICENSE`, `NOTICE`). The licenses of the open source software it contains are in `THIRD-PARTY-NOTICES.md`.
