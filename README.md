# wombat

Distill coding-agent session logs (**Codex**, **Claude Code**, **OpenCode**, **Pi**, **OMP**, **Hermes Agent**, **Aside**) into a prefix-stable plain-text mirror directory, then index and search user–assistant dialogue via the external [`minsync`](https://github.com/NomaDamas/MinSync) CLI (`>= 0.6.0`) using **SPLADE** (`KURE-Sparse-nano`), **dense embeddings** (`Qwen/Qwen3-Embedding-0.6B`), and **incremental content-defined chunking**.

Designed as an external CLI datasource for **AutoRAG**.

## Architecture

1. **Dialogue-only parsers (`src/parsers/`)** — strip system/developer prompts, injected `AGENTS.md` / `<environment_context>` / `<system-reminder>` blocks, reasoning/thinking traces, tool calls, tool outputs, and file-history snapshots at parse time:
   - **Codex** (`~/.codex/sessions`, `~/.codex/archived_sessions`): streaming `.jsonl` and zstd `.jsonl.zst` rollouts.
   - **Claude Code** (`~/.claude/projects`): project `.jsonl` transcripts (skips `isMeta`, slash-command stdout, snapshots, and `subagents/` sidechains by default).
   - **OpenCode** (`~/.local/share/opencode`): `opencode.db` v2 (`session_v2`/`session_message`), v1 (`session`/`message`/`part`), and legacy `storage/` JSON trees via WAL-safe read-only SQLite snapshots.
   - **Hermes Agent** (`~/.hermes` or `$HERMES_HOME`, plus `profiles/*`): `state.db` `sessions`/`messages` (read-only, WAL-safe) and `sessions/session_*.json` snapshots for sessions absent from any DB. Cron-run sessions, `/tmp` cwd sessions, harness `[System note:]` prompts, context-compaction summaries and failed/hidden turns are skipped.
   - **Aside browser agent** (`~/.aside`, account dirs `u/<n>`): `sessions/<date>_<id>/messages.jsonl` plus titles/triggers from `state.db` (read-only, WAL-safe). Only `user` and assistant `text` blocks are kept (steer messages included); thinking, tool calls/results, recalled memories, compaction summaries and routine-triggered assistant output are dropped. Subagent/forked-worker sessions, routine and ephemeral sessions are skipped unless `--include-subagents` / `--include-ephemeral`.
   - **Pi & OMP** (`~/.pi/agent/sessions`, `~/.omp/agent/sessions`): v3 JSONL sessions (skips ephemeral `/tmp` / `/var/folders`, `faux` automations, and nested subagent logs by default).
2. **Prefix-stable mirror (`src/mirror.rs`)** — each session renders to `<data_dir>/mirror/<agent>/<project_slug>/<session>__<hash>.md`. Volatile values (mutable titles, `ended_at`) live only in `catalog.sqlite3`, never in the mirror body, so appended turns extend files strictly by byte prefix and existing CDC chunks keep their hashes and embeddings.
3. **Incremental materialization + indexing (`src/minsync.rs`, `src/engine.rs`)** — `minsync stage` tracks per-source fingerprints so unchanged sessions are not re-parsed; `minsync sync` incrementally updates only new/modified chunks across both SPLADE and dense indexes. Every `minsync` interaction uses the external binary (`minsync init|stage|sync|query|status`); the `minsync` crate is never linked.
4. **Session & full-trace navigation (`src/catalog.rs`, `wombat show`, `wombat trace`)** — every rendered turn records its exact source pointer (`[src:L134]` for JSONL/zstd lines or `[src:id:<msg_id>]` for SQLite stores), allowing an agent to jump from any retrieved chunk straight to the surrounding turns or the raw underlying session trace (including tool calls and diffs).

## Install

```bash
cargo install wombat-sessions        # installs the `wombat` binary
# or download a prebuilt binary from https://github.com/NomaDamas/wombat/releases
```

The crates.io package is `wombat-sessions` (the `wombat` name is taken by an unrelated crate); the
executable is `wombat`.

## Requirements

- Rust `1.88+` (only to build from source)
- `minsync >= 0.6.0` on `PATH` (or `--minsync-bin <path>` / `WOMBAT_MINSYNC_BIN`)

## CLI Usage

```bash
# Check minsync >= 0.6.0 health and index freshness
wombat doctor
wombat doctor --json

# Incrementally parse sessions into ~/.wombat/mirror and index with MinSync
wombat sync --index

# Narrow to specific agents or explicit source directories
wombat sync --agent codex,claude-code
wombat --data-dir ~/.wombat --source codex=~/.codex/sessions sync --index

# Search dialogue (modes: hybrid [default], keyword/sparse [SPLADE], semantic/vector [dense])
wombat search "BM25 형태소 분석기"
wombat search keyword "1Password Chrome CSV" --limit 5
wombat search semantic "LanceDB snapshot isolation" --agent opencode --json

# Inspect sessions, clean dialogue turns, or full raw traces (with tool outputs)
wombat sessions --limit 20
wombat show codex:01a13fa4-1b1a-7c03-9fcd-cce611b321ca
wombat trace codex:01a13fa4-1b1a-7c03-9fcd-cce611b321ca --turn 6 --context 3

# AutoRAG CLI datasource chunk endpoints
wombat chunk get <chunk_id> --json
wombat chunk context <chunk_id> --window 2 --json
wombat chunk parent <chunk_id> --json
```

## Environment Variables

| Variable | Description |
| --- | --- |
| `WOMBAT_DATA_DIR` | Data directory containing `mirror/`, `stage/`, and `catalog.sqlite3` (default `~/.wombat`) |
| `WOMBAT_CONFIG` | Path to `config.toml` (default `~/.config/wombat/config.toml`) |
| `WOMBAT_MINSYNC_BIN` | Path to external `minsync` binary (`>= 0.6.0`) |
| `WOMBAT_EMBEDDER` | MinSync dense embedder id (`native:Qwen/Qwen3-Embedding-0.6B`, or `none` for SPLADE-only) |
| `WOMBAT_EMBED_BASE_URL` | Optional shared loopback embedding endpoint (`minsync serve`) |
| `WOMBAT_CHUNKER` | Chunker id (`auto` selects `line-cdc` on `minsync >= 0.7`, `cdc` on `0.6.x`) |

## Releasing

Releases are cut by pushing a version tag; `.github/workflows/release.yml` does the rest:

1. Bump `version` in `Cargo.toml` (and `Cargo.lock` via `cargo check`), merge to `main`.
2. `git tag v0.2.0 && git push origin v0.2.0`. The tag must equal the `Cargo.toml` version or the run fails.
3. The workflow runs the tests and a `cargo publish --dry-run`, builds `wombat` for Linux x86_64 and
   macOS arm64, creates the GitHub Release with the tarballs and `.sha256` files, and publishes
   `wombat-sessions` to crates.io. A version already on crates.io is skipped, so a failed run can be re-run.
   Intel macOS has no prebuilt binary; use `cargo install wombat-sessions`.

crates.io publishing uses [Trusted Publishing](https://crates.io/docs/trusted-publishing) (GitHub OIDC),
so no `CARGO_REGISTRY_TOKEN` secret is stored. One-time setup (crates.io -> `wombat-sessions` ->
Settings -> Trusted Publishing): repository `NomaDamas/wombat`, workflow `release.yml`, environment
`crates-io`. Create the `crates-io` environment in the GitHub repo settings as well.
