# Mercator — Project Topography Tool

> "Cartography for your local development landscape"

This file is for Claude Code agents picking up the codebase. The README is the user-facing pitch; this is the operator's manual.

## Quickstart

If you just want to run it locally and see what it does:

```bash
cargo run --release -- survey ~/Desktop/code
cargo run --release -- serve
# open http://127.0.0.1:3000
```

`survey` creates `mercator.db` (SQLite) + `mercator_map.json` (legacy snapshot) in the working directory. `serve` reads from the DB.

## Read this first

- **[docs/STATUS.md](docs/STATUS.md)** — current state, what just shipped, where we're heading. The most useful single doc when picking the project up after time away.
- **[GOALS.md](GOALS.md)** — long-term direction (Phase 1/2/3).
- **[docs/decisions/](docs/decisions/)** — ADRs for the non-obvious design calls. If you're about to revisit one of these decisions, read the ADR first; it captures alternatives we already weighed.
- **[Project board](https://github.com/users/zot24/projects/12)** — what's queued.

## What it does

Mercator is a Rust CLI + single-file web dashboard. It surveys local directories, GitHub, GitLab, and an Obsidian vault, deduplicates the result, auto-tags, and serves it on a localhost port. See [`README.md`](README.md) for the full feature list.

### Enrichment (#8) — the second extension axis

`Source` (in `sources.rs`) discovers **projects** (`fetch → Vec<Project>`). **Enrichment** (`enrichment.rs`) is the orthogonal concern of attaching **per-project child data** to projects that already exist — data that can't ride `Source`. Two providers ship today:

- **GitHub issues** (`github.rs`) → the dashboard's read-only **KANBAN** view (`/api/issues`). Lanes are derived locally from issue state + a `status:` label convention.
- **Vercel deploy status** (`vercel.rs`) → colored **deploy badges** on project rows/cards (`/api/deployments`).

Providers are enum-dispatched via `AnyEnrichment` exactly like `AnySource` (ADR 0003). Child data lands in schema-v6 tables keyed by `remote_url` (`github_issues`, `github_remotes`, `vercel_deployments`), with per-provider sync status in `enrichment_state`. **Adding a backend (Supabase/Turso/…) = one struct + one `impl Enrichment` + one `AnyEnrichment` variant + one table.** Enrichment runs after project upsert at both dispatch sites (`survey`, `POST /api/survey/refresh`) and on-demand via `mercator enrich`; each provider opens its own short-lived write connection (WAL + `busy_timeout`) so network I/O never holds the shared DB lock.

## Status

v0.1.x. Single-user. Breaking changes are likely. Phase 1 of the [Goals doc](GOALS.md) is essentially complete as of 2026-05-04 — see [docs/STATUS.md](docs/STATUS.md) for the precise picture.

## Commands

```bash
# Build (default: no agent runner, no swarm dep needed)
cargo build --release

# Local survey (creates mercator.db + mercator_map.json)
cargo run -- survey ~/Desktop/code

# With remote sources
cargo run -- survey ~/Desktop/code --github zot24 --gitlab zot24

# With Obsidian vault
cargo run -- survey ~/Desktop/code --obsidian ~/Desktop/brain

# Watch mode — re-survey every N minutes
cargo run -- survey ~/Desktop/code --watch 5

# Enrich the surveyed projects with GitHub issues + Vercel deploy status (#8)
# without re-surveying. Providers run when their token is configured
# (GitHub via --github-token/GITHUB_TOKEN/config; Vercel via config.toml).
# Survey/refresh also run enrichment automatically.
cargo run -- enrich --github-token ghp_xxx
# Scope issues to repos you own/administer (drop push-only collaborations).
# Persist as `[github] owned_only = true` (or `owners = [...]`) in config.toml.
cargo run -- enrich --github-token ghp_xxx --owned-only

# Dashboard (reads from mercator.db; falls back to map.json on DB error)
# Views: LIST / BLOCKS / GRAPH / KANBAN (GitHub issues). Vercel deploy
# badges appear on project rows/cards in LIST + BLOCKS.
cargo run -- serve --port 3000

# CLI list / search (closes the loop on #25)
cargo run -- list --type Git
cargo run -- list --tech Rust
cargo run -- search 'cli-tool'

# list --format text is TTY-aware: an aligned, coloured table
# (TYPE / NAME / SYNC / TECH / PATH) when run interactively, and the
# stable tab-separated rows when piped (so awk/cut/grep keep working).
# NO_COLOR=1 disables the ANSI styling.
cargo run -- list                            # pretty table in a terminal
cargo run -- list --no-git                   # only non-repos (Folder/Idea)
cargo run -- list --no-remote                # repos/folders with no git remote
cargo run -- list --out-of-sync              # branch ahead/behind its upstream

# Mark a project as "currently working on" — auto-writes
# active-projects.json next to the DB for Hermes to load each session.
cargo run -- active add ~/Desktop/code/mercator --note "shipping active export"
cargo run -- active list                     # tab-separated, recent first
cargo run -- active list --format json       # enriched payload with project metadata
cargo run -- list --active                   # filter the project list to active only
cargo run -- active remove ~/Desktop/code/mercator
cargo run -- active export                   # rewrite the JSON snapshot from current state

# Generate a Markdown "projects" section (active set by default) for a profile
# README, spliced between <!-- MERCATOR:START --> / <!-- MERCATOR:END --> markers.
cargo run -- readme                              # print the block to stdout (table)
cargo run -- readme --inject ~/me/README.md      # update a profile README in place
cargo run -- readme --all --limit 10 --no-badge  # every project, capped, no badge
cargo run -- readme --list                       # bullet list w/ per-project tech emoji
cargo run -- readme --public-only                # only verifiably-public repos (network)
cargo run -- readme --list --public-only --title "🚀 Currently Building" --inject README.md
```

The `-d/--db` flag is on every subcommand and defaults to `mercator.db`.

## Architecture

```
mercator/
├── src/
│   ├── main.rs           # CLI parsing, HTTP handlers, AppState, route registration
│   ├── db.rs             # SQLite schema, migrations, CRUD, FTS5 search/list
│   ├── project.rs        # `Project` struct + JSON load/save (legacy snapshot)
│   ├── sources.rs        # FS survey, GitHub/GitLab fetchers, Obsidian, dedup, Source trait
│   ├── enrichment.rs     # Enrichment plug-point: trait + AnyEnrichment dispatch + run_all (#8)
│   ├── github.rs         # GitHub-issues enrichment provider (kanban ingestion)
│   ├── vercel.rs         # Vercel deploy-status enrichment provider
│   ├── markdown.rs       # description extraction, frontmatter, export rendering
│   ├── tags_graph.rs     # auto-tagging + D3 graph computation
│   ├── skills.rs         # skills inventory walker
│   ├── config.rs         # ~/.config/mercator/config.toml (GitHub/GitLab/Vercel tokens)
│   └── agent.rs          # swarm-feature agent runner (cfg-gated)
├── dist/index.html       # single-file dashboard (~2.2k lines); LIST/BLOCKS/GRAPH/KANBAN views
├── docs/
│   ├── STATUS.md         # current state + roadmap
│   └── decisions/        # ADRs
├── .github/workflows/ci.yml
├── Cargo.toml            # `swarm` feature flag (path-dep added manually)
├── Dockerfile            # alpine + musl
├── mercator.db           # generated; SQLite source of truth
├── mercator_map.json     # generated; legacy snapshot, fallback only
└── active-projects.json  # generated by `mercator active *`; Hermes session context
```

### Tech stack

- **Rust** 2021 edition, Tokio async runtime
- **Axum 0.8** for HTTP
- **Clap 4** (derive) for CLI
- **rusqlite 0.32** with `bundled` feature — SQLite + FTS5, no system dep
- **Reqwest** for GitHub/GitLab APIs
- **Walkdir** for filesystem traversal
- **chrono** for timestamps (used carefully — see [ADR 0001](docs/decisions/0001-sqlite-staged-migration.md) for the `%f` format gotcha)
- **Tailwind (CDN) + JetBrains Mono** in the dashboard
- **D3 v7** for the graph view
- **marked.js** for in-app markdown rendering

## Dev workflow

```bash
# Format
cargo fmt

# Lint (CI runs this with -D warnings)
cargo clippy --all-targets --no-deps -- -D warnings

# Test (97 tests as of 2026-05-04)
cargo test

# Local-only: enable the in-dashboard agent runner
# Step 1 — add the path dep manually to [dependencies] in Cargo.toml:
#   swarm = { path = "../swarm" }
# Step 2 — build with the feature on:
cargo build --features swarm
```

## CI

`.github/workflows/ci.yml` runs three required jobs on every PR to `master`:

1. **rustfmt** — `cargo fmt --all --check`
2. **clippy** — `cargo clippy --all-targets --no-deps -- -D warnings`
3. **build & test** — `cargo build && cargo test`

All three must be green before merge.

## Code style

- `cargo fmt` is enforced by CI; don't fight rustfmt
- `cargo clippy -D warnings` is enforced by CI; fix or `#[allow]` with a reason
- Async fns live in handler functions, not in module roots
- Every struct that crosses the JSON boundary derives `Serialize` / `Deserialize`
- Path-traversal-sensitive endpoints (`/api/project/file`) canonicalize both root and target before comparison
- **Tests before code, especially for refactors.** See [ADR 0004](docs/decisions/0004-tdd-discipline.md). Two real bugs were caught this way; the discipline pays for itself fast.

## Adding a module

`main.rs` was a 3604-line monolith before [#11](https://github.com/zot24/mercator/issues/11) split it. The split landed in waves; the structure now is the seven modules above. Don't grow `main.rs`. New domains belong in their own module — model the existing ones (`project.rs`, `sources.rs`, `db.rs`) for the convention:

- One `//!` docstring at the top explaining what the module owns and what it doesn't.
- `pub` only what's needed across module boundaries.
- Tests in the same file under `#[cfg(test)] mod tests` (small modules) or in `src/main.rs::tests` (when the test pulls in too many other modules' helpers).

## Known sharp edges

1. **`Cargo.toml` has no `swarm` dep declared by default.** Feature `swarm` is just a flag — adding `--features swarm` without manually adding the path dep will fail to build. Intentional until [#21](https://github.com/zot24/mercator/issues/21) lands.
2. **`SourceError` (for the `Source` trait) still has only `Generic(String)`.** The structured Network/Api/Parse split #8 called for landed in the new **`EnrichmentError`** (`enrichment.rs`) instead — the enrichment providers discriminate; the legacy source fetchers still `eprintln!`. Aligning `SourceError` to the same taxonomy is a follow-up.
3. ~~Settings panel UI writes tokens to localStorage~~ — **closed by [#2](https://github.com/zot24/mercator/issues/2)**. Tokens now live in `~/.config/mercator/config.toml` (mode 0600); the dashboard's settings panel reads `GET /api/settings` for the redacted shape and writes via `POST /api/settings`. Legacy `mercator-settings` localStorage rows are migrated on first settings open.
4. **`osascript` Terminal launcher is macOS-only**. Closed as wontfix-for-now ([#17](https://github.com/zot24/mercator/issues/17)).
5. **Dashboard runs at `127.0.0.1` by default; Docker too**. Expose with `-b 0.0.0.0` *and* `MERCATOR_TOKEN`.
6. **`agent_jobs` table is not in the schema yet.** Swarm-feature agent runner keeps state in process memory; restart loses the job list. Stage 5 of the SQLite work, not yet scheduled.

## Why Rust / Axum / SQLite

- **Rust** — single binary, fast startup, memory safe, async via Tokio. The dashboard tab opening in <1s matters for a tool you live in.
- **Axum** — minimal, ergonomic, plays well with Tower middleware. The `MERCATOR_TOKEN` middleware slotted in cleanly.
- **rusqlite + FTS5** — the migration off `mercator_map.json` was the single biggest unblock for [#25](https://github.com/zot24/mercator/issues/25), [#1](https://github.com/zot24/mercator/issues/1), [#7](https://github.com/zot24/mercator/issues/7), [#10](https://github.com/zot24/mercator/issues/10), [#15](https://github.com/zot24/mercator/issues/15). FTS5 ships with SQLite, the `bundled` feature compiles SQLite into the binary so users don't need a system install. See [ADR 0002](docs/decisions/0002-fts5-default-mode-and-token-quoting.md).
- **Walkdir** — depth control + skip-current-dir is enough; no need for `notify`/`watch` until [#15](https://github.com/zot24/mercator/issues/15) becomes real-time.

---

*Last updated: 2026-07-08 — the **enrichment plug-point** landed the first slice of #8: an `Enrichment` trait + `AnyEnrichment` dispatch (`enrichment.rs`) for per-project child data, with two providers — GitHub issues (`github.rs`) feeding a read-only KANBAN view, and Vercel deploy status (`vercel.rs`) feeding deploy badges. New schema v6 tables (`github_issues`, `github_remotes`, `vercel_deployments`, `enrichment_state`), endpoints `/api/issues` + `/api/deployments`, a `[vercel]` config block, and a `mercator enrich` command. Previous: 2026-06-19 — TTY-aware `mercator list` table + `--no-git`/`--no-remote`/`--out-of-sync` filters (schema v4 ahead/behind counts). See [docs/STATUS.md](docs/STATUS.md) for the live snapshot.*
