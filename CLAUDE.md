# Mercator - Project Topography Tool

> "Cartography for your local development landscape"

## What is Mercator?

Mercator is a Rust-based CLI tool and web dashboard that helps you discover, organize, and visualize all your development projects in one place. It scans your local directories, GitHub, and GitLab accounts to build a comprehensive "map" of your project landscape.

## Current Status: v0.1.0 (In Development)

### What's Working ✅

#### CLI Commands
```bash
# Survey local projects
cargo run -- survey ~/Desktop/code

# Survey with GitHub integration
cargo run -- survey --github yourusername

# Survey with GitLab integration  
cargo run -- survey --gitlab yourusername

# Start the web dashboard
cargo run -- serve --port 3000
```

#### Features Implemented

| Feature | Description |
|---------|-------------|
| **Local Project Scanning** | Discovers Git repos, IDEA.md files, and folders |
| **Git Metadata** | Extracts branch name, last commit message, dirty status |
| **Tech Stack Detection** | Identifies Node.js, Rust, Python, Go, Docker, etc. |
| **GitHub Integration** | Fetches public repos via GitHub API |
| **GitLab Integration** | Fetches public repos via GitLab API |
| **Web Dashboard** | Dark-themed UI with cards, filters, search |
| **Project Filtering** | By type (Git, GitHub, GitLab, Idea, Folder) |
| **Tech Stack Filter** | Filter by detected technologies |
| **Dirty Project Filter** | Highlight repos with uncommitted changes |
| **Search** | Real-time search by name or description |
| **Sorting** | By name A-Z/Z-A, newest/oldest |
| **VS Code Integration** | Click cards to open in VS Code |

#### Project Types Detected

- **Git** - Local Git repositories
- **GitHub** - Repos fetched from GitHub API
- **GitLab** - Repos fetched from GitLab API  
- **Idea** - Folders containing `IDEA.md` or `README.md`
- **Folder** - Uncategorized directories

## Architecture

```
mercator/
├── src/
│   └── main.rs          # CLI + Axum web server
├── dist/
│   └── index.html       # Web dashboard (single file)
├── Cargo.toml           # Rust dependencies
└── mercator_map.json   # Survey output (generated)
```

### Tech Stack
- **Language**: Rust
- **CLI**: Clap 4 (derive)
- **Web Server**: Axum 0.8
- **Async Runtime**: Tokio
- **HTTP Client**: Reqwest
- **Filesystem**: Walkdir
- **Serialization**: Serde + JSON

## What's Missing / TODO

### High Priority
- [ ] **Settings Panel** - UI doesn't yet save/load the GitHub/GitLab credentials from settings
- [ ] **Private Repos** - Need to support GitHub/GitLab tokens for private repos
- [ ] **Refresh Button** - UI refresh doesn't actually trigger a new survey
- [ ] **Multiple Scan Paths** - Can't scan multiple directories yet

### Medium Priority
- [ ] **Config File** - Load defaults from `~/.config/mercator.toml`
- [ ] **Pagination** - For GitHub/GitLab (currently limited to 50 repos)
- [ ] **Keyboard Shortcuts** - Search focus, filter shortcuts
- [ ] **Project Details Modal** - Click to see full description, commits, etc.

### Nice to Have
- [ ] **GitHub/GitLab Status Badges** - Show if repo is ahead/behind remote
- [ ] **Language Icons** - Visual indicators for Python, Rust, Go, etc.
- [ ] **Tags/Categories** - User-defined project categories
- [ ] **Export** - Export to CSV, JSON, Markdown
- [ ] **Watch Mode** - Auto-refresh when filesystem changes

## Known Issues

1. **Rust Edition Error** - Sometimes `cargo build` shows "async fn is not permitted in Rust 2015". If this happens, run `cargo clean && cargo build`

2. **Visual Scrolling Error** - User reported visual errors when scrolling. The UI is a single long page without pagination.

3. **GitHub/GitLab API Limits** - Unauthenticated requests are limited. Consider adding token support.

## How to Contribute

### Development Workflow
```bash
# Build
cargo build

# Run survey
cargo run -- survey ~/Desktop/code

# Run web server
cargo run -- serve --port 3000

# Open in browser
open http://localhost:3000
```

### Testing with Real Data
```bash
# Survey your actual projects
cargo run -- survey ~/Desktop/code

# With GitHub integration
cargo run -- survey --github yourusername

# Survey specific directory
cargo run -- survey /path/to/projects
```

### Code Style
- Use `rustfmt` for formatting
- Run `cargo clippy` for linting
- Keep async functions in `async fn main()` or separate handlers
- Use `serde` derive for all structs that need JSON serialization

## Design Decisions

### Why Rust?
- Fast compilation and execution
- Single binary deployment
- Memory safety without GC
- Excellent async support with Tokio

### Why Axum?
- Minimal, ergonomic API
- Built on Tokio and Hyper
- Great integration with Tower middleware

### Why Walkdir?
- Handles deep directory structures
- Async-friendly
- Good error handling

### Project Structure
The `Project` struct contains all metadata that can come from different sources:
- Local Git repos (via git commands)
- GitHub API (via REST)
- GitLab API (via REST)
- Filesystem (via Walkdir)

The `project_type` enum distinguishes between these sources, but the UI treats them all uniformly.

## Future Vision

The ultimate goal is to create a **personal development command center**:

1. **Universal Project Discovery** - Find projects anywhere: local dirs, GitHub, GitLab, GitHub Enterprise, Bitbucket
2. **Project Intelligence** - Track which projects need attention (dirty, stale, deprecated)
3. **Quick Actions** - Open in VS Code, launch in browser, run scripts
4. **Team Sync** - Share project maps with team members via WhatsApp/Telegram bot
5. **AI Integration** - Let the bot analyze your project landscape and suggest focus areas

---

*Last updated: 2026-03-21*
