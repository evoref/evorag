# evorag

English | [日本語](README.ja.md)

A chat and long-term memory tool written in Rust. It runs against your own llama-server (llama.cpp) and nothing else, so conversations stay on your PC. It attaches related parts of your earlier conversations to its answers, with sources. It remembers facts about you, replaces them when you correct yourself, and forgets them when you ask. Later versions will improve from your ratings and corrections. It never trains model weights.

> **The latest release is v0.4.0 (2026-11-21).** Downloads are on [GitHub Releases](https://github.com/evoref/evorag/releases): a zip for Windows 11 (x86_64) and a tar.gz for Linux (x86_64). Installation, setup, updating and rolling back, signature verification and known limitations are described in the bundled README ([dist/README.en.md](dist/README.en.md); the Japanese [dist/README.md](dist/README.md) is the original).

## Status

- **v0.1 (minimal core)** was released as v0.1.0 on 2026-11-10. v0.1.1, released the same day, fixed error messages. v0.1.2 (2026-11-11) lets v0.1 open sessions that contain records written by a newer version.
  - It runs in Windows Terminal on Windows 11. The user interface is in Japanese.
  - It chats with a local llama-server, showing the answer as it is generated. You can interrupt and regenerate.
  - Conversations are kept as sessions. They are written to a database and a journal, so they survive a forced exit. A backup is taken once a day, and `evorag restore` brings one back.
  - It finds related remarks in earlier conversations (a lexical index) and attaches them to the answer with source numbers.
  - Linux and macOS are not supported yet.
- **v0.2 (recall)** was released as v0.2.0 on 2026-11-12.
  - It shortens the wait before an answer starts: the context keeps the same prefix from turn to turn, and the next turn is processed ahead of time.
  - With an embedding server and a reranker (optional), it finds earlier conversations by meaning as well as by words, and attaches only the one exchange the reranker judges relevant. Nearly relevant ones are shown below the answer, and `/memory` explains the choice.
  - While you are away, it embeds earlier conversations and writes search keys for each topic (notes).
  - You can choose the default answer language, length and format (`/style`).
  - Linux (x86_64, GNOME Terminal and tmux) is supported.
  - The storage format moved to 0.2; a v0.1 data root is backed up and migrated on the first start.
- **v0.3 (memory)** was released as v0.3.0 on 2026-11-19.
  - While you are away, it extracts facts about you from conversations (name, where you live, likes and dislikes, plans and so on) and attaches them to answers in other sessions too. Only values that pass fixed rule-based gates are used. `/memory` lists them and lets you accept, view the history of, and revert them.
  - When you correct yourself ("not X but Y"), it replaces the old value.
  - `/forget` forgets a fact and the text it came from: the text is redacted and removed from the database, the journal, the indexes and the automatic backups, and the same value is not learned again. `/private` marks a fact so that it is not sent to llama-server.
  - `evorag update` updates the program and rolls it back. Downloads are checked by signature. It goes online only when you run `update check` or `update apply`.
  - The user interface can be switched to English (`ui.language = "en"`).
  - Added `evorag repair`, `evorag backups purge` and `evorag rebuild --facts`.
  - The storage format moved to 0.3. Records are not rewritten, so v0.2 can still read and write the data. From v0.2, update by hand as described in the bundled README.
- **v0.4 (faster and fuller recall)** was released as v0.4.0 on 2026-11-21.
  - It attaches several relevant exchanges from earlier conversations together, using the sentences that match your question and the sentences around them. It reaches answers near the end of a long reply and answers spread over several sessions. For questions about counts, totals and lists, it gathers more.
  - When input pauses, it has llama-server process the quotes it would attach ahead of time (prefetch); if you send that text as it is, the reply starts right away. Text you have not sent is not saved and is never sent to a llama-server outside your PC.
  - Fixed fact extraction stopping with a thinking model when the configuration had no setting to turn thinking off (the model check now tries it).
  - The status line shows how many background steps are left.
  - The storage format stays at 0.3 (only more observation fields are recorded).

The plan for each version up to v0.8 is in [docs/08-roadmap.md](docs/08-roadmap.md) (in Japanese).

The storage format may change until v0.7 (v0.5 imports into a new data root). Each version migrates data from the version just before it, so update one version at a time.

Changes are listed in [CHANGELOG.md](CHANGELOG.md) (in Japanese).

## Requirements

evorag does not include a model or an inference engine. You need a running llama-server from [llama.cpp](https://github.com/ggml-org/llama.cpp) and a chat model in GGUF format. From v0.2, an embedding model and a reranker (each served by its own llama-server, for example bge-m3 and bge-reranker-v2-m3) are recommended but optional. `evorag setup` finds the servers, checks what they can do, and writes the configuration.

By default evorag connects only to a llama-server on the same PC (loopback).

## Verifying downloads

Each release has a `SHA256SUMS` file signed with minisign (`SHA256SUMS.minisig`). The public key is below. It is the same as [minisign.pub](minisign.pub) in this repository; compare the two before you trust it.

```text
untrusted comment: minisign public key A3B00C77E1EA828E
RWSOgurhdwywo1k6KUE/J4IMo5HcwmhR6dsz/+OgROUdnDutFx3qxies
```

```sh
minisign -Vm SHA256SUMS -P RWSOgurhdwywo1k6KUE/J4IMo5HcwmhR6dsz/+OgROUdnDutFx3qxies
```

evorag also embeds a second key, the next key ([minisign.next.pub](minisign.next.pub)), so that it can switch keys if the current one has to be retired. `evorag update` accepts a release signed with either key. Releases are signed with the current key above unless the release notes say otherwise.

```text
untrusted comment: minisign public key 45B083FCB9BE29F1
RWTxKb65/IOwRZ5bqFrSPF7sR8YJANIQZYNUkjuxl99PRFJaj4XJK6g9
```

The full steps are in the bundled README.

## Specification

The specification is in [docs/](docs/README.md) and is written in Japanese.

**It is published as a work in progress.** It may change, and items marked "提案中" (proposed) are not yet decided.

The specification of a released version is the `docs/` of that version's tag (or of `main`). The version in development is the `docs/` of `develop`.

- Entry point: [docs/README.md](docs/README.md)
- Decision records (ADR) and open questions (OQ): [docs/10-decisions.md](docs/10-decisions.md)
- Roadmap: [docs/08-roadmap.md](docs/08-roadmap.md)

## Branches

- `main`: releases only. Only released versions are merged here.
- `develop`: the main line of development. Changes come in from work branches through pull requests.

See §9.1 of [docs/11-dev-workflow.md](docs/11-dev-workflow.md) for details.

## Contributing

**Pull requests from outside the project are not accepted yet.** Bug reports and feedback are welcome in Issues.

Do not report vulnerabilities in public issues; use the contact in [SECURITY.md](SECURITY.md).

The contribution policy is in [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[Apache License 2.0](LICENSE). See [NOTICE](NOTICE) for attributions.

---

The Japanese version ([README.ja.md](README.ja.md)) is authoritative. If the two differ, the Japanese version applies.
