# Mercator

> Cartography for your local development landscape

Mercator is a Rust CLI tool and web dashboard that discovers, organizes, and visualizes all your development projects in one place. It scans local directories, GitHub, GitLab, and an Obsidian vault to build a map of your project landscape.

**Status: v0.1.x — early, single-user, breaking changes likely.** What ships today is local + GitHub + GitLab + Obsidian aggregation backed by SQLite + FTS5, an in-app explorer with file tree and README rendering, auto-tagging, a graph view, a skills inventory, project purge, `mercator list` / `mercator search` / `mercator export` CLI subcommands, and an opt-in Claude Code agent runner. What's described in *Why Mercator?* below as detecting deploy decay or cross-project AI is **roadmap, not yet shipped** — see [docs/STATUS.md](docs/STATUS.md) for the precise live state and the [project board](https://github.com/users/zot24/projects/12) for what's queued.

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

**Tells me where to point AI.** Once I trust Mercator's view of my landscape,
I can ask it — or the agents it launches — questions like *"which project
could ship this week"* or *"which idea has signal but no code yet."* That's
the bridge to actually using AI for leverage instead of just for autocomplete.

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

### The bigger picture

Mercator is the **eyes** of a three-part stack I'm building for myself:

- **Mercator** — sees my landscape (this repo)
- **An LLM-wiki layer** — understands and synthesizes what each project means,
  where it's going, what's blocking it (lives in my Obsidian vault, fed by
  Mercator's export)
- **Swarm** — executes against that understanding under guardrails, using
  Claude Code as the actual builder

Each piece is useful alone. Together they're a personal AI dev platform — the
thing I keep seeing people cobble together out of half a dozen ChatGPT tabs
and abandoned Notion pages, except this one stays maintained because the
maintenance cost is near zero.

But you don't need to buy the long thesis to use Mercator. It earns its place
the day it makes your existing project sprawl manageable.

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
- Pluggable source trait — adding new providers (Vercel/Supabase/Turso, in flight) is one struct + one `impl Source`; no fork of the survey loop

**CLI access**
- `mercator list [--type T] [--tag T] [--tech T]` — filter projects, tab-separated stdout for piping to `awk`/`cut`/`grep`
- `mercator search <query>` — full-text search via SQLite FTS5 (name + description + tags), AND across whitespace tokens, hyphens-in-words are literal
- `mercator export <out_dir>` — one markdown file per project, suitable for an Obsidian wiki or any other consumer

**Organisation**
- Auto-tagging into 15 categories (`ai`, `web`, `api`, `cli`, `devops`, `mobile`, `data`, `blockchain`, `seo`, `auth`, `bot`, `automation`, `game`, `docs`, `finance`)
- Favorites (per-browser, persisted in `localStorage`)
- Purge — remove a project from the map; persisted in `mercator_purged.json` so future surveys keep it gone
- Smarter description extraction — reads `IDEA.md` → `README.md` → `CLAUDE.md` → `AGENTS.md`, strips frontmatter / badges / callouts, joins the first prose paragraph

**Visualisation**
- Three views: list, blocks (tile grid), graph (D3 force-directed)
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

**Agent runner** (opt-in, requires the `swarm` feature flag and a local `../swarm` checkout)
- Launch a Claude Code task per project with prompt + model + permission mode + budget
- Live job list with cost / tool-call / token counters
- See [issue #21](https://github.com/zot24/mercator/issues/21) for the long-term distribution plan

## Quick Start

```bash
# Build
cargo build --release

# Scan your projects
./target/release/mercator survey ~/code --github yourusername

# Start the dashboard
./target/release/mercator serve --port 3000

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
```

| Flag | Description |
|------|-------------|
| `--github <user>` | Fetch repos from GitHub |
| `--github-token <token>` | GitHub PAT (also reads `GITHUB_TOKEN` env). Required for private repos; raises rate limit from 60/hr to 5000/hr. |
| `--gitlab <user>` | Fetch repos from GitLab |
| `--gitlab-token <token>` | GitLab PAT (also reads `GITLAB_TOKEN` env) |
| `--max-repos <n>` | Cap the number of repos fetched per remote source (default: no cap, paginates until done) |
| `-o, --output <file>` | Output JSON snapshot (default: `mercator_map.json`). Snapshot only — the DB is the source of truth. |
| `-d, --db <file>` | SQLite DB file (default: `mercator.db`). Created if missing; migrated to schema v2 on first open. |
| `-w, --watch <minutes>` | Re-scan every N minutes (keeps running) |

### `mercator list`

Filter projects by type, tag, or tech-stack entry. Output is one project per line, tab-separated columns (type, path, name, tags, tech) — designed to pipe to `awk`/`cut`/`grep`.

```bash
mercator list                          # all projects
mercator list --type Git               # only Git-classified
mercator list --tech Rust              # projects whose tech-stack contains "Rust"
mercator list --tag cli --tech Rust    # AND across filters
```

| Flag | Description |
|------|-------------|
| `-d, --db <file>` | SQLite DB file (default: `mercator.db`) |
| `-t, --type <type>` | Filter by project type (`Git`, `Folder`, `Idea`, `GitHub`, `GitLab`, `Obsidian`) |
| `--tag <tag>` | Filter by exact tag |
| `--tech <tech>` | Filter by tech-stack entry |

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

### `mercator serve`

Start the web dashboard. All `/api/*` endpoints read and write the SQLite DB; the JSON file is consulted only as a fallback if a DB read errors.

#### Optional: token config

The dashboard's refresh button can fetch GitHub/GitLab if you drop a config file at `~/.config/mercator/config.toml`:

```toml
[github]
user = "zot24"
token = "ghp_xxxxx"   # optional — public repos work without it (60/hr cap)

[gitlab]
user = "zot24"
token = "glpat-xxxxx" # optional
```

The file is read once at `mercator serve` startup and stored in memory; chmod 0600 is applied automatically when the binary writes it (the read path doesn't enforce the mode but it's recommended). Tokens never leave the server — `GET /api/settings` returns a redacted shape (`{github_user, github_token_set, gitlab_user, gitlab_token_set}`) for the dashboard's "you have a token configured" hint.

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
| `-m, --map-file <file>` | Legacy JSON snapshot path (default: `mercator_map.json`) — used only as a fallback if the DB read fails. |
| `-d, --db <file>` | SQLite DB file (default: `mercator.db`). Source of truth for every read and write. |
| `--refresh <path>` | Local path the dashboard's refresh button re-scans. Repeat for multiple roots: `serve --refresh ~/code --refresh ~/oss`. Without this, the refresh button just reloads the page. |

Without `--refresh`, the dashboard sees new projects only after a fresh `mercator survey ...`. With `--refresh`, the in-dashboard refresh button re-scans and upserts directly into the live DB — faster for ad-hoc local changes. Remote sources (GitHub/GitLab/Obsidian) are not re-fetched on refresh; use `mercator survey ...` for those.

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

- **"Stops me from losing projects"** — local + GitHub + GitLab + Obsidian work; **Vercel / Supabase / Turso don't exist yet** ([#8](https://github.com/zot24/mercator/issues/8))
- **"Cuts the context-switch tax"** — file-tree explorer ships with smart auto-open: dirty repos open the most-recently-modified uncommitted file; clean repos open the freshest file under `src/`/`app/`/`lib/`; README is the fallback. Header banner shows branch, last commit, and days-since-modified.
- **"Catches silent decay"** — dirty repos and stale (≥21 days idle) surface today, plus a `ROTTING` filter for the rare project that's both. Deploy / quota decay is gated on [#8](https://github.com/zot24/mercator/issues/8)
- **"Tells me where to point AI"** — single-project agent launch works (with `--features swarm`); **cross-project landscape questioning** is [#20](https://github.com/zot24/mercator/issues/20)
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
- **[GOALS.md](GOALS.md)** — long-term direction (Phase 1/2/3).
- **[CLAUDE.md](CLAUDE.md)** — operator's manual for picking up the codebase.
- **[docs/decisions/](docs/decisions/)** — ADRs for non-obvious design decisions.
