# Mercator

> Cartography for your local development landscape

Mercator is a Rust CLI tool and web dashboard that discovers, organizes, and visualizes all your development projects in one place. It scans local directories, GitHub, GitLab, and an Obsidian vault to build a map of your project landscape.

**Status: v0.7.5 — single-user, breaking changes still possible.** What ships today is local + GitHub + GitLab + Obsidian aggregation in SQLite + FTS5; an in-app explorer with file tree and README rendering; auto-tagging; four dashboard views (list, blocks, graph, and a read-only GitHub-issues kanban); Vercel deploy badges; a skills inventory; project purge; a "currently working on" active set; the `survey` / `enrich` / `list` / `search` / `export` / `active` / `readme` / `serve` CLI; and an opt-in Claude Code agent runner that is compiled out of default builds. Supabase / Turso status is not built. See [docs/STATUS.md](docs/STATUS.md) for the precise live state and the [project board](https://github.com/users/zot24/projects/12) for what's queued.

## Why Mercator?

I have too many projects. Local repos on disk. Public ones on GitHub. Half-built
ideas in Obsidian. MVPs deployed to Vercel that I forgot existed. Databases on
Supabase and Turso that may or may not still be alive. Branches sitting dirty
for weeks because I jumped to the next thing.

The only place all of that existed together was my head. My head leaks.

Mercator is the map I wished I had — one screen that knows about everything I'm
working on, where I left off, and what's quietly rotting.

### What it actually does for me

**Stops me from losing projects.** Every repo, idea, and deployment lives in
one view. Nothing falls off the radar just because I haven't opened it in a
month.

**Cuts the context-switch tax.** Coming back to a project after two weeks used
to mean fifteen minutes of archaeology — `git status`, scroll through commits,
find the README, check the Obsidian note, remember what the deploy looked like.
The deep-dive page collapses that into thirty seconds.

**Shows me my own patterns.** Fifty projects laid out together tell you things
you can't see one at a time. Which stacks I actually ship with. Which ideas I
keep circling back to. Which "MVPs" have been "almost done" for six months and
are really just zombies eating mental space.

**Catches silent decay.** Dirty repos sitting for three weeks. Deploys that
quietly broke. Free tiers creeping toward the limit. Nothing alerts me about
these today — I find out when something fails. Mercator surfaces them on the
same screen as everything else, so they're hard to ignore.

**Tells me where to point work.** `mercator list --out-of-sync`, the
`ROTTING` filter, and the active set answer "what needs me today" from the
map itself. Mercator does not run the work: dispatching agents, proving
results, and recording decisions happen in a separate ops loop outside this
repo. The join between the two is the files Mercator exports; nothing reads
them yet.

**Doesn't trap my data.** Everything exports to plain markdown. If Mercator
dies tomorrow, I still walk away with a folder of structured notes on every
project I've ever touched. Most dashboards lock your data in. This one hands
it back.

### Who this is for

Probably not you if you have five projects. GitHub and a working brain do fine
at that scale.

Definitely you if you're an indie dev / solo builder with twenty-plus things
across local directories, GitHub, Obsidian, and a handful of deploy targets,
and you're actively trying to ship more — not less. The cognitive overhead of
keeping track of everything is a real tax on throughput. Mercator pays it for
you.

### What Mercator is, and is not

Mercator is the map: one SQLite file plus a dashboard that knows every
project, where each one was left, and which ones are decaying. Two things
sit next to it and are not this repo:

- **A wiki.** `mercator export` writes one markdown note per project so a
  knowledge base (Obsidian, an LLM-maintained wiki) can synthesize what each
  project means and what is blocking it. The join is the exported files
  ([#22](https://github.com/zot24/mercator/issues/22)).
- **The doing.** Agents that build, verify, and record decisions run in a
  separate ops loop. Mercator does not dispatch them, own workflows, or set
  guardrails; the Phase 3 that once planned that here is retired
  ([GOALS.md](GOALS.md)).

Mercator earns its place the day it makes your existing project sprawl
manageable. It does not need the wiki or the loop to do that.

## What ships today

**Persistence**
- SQLite + FTS5 source of truth (`mercator.db`); the legacy `mercator_map.json` is still written as a backup snapshot but no longer the source of truth
- Auto-import migration from `mercator_map.json` + `mercator_purged.json` if you're upgrading from v0.1.x JSON-only — runs once on first start, idempotent
- Per-row sync from survey through to FTS5 index, atomic-tx purges, foreign-key cascades

**Discovery**
- Local project scanning — Git repos, `IDEA.md` folders, top-level directories
- Git metadata — branch, last commit, dirty/uncommitted-files detection (click the warning to see changed files)
- Tech stack detection — Node.js, Rust, Python, Go, Docker, Ruby, Java, PHP, Elixir, …
- GitHub / GitLab integration — paginated API (5000/hr authenticated, 60/hr otherwise); set `GITHUB_TOKEN` / `GITLAB_TOKEN` for private repos
- Obsidian vault scan — pulls `Projects/` notes and the `@Projects.md` idea list, links them to matching repos by name
- AI agent detection — identifies projects using Claude Code (`CLAUDE.md`, `.claude/`) or Codex (`AGENTS.md`, `.codex/`)
- Deduplication — local Git repos merge with their GitHub/GitLab counterparts via remote URL or fallback name match
- Pluggable source trait — adding new providers is one struct + one `impl Source`; no fork of the survey loop
- Pluggable enrichment trait (#8) — per-project child data. **GitHub issues** power a read-only **kanban** view; **Vercel deploy status** shows as badges on every project. Adding a backend (Supabase/Turso next) is one struct + one `impl Enrichment` + one table

**CLI access**
- `mercator list [--type T] [--tag T] [--tech T] [--active] [--no-git] [--no-remote] [--out-of-sync] [--format text|json]` — filter projects; an aligned table on a terminal, tab-separated rows when piped
- `mercator search <query>` — full-text search via SQLite FTS5 (name + description + tags), AND across whitespace tokens, hyphens-in-words are literal
- `mercator export <out_dir>` — one markdown file per project, suitable for an Obsidian wiki or any other consumer
- `mercator active add|remove|list|export` — the "currently working on" set, mirrored to `active-projects.json` for session-loaders
- `mercator enrich` — pull GitHub issues and Vercel deploy status for the surveyed projects without re-surveying
- `mercator readme` — render the active set as a Markdown block and splice it into a profile README

**Organisation**
- Auto-tagging into 15 categories (`ai`, `web`, `api`, `cli`, `devops`, `mobile`, `data`, `blockchain`, `seo`, `auth`, `bot`, `automation`, `game`, `docs`, `finance`)
- Favorites (per-browser, persisted in `localStorage`)
- Purge — remove a project from the map; persisted in the `purged` table so future surveys keep it gone (`mercator_purged.json` is read only during the one-time first-run import)
- Smarter description extraction — reads `IDEA.md` → `README.md` → `CLAUDE.md` → `AGENTS.md`, strips frontmatter / badges / callouts, joins the first prose paragraph

**Visualisation**
- Four views: list, blocks (tile grid), graph (D3 force-directed), and a read-only KANBAN of GitHub issues (lanes from issue state + a `status:` label convention)
- Vercel deploy badges on rows and tiles; a per-project ISSUES tab in the explorer
- Graph edges from name-mention, shared keywords, shared tags, and idea-↔-implementation links
- Sidebar filters: type, dirty, stale (≥21 days idle), rotting (stale + dirty), favorites, dynamic categories
- Real-time search, sort by name or last modified

**In-app explorer**
- Click any local project for a 3-pane preview: project list, file tree, viewer
- Smart auto-open — dirty repos open the most-recently-modified uncommitted file; clean repos open the freshest file under `src/`/`app/`/`lib/`; README is the fallback
- Header banner with branch, last commit, days-since-modified, dirty/stale badges
- Markdown rendering for `.md` / `.mdx`; everything else as monospaced text
- Arrow keys + filter input swap projects without leaving the modal
- IDE / Claude Code / Codex launch buttons per project

**Skills inventory**
- Walks `~/.claude/skills/` and every project's `.claude/skills/` plus the plugin marketplace cache
- Groups by inferred prefix (e.g. all `gsd-*` collapse into one) or by marketplace name
- Detects drift between project copies and the global copy via content hash (synced / diverged / no-global)
- Repo links surfaced from `known_marketplaces.json` and from `repository:` in skill frontmatter

**Agent runner** (opt-in, requires the `swarm` feature flag and a local `../swarm` checkout; not in any release)
- Launch a Claude Code task per project with prompt + model + permission mode + budget
- Live job list with cost / tool-call / token counters
- Default builds compile it out. The dashboard still draws the RUN / LAUNCH / AGENTS controls, and those routes answer 404 without the feature ([#21](https://github.com/zot24/mercator/issues/21) tracks that gap; no distribution of `swarm` is planned)

## Install

### Homebrew (macOS / Linux)

```bash
brew install zot24/tap/mercator
```

This builds from source (needs the Rust toolchain, which Homebrew pulls in as a
build dependency). Upgrade to the newest release any time with:

```bash
brew upgrade mercator
```

Every published release auto-updates the formula in [`zot24/homebrew-tap`](https://github.com/zot24/homebrew-tap),
so `brew upgrade` always tracks the latest. Check what you're on with
`mercator --version`.

### From source

```bash
cargo install --git https://github.com/zot24/mercator
# or clone + `cargo build --release` (binary at ./target/release/mercator)
```

## Quick Start

```bash
# Scan your projects
mercator survey ~/code --github yourusername

# Start the dashboard
mercator serve --port 3000

# Open http://127.0.0.1:3000
```

## CLI Reference

### `mercator survey <path>`

Scan a directory for projects, upsert the result into SQLite, and write a JSON snapshot for backup.

```bash
mercator survey ~/code                                    # Local only
mercator survey ~/code ~/work/repos ~/oss                 # Multiple roots in one run
mercator survey ~/code --github zot24                     # + GitHub public repos (60/hr cap)
mercator survey ~/code --github zot24 \
  --github-token ghp_xxx                                  # + private repos, 5000/hr cap
GITHUB_TOKEN=ghp_xxx mercator survey ~/code --github zot24  # Same via env
mercator survey ~/code --gitlab myuser                    # + GitLab repos
mercator survey ~/code --github zot24 --max-repos 1000    # Cap fetched repos
mercator survey ~/code --github zot24 -w 5                # Re-scan every 5 minutes
mercator survey ~/code -d ~/.mercator/main.db             # Custom DB path
mercator survey ~/code --obsidian ~/Desktop/brain         # + Obsidian vault (Projects/ folder)
```

| Flag | Description |
|------|-------------|
| `--github <user>` | Fetch repos from GitHub |
| `--github-token <token>` | GitHub PAT (also reads `GITHUB_TOKEN` env). Required for private repos; raises rate limit from 60/hr to 5000/hr. |
| `--gitlab <user>` | Fetch repos from GitLab |
| `--gitlab-token <token>` | GitLab PAT (also reads `GITLAB_TOKEN` env) |
| `--max-repos <n>` | Cap the number of repos fetched per remote source (default: no cap, paginates until done) |
| `-o, --output <file>` | Output JSON snapshot (default: `mercator_map.json`). Snapshot only — the DB is the source of truth. |
| `-d, --db <file>` | SQLite DB file (default: `mercator.db`). Created if missing; migrated to schema v6 on first open. |
| `-w, --watch <minutes>` | Re-scan every N minutes (keeps running) |
| `--obsidian <vault>` | Also scan an Obsidian vault: subfolders and notes under its projects folder become `Obsidian` projects, linked to repos by name |
| `--obsidian-folder <name>` | Projects folder inside the vault (default: `Projects`) |
| `--obsidian-vault <name>` | Vault name used in `obsidian://` URIs (default: the vault directory's name) |
| `--obsidian-sync` | Run `ob sync` (obsidian-headless) before scanning, for Docker/remote setups |

### `mercator list`

Filter projects by type, tag, tech-stack entry, or attention state. On a terminal the output is an aligned, lightly coloured table (`TYPE / NAME / SYNC / TECH / PATH`, `NO_COLOR` respected); when piped it is one project per line, tab-separated columns (type, path, name, tags, tech) — designed for `awk`/`cut`/`grep`. All filters AND together.

```bash
mercator list                          # all projects
mercator list --type Git               # only Git-classified
mercator list --tech Rust              # projects whose tech-stack contains "Rust"
mercator list --tag cli --tech Rust    # AND across filters
mercator list --active                 # only the "currently working on" set
mercator list --no-git                 # Folder / Idea: directories not under version control
mercator list --no-remote              # nothing pushed anywhere (no origin)
mercator list --out-of-sync            # branch ahead and/or behind its upstream (as of last fetch)
mercator list --format json | jq '.[].name'
```

| Flag | Description |
|------|-------------|
| `-d, --db <file>` | SQLite DB file (default: `mercator.db`) |
| `-t, --type <type>` | Filter by project type (`Git`, `Folder`, `Idea`, `GitHub`, `GitLab`, `Obsidian`) |
| `--tag <tag>` | Filter by exact tag (case-sensitive) |
| `--tech <tech>` | Filter by tech-stack entry |
| `--active` | Only projects on the active list (see `mercator active`) |
| `--no-git` | Only projects that are not git repositories (`Folder` / `Idea`) |
| `--no-remote` | Only projects with no git remote configured (plain folders and remote-less repos) |
| `--out-of-sync` | Only git projects whose branch is ahead and/or behind its upstream. `survey` does not fetch, so this reflects the last fetch |
| `--format <text\|json>` | `text` (default): table on a TTY, tab-separated rows when piped. `json`: full project records, the `/api/map` shape |

### `mercator search <query>`

Full-text search across name, description, and tags. Backed by SQLite FTS5. Each whitespace-separated token must match (AND); punctuation inside a word is literal so `cli-tool` finds the project named `cli-tool`. (Tech stack isn't FTS-indexed — use `mercator list --tech` for that.)

```bash
mercator search agent                  # name/description/tag contains "agent"
mercator search 'cli-tool'             # hyphenated names work as expected
mercator search 'rust web'             # AND across both tokens
```

| Flag | Description |
|------|-------------|
| `<query>` | FTS5 query (positional, required) |
| `-d, --db <file>` | SQLite DB file (default: `mercator.db`) |
| `--format <text\|json>` | `text` (default, tab-separated) or `json` (full project records, the `/api/map` shape) |

### `mercator export <out_dir>`

Write one markdown file per project. The output is a folder of structured notes that any other tool (Obsidian, Logseq, grep, ripgrep, an LLM) can consume directly. Reads from SQLite — so dashboard purges since the last `mercator survey` are honored.

```bash
mercator export ./notes                                         # → ./notes/<project>.md
mercator export --obsidian-vault ~/Desktop/brain                # → ~/Desktop/brain/Projects/<project>.md
mercator export --obsidian-vault ~/Desktop/brain \
  --obsidian-folder Mercator                                    # custom subdir
mercator export ./notes -d ~/.mercator/main.db                  # use a different DB
```

Each note has YAML frontmatter (`name`, `type`, `path`, `branch`, `status`, `last_modified`, `remote`, `agent`, `tech`, `tags`, `obsidian`) plus a body with status, links, and a tag/stack footer. Filenames are sanitised; collisions append ` (N)`.

| Flag | Description |
|------|-------------|
| `<out_dir>` | Output directory (default: `./mercator-export`). Created if missing. |
| `-d, --db <file>` | SQLite DB file (default: `mercator.db`) |
| `--obsidian-vault <path>` | Write under `<vault>/<folder>/` instead of `out_dir` |
| `--obsidian-folder <name>` | Subdirectory inside the vault (default: `Projects`) |

### `mercator active <add|remove|list|export>`

Manage the "currently working on" set. It is orthogonal to surveyed state: it survives re-surveys, and a path can be activated before it is surveyed. Every mutation rewrites `active-projects.json` next to the DB so a session-loader (Hermes, or any agent) can pick up the current focus without opening the DB.

```bash
mercator active add ~/code/mercator --note "shipping the docs sweep"
mercator active list                     # tab-separated: path, activated_at, note; most recent first
mercator active list --format json       # joins in description / type / tech / tags when surveyed
mercator active remove ~/code/mercator
mercator active export                   # rewrite active-projects.json after a re-survey
mercator list --active                   # the project list filtered to the active set
```

| Flag | Description |
|------|-------------|
| `add <path> [-n, --note <text>]` | Mark a path active. Re-adding refreshes the timestamp and replaces the note |
| `remove <path>` | Drop a path from the set (no-op with a warning if it is not there) |
| `list [--format text\|json]` | Print the set; `json` joins in project metadata when the path is surveyed |
| `export` | Rewrite the JSON snapshot from the current DB state without changing the set |
| `-d, --db <file>` | SQLite DB file (default: `mercator.db`) |
| `--export <path>` | Where the snapshot is written (default: `active-projects.json` next to the DB). `-` or any path containing `/dev/null` suppresses it |

### `mercator enrich`

Attach per-project child data to the projects already in the DB without re-surveying: GitHub issues (the dashboard's KANBAN view and the explorer's ISSUES tab) and the latest Vercel deployment per project (deploy badges). A provider runs only when its token is configured. `survey` and the dashboard's refresh button run the same providers automatically after upserting projects.

```bash
mercator enrich --github-token ghp_xxx                 # issues for every repo the token can push to
mercator enrich --github-token ghp_xxx --owned-only    # only repos you own/administer
GITHUB_TOKEN=$(gh auth token) mercator enrich          # same via env; Vercel needs config.toml
```

| Flag | Description |
|------|-------------|
| `--github-token <token>` | GitHub PAT (falls back to `GITHUB_TOKEN`, then `[github] token` in `config.toml`). Repos are pre-checked for push access and enabled issues |
| `--owned-only` | Only ingest issues from repos you own/administer, dropping push-only collaborations. Overrides `[github] owned_only` |
| `-d, --db <file>` | SQLite DB file (default: `mercator.db`) |

Issues land in `github_issues` / `github_remotes` and deployments in `vercel_deployments`, keyed by the project's git remote; each provider records its last run in `enrichment_state`. Vercel has no CLI flag: set `[vercel] token` (and `user` to a team id for team accounts) in `~/.config/mercator/config.toml`.

### `mercator readme`

Render a Markdown "projects" section (the active set by default) for a profile README, and optionally splice it into a file between `<!-- MERCATOR:START -->` / `<!-- MERCATOR:END -->` markers. Everything outside the markers is left alone; without `--inject` the block is printed to stdout.

```bash
mercator readme                                   # table to stdout
mercator readme --inject ~/me/README.md           # update in place (markers appended if absent)
mercator readme --all --limit 10 --no-badge       # every project, capped, no footer
mercator readme --list --public-only --title "🚀 Currently Building" --inject README.md
```

| Flag | Description |
|------|-------------|
| `--inject <file>` | Update this file in place; a missing file is created |
| `--all` | Every project instead of just the active set |
| `-t, --type`, `--tag`, `--tech` | The same filters as `mercator list` |
| `--limit <n>` | Cap the number of projects rendered |
| `--title <text>` | Section heading (default: `🛠️ What I'm working on`) |
| `--no-badge` | Omit the "mapped by mercator" footer |
| `--public-only` | Keep only verifiably public repos. Uses the GitHub API when `GITHUB_TOKEN` is set, else an unauthenticated web check; private, remote-less, and unverifiable repos are dropped. Needs network |
| `--list` | Bullet list (`- <emoji> **[name](url)** — description`) instead of a table |
| `--no-emoji` | In list layout, omit the per-project tech emoji |
| `-d, --db <file>` | SQLite DB file (default: `mercator.db`) |

### `mercator serve`

Start the web dashboard. Every `/api/*` endpoint reads and writes the SQLite DB, including `/api/skills`. On start, `serve` imports `mercator_map.json` into the DB **only if the `projects` table is empty** (the first-run upgrade path); a populated DB is never overwritten by the snapshot. After that the JSON is read only as a fallback if a DB read errors.

```bash
mercator serve                          # http://127.0.0.1:3000
mercator serve -p 8080                  # Custom port
mercator serve -b 0.0.0.0               # Listen on all interfaces
mercator serve -d ~/.mercator/main.db   # Custom DB
mercator serve --refresh ~/code         # Refresh button re-scans this path in-process
```

| Flag | Description |
|------|-------------|
| `-p, --port <port>` | Port to listen on (default: 3000) |
| `-b, --bind <ip>` | Bind address (default: 127.0.0.1) |
| `-m, --map-file <file>` | Legacy JSON snapshot path (default: `mercator_map.json`). Imported into the DB only when the DB has no projects (first run); otherwise read only as a fallback if a DB read fails. |
| `-d, --db <file>` | SQLite DB file (default: `mercator.db`). Source of truth for every read and write. |
| `--refresh <path>` | Local path the dashboard's refresh button re-scans. Repeat for multiple roots: `serve --refresh ~/code --refresh ~/oss`. Without this, the refresh button just reloads the page. |

Without `--refresh`, the dashboard sees new projects only after a fresh `mercator survey ...`. With `--refresh`, the in-dashboard refresh button re-scans those paths and upserts directly into the live DB — faster for ad-hoc local changes. In the same request it re-fetches GitHub / GitLab when `config.toml` names a user, and then runs enrichment (issues, Vercel deploys) when the tokens are configured. The Obsidian vault is not re-scanned on refresh; use `mercator survey --obsidian ...` for that.

#### Optional: token config

The dashboard's refresh button can fetch GitHub/GitLab, and `survey` / `enrich` / refresh can pull issues and Vercel deploy status, if you drop a config file at `~/.config/mercator/config.toml`:

```toml
[github]
user = "zot24"
token = "ghp_xxxxx"          # optional — public repos work without it (60/hr cap)
owned_only = true            # optional — issues only from repos you own/administer
owners = ["zot24", "motty"]  # optional — restrict issue ingestion to these owners

[gitlab]
user = "zot24"
token = "glpat-xxxxx"        # optional

[vercel]
token = "vercel_xxxxx"       # deploy-status badges
user = "team_xxxxx"          # optional teamId for team-scoped accounts
```

The file is read once at `mercator serve` startup and stored in memory; chmod 0600 is applied automatically when the binary writes it (the read path doesn't enforce the mode but it's recommended). Tokens never leave the server — `GET /api/settings` returns a redacted shape (`{github_user, github_token_set, gitlab_user, gitlab_token_set, vercel_team, vercel_token_set}`) for the dashboard's "you have a token configured" hint. The dashboard's settings panel can set only the GitHub and GitLab user and token (`POST /api/settings`); the `[vercel]` block and the `owned_only` / `owners` keys are edited in the file.

## Docker

The image's default `CMD` binds to `127.0.0.1` inside the container, which is unreachable from the host. To expose the dashboard you must explicitly opt into a public bind **and** set `MERCATOR_TOKEN`:

```bash
# Build
docker build -t mercator .

# Generate an API token once
TOKEN=$(openssl rand -hex 32)

# Run with auth (mount your code directory read-only)
docker run -p 3000:3000 \
  -e MERCATOR_TOKEN=$TOKEN \
  -v ~/code:/data/code:ro \
  mercator sh -c "mercator survey /data/code -o /data/map.json && \
                  mercator serve -b 0.0.0.0 -m /data/map.json"

# With GitHub integration
docker run -p 3000:3000 \
  -e MERCATOR_TOKEN=$TOKEN \
  -v ~/code:/data/code:ro \
  mercator sh -c "mercator survey /data/code --github zot24 -o /data/map.json && \
                  mercator serve -b 0.0.0.0 -m /data/map.json"

# With watch mode (survey + serve in parallel)
docker run -p 3000:3000 \
  -e MERCATOR_TOKEN=$TOKEN \
  -v ~/code:/data/code:ro \
  mercator sh -c "mercator survey /data/code --github zot24 -o /data/map.json -w 5 & \
                  mercator serve -b 0.0.0.0 -m /data/map.json"
```

When `MERCATOR_TOKEN` is set, every `/api/*` request must include `Authorization: Bearer $TOKEN`. The dashboard HTML itself is served without auth (the API behind it is the sensitive surface), so cross-network usage requires a browser extension to inject the header — for local use, prefer ssh-tunnelling to a `127.0.0.1` bind.

## Project Types Detected

| Type | Source | Description |
|------|--------|-------------|
| **Git** | Local | Directories containing `.git` |
| **GitHub** | API | Public repos from GitHub user |
| **GitLab** | API | Public repos from GitLab user |
| **Idea** | Local | Directories with `IDEA.md` |
| **Folder** | Local | Top-level directories without Git |
| **Obsidian** | Local | Notes and folders under a vault's `Projects/` directory |

## Roadmap

The promises in *Why Mercator?* that don't ship today live as tracked issues. The honest delta:

- **"Stops me from losing projects"** — local + GitHub + GitLab + Obsidian work; **GitHub issues (kanban) and Vercel deploy status now land via the enrichment plug-point** ([#8](https://github.com/zot24/mercator/issues/8)); Supabase / Turso are next on the same seam
- **"Cuts the context-switch tax"** — file-tree explorer ships with smart auto-open: dirty repos open the most-recently-modified uncommitted file; clean repos open the freshest file under `src/`/`app/`/`lib/`; README is the fallback. Header banner shows branch, last commit, and days-since-modified.
- **"Catches silent decay"** — dirty repos and stale (≥21 days idle) surface today, plus a `ROTTING` filter for the rare project that's both. Failed/ERROR Vercel deploys now surface as red badges ([#8](https://github.com/zot24/mercator/issues/8)); Supabase/Turso quota decay is still pending
- **"Tells me where to point work"** — `list --out-of-sync` / `--active` / `--no-remote` and the `ROTTING` filter answer it from the map. Cross-project AI questioning ([#20](https://github.com/zot24/mercator/issues/20)) and the in-Mercator workflow loop are retired from this repo; see [GOALS.md](GOALS.md)
- **"Doesn't trap my data"** — `mercator export` writes one markdown file per project with frontmatter + body; `--obsidian-vault` mode targets the Obsidian wiki layer.

Everything else is in the [project board](https://github.com/users/zot24/projects/12), grouped by phase.

## Tech Stack

- **Rust** with Tokio async runtime
- **Axum** web framework
- **Clap** CLI parser
- **rusqlite** with `bundled` feature — SQLite + FTS5 compiled into the binary, no system dep
- **Reqwest** HTTP client
- **Walkdir** filesystem traversal
- **Tailwind CSS** + JetBrains Mono for the dashboard UI

## Documentation map

- **[docs/STATUS.md](docs/STATUS.md)** — current state, what just shipped, where things are heading.
- **[GOALS.md](GOALS.md)** — long-term direction (Phase 1 shipped, Phase 2 open, Phase 3 retired).
- **[CLAUDE.md](CLAUDE.md)** — operator's manual for picking up the codebase.
- **[docs/decisions/](docs/decisions/)** — ADRs for non-obvious design decisions.
- **[docs/TICKET_CONTRACT.md](docs/TICKET_CONTRACT.md)** — the `POST /api/tickets` contract (partly implemented: creation only, nothing reads `local_tickets` back yet).
