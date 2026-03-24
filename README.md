# Mercator

> Cartography for your local development landscape

Mercator is a Rust CLI tool and web dashboard that discovers, organizes, and visualizes all your development projects in one place. It scans local directories, GitHub, and GitLab accounts to build a comprehensive map of your project landscape.

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
