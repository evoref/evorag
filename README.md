# evorag

English | [日本語](README.ja.md)

A chat and long-term memory tool written in Rust. It runs against your own llama-server (llama.cpp) and nothing else, so conversations stay on your PC. It attaches related parts of your earlier conversations to its answers, with sources. Later versions will remember facts about you and improve from your ratings and corrections. It never trains model weights.

> **The latest release is v0.2.0 (2026-11-12).** Downloads are on [GitHub Releases](https://github.com/evoref/evorag/releases): a zip for Windows 11 (x86_64) and a tar.gz for Linux (x86_64). Installation, setup, updating and rolling back, signature verification and known limitations are described in the bundled README ([dist/README.md](dist/README.md), in Japanese).

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

The plan for each version up to v0.6 is in [docs/08-roadmap.md](docs/08-roadmap.md) (in Japanese).

The storage format may change until v0.5. Each version migrates data from the version just before it, so update one version at a time.

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
