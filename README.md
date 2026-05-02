# Mercator

> Cartography for your local development landscape

Mercator is a Rust CLI tool and web dashboard that discovers, organizes, and visualizes all your development projects in one place. It scans local directories, GitHub, and GitLab accounts to build a comprehensive map of your project landscape.

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

## Features

- **Local project scanning** — discovers Git repos, IDEA.md files, and folders
- **Git metadata** — branch, last commit, dirty status
- **Tech stack detection** — Node.js, Rust, Python, Go, Docker, and more
- **GitHub/GitLab integration** — fetches public repos via API
- **AI agent detection** — identifies projects using Claude Code or Codex (via CLAUDE.md, .claude, AGENTS.md, .codex)
- **Deduplication** — merges local repos with their GitHub/GitLab counterparts
- **Web dashboard** — dark-themed list UI with search, filters, and sorting
- **One-click actions** — open in VS Code, Claude Code, or Codex directly from the dashboard
- **Watch mode** — re-scan on an interval to keep the dashboard fresh

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

Scan a directory for projects and write results to JSON.

```bash
mercator survey ~/code                          # Local only
mercator survey ~/code --github zot24           # + GitHub repos
mercator survey ~/code --gitlab myuser          # + GitLab repos
mercator survey ~/code --github zot24 -w 5      # Re-scan every 5 minutes
mercator survey ~/code -o custom_map.json       # Custom output file
```

| Flag | Description |
|------|-------------|
| `--github <user>` | Fetch public repos from GitHub |
| `--gitlab <user>` | Fetch public repos from GitLab |
| `-o, --output <file>` | Output JSON file (default: `mercator_map.json`) |
| `-w, --watch <minutes>` | Re-scan every N minutes (keeps running) |

### `mercator serve`

Start the web dashboard.

```bash
mercator serve                          # http://127.0.0.1:3000
mercator serve -p 8080                  # Custom port
mercator serve -b 0.0.0.0              # Listen on all interfaces
mercator serve -m custom_map.json      # Custom map file
```

| Flag | Description |
|------|-------------|
| `-p, --port <port>` | Port to listen on (default: 3000) |
| `-b, --bind <ip>` | Bind address (default: 127.0.0.1) |
| `-m, --map-file <file>` | Path to map JSON (default: `mercator_map.json`) |

The serve command re-reads the JSON file on each request, so running `survey --watch` in the background keeps the dashboard fresh.

## Docker

```bash
# Build
docker build -t mercator .

# Run (mount your code directory)
docker run -p 3000:3000 -v ~/code:/data/code:ro mercator \
  sh -c "mercator survey /data/code -o /data/map.json && mercator serve -b 0.0.0.0 -m /data/map.json"

# With GitHub integration
docker run -p 3000:3000 -v ~/code:/data/code:ro mercator \
  sh -c "mercator survey /data/code --github zot24 -o /data/map.json && mercator serve -b 0.0.0.0 -m /data/map.json"

# With watch mode (survey + serve in parallel)
docker run -p 3000:3000 -v ~/code:/data/code:ro mercator \
  sh -c "mercator survey /data/code --github zot24 -o /data/map.json -w 5 & mercator serve -b 0.0.0.0 -m /data/map.json"
```

## Project Types Detected

| Type | Source | Description |
|------|--------|-------------|
| **Git** | Local | Directories containing `.git` |
| **GitHub** | API | Public repos from GitHub user |
| **GitLab** | API | Public repos from GitLab user |
| **Idea** | Local | Directories with `IDEA.md` |
| **Folder** | Local | Top-level directories without Git |

## Tech Stack

- **Rust** with Tokio async runtime
- **Axum** web framework
- **Clap** CLI parser
- **Reqwest** HTTP client
- **Walkdir** filesystem traversal
- **Tailwind CSS** + JetBrains Mono for the dashboard UI
