# Mercator — Project Topography Tool

> "Cartography for your local development landscape"

This file is for Claude Code agents picking up the codebase. The README is the user-facing pitch; this is the operator's manual.

## What it does

Mercator is a Rust CLI + single-file web dashboard. It surveys local directories, GitHub, GitLab, and an Obsidian vault, deduplicates the result, auto-tags, and serves it on a localhost port. See [`README.md`](README.md) for the full feature list.

## Status

v0.1.x. Single-user. Breaking changes are likely. The [Goals doc](GOALS.md) (Phase 1/2/3 split) and the [project board](https://github.com/users/zot24/projects/12) are authoritative for what's queued.

## Commands

```bash
# Build (default: no agent runner, no swarm dep needed)
cargo build --release

# Local survey
cargo run -- survey ~/Desktop/code

# With remote sources
cargo run -- survey ~/Desktop/code --github zot24 --gitlab zot24

# With Obsidian vault
cargo run -- survey ~/Desktop/code --obsidian ~/Desktop/brain

# Watch mode — re-survey every N minutes
cargo run -- survey ~/Desktop/code --watch 5

# Dashboard
cargo run -- serve --port 3000
```

## Architecture

```
mercator/
├── src/main.rs              # CLI + Axum server + survey + UI handlers (~2k lines, see #11)
├── dist/index.html          # Single-file dashboard (~1.7k lines)
├── .github/workflows/ci.yml # rustfmt + clippy + test, gates PRs to master
├── Cargo.toml               # `swarm` feature is opt-in; default build skips agent runner
├── Dockerfile               # alpine + musl; serves on 0.0.0.0 (see #6)
├── mercator.toml.example    # documents a config surface that isn't wired yet (see #13)
├── mercator_map.json        # generated; the entire map lives here as JSON (see #24)
└── mercator_purged.json     # generated; sticky purge blocklist
```

### Tech stack

- **Rust** 2021 edition, Tokio async runtime
- **Axum 0.8** for HTTP
- **Clap 4** (derive) for CLI
- **Reqwest** for GitHub/GitLab APIs
- **Walkdir** for filesystem traversal
- **Tailwind (CDN) + JetBrains Mono** in the dashboard
- **D3 v7** for the graph view
- **marked.js** for in-app markdown rendering

## Dev workflow

```bash
# Format
cargo fmt

# Lint (CI runs this with -D warnings)
cargo clippy --all-targets --no-deps -- -D warnings

# Test
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

All three must be green before merge (configure branch protection in repo Settings).

## Code style

- `cargo fmt` is enforced by CI; don't fight rustfmt
- `cargo clippy -D warnings` is enforced by CI; fix or `#[allow]` with a reason
- Async fns live in handler functions, not in module roots
- Every struct that crosses the JSON boundary derives `Serialize` / `Deserialize`
- Path-traversal-sensitive endpoints (`/api/project/file`) canonicalize both root and target before comparison

## Project structure note

`src/main.rs` currently holds CLI parsing, FS walking, git shelling, HTTP fetching, frontmatter parsing, dedup logic, graph computation, agent orchestration (gated), skills inventory, and all route handlers. Splitting it is tracked in [#11](https://github.com/zot24/mercator/issues/11). Don't grow it; new domains belong in their own future modules.

## What's missing

The README's Roadmap section enumerates promises that don't ship yet. The [project board](https://github.com/users/zot24/projects/12) is grouped by phase:

- **P0 — Cross-cutting** (must ship before P1): docs sync, distribution, security/auth, tests
- **P1 — Make it real**: SQLite + FTS5, `mercator list`/`search`/`export`, provider trait, multi-path
- **P2 — Make it smart**: deploy integrations, semantic graph, cost visibility, LLM-wiki layer
- **P3 — Close the loop**: Swarm workflow templates, per-project guardrails, cross-project AI

When in doubt, follow the order suggested in [#18](https://github.com/zot24/mercator/issues/18) and the project board.

## Known sharp edges

1. **`Cargo.toml` has no `swarm` dep declared by default.** Feature `swarm` is just a flag — adding `--features swarm` without manually adding the path dep will fail to build. This is intentional until [#21](https://github.com/zot24/mercator/issues/21) lands.
2. **`mercator_map.json` is read on every API request and rewritten on every survey.** No locking. A `survey --watch` running alongside `serve` can cause partial reads ([#10](https://github.com/zot24/mercator/issues/10)) — handler self-heals to `[]` on parse failure but mutating endpoints can clobber. Resolved by [#24](https://github.com/zot24/mercator/issues/24).
3. **GitHub / GitLab fetches fail silently** on 4xx and rate limits ([#5](https://github.com/zot24/mercator/issues/5)). User sees an empty list.
4. **Settings panel collects tokens in localStorage but the backend never reads them** ([#2](https://github.com/zot24/mercator/issues/2)). Tokens are dead UI today.
5. **`osascript` Terminal launcher is macOS-only**. Closed as wontfix-for-now ([#17](https://github.com/zot24/mercator/issues/17)).
6. **Dashboard runs at `0.0.0.0` in Docker with no auth** ([#6](https://github.com/zot24/mercator/issues/6)). Treat the binary as localhost-only until that lands.

## Why Rust / Axum / Walkdir

- **Rust** — single binary, fast startup, memory safe, async via Tokio. The dashboard tab opening in <1s matters for a tool you live in.
- **Axum** — minimal, ergonomic, plays well with Tower middleware. The router is small enough that auth (#6) will slot in cleanly.
- **Walkdir** — depth control + skip-current-dir is enough; no need for `notify`/`watch` until [`watch`](https://github.com/zot24/mercator/issues/15) becomes real-time.

---

*Last updated: 2026-05-03 — synced with the actual feature set as of commit master + #18.*
