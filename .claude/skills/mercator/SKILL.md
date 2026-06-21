---
name: mercator
description: "Manage and discover local projects with the mercator CLI + SQLite map. Use when registering a newly created/cloned project, finding existing projects, marking what you're currently working on, generating a README 'projects' section, or checking which repos are dirty/unpushed/out-of-sync. Triggers on: mercator, project map/cartography, active projects, survey projects, 'what projects do I have', register this project, mark active."
allowed-tools:
  - Bash
  - Read
---

# mercator — cartography for your local projects

`mercator` is a Rust CLI (`brew install zot24/tap/mercator`, or `cargo run`) that
surveys local directories — plus GitHub/GitLab/Obsidian — into a SQLite "map" of
every project, auto-tags them, tracks git sync state, and tracks a "currently
working on" active set. It also serves a localhost dashboard.

> This skill ships inside the mercator repo (`.claude/skills/mercator/`), so it
> stays in lockstep with the CLI it documents.

## The database

Every subcommand takes `-d/--db` and **defaults to `./mercator.db`** in the
current directory. Pick one canonical DB and always point at it (or run from its
directory) — otherwise you scatter empty DBs around. A handy pattern:

```bash
DB=./mercator.db                       # or your canonical path, e.g.
# DB=~/Desktop/code/mercator/mercator.db
```

Mutations to the active set also write `active-projects.json` next to the DB, which
session-loaders (e.g. Hermes) read to know the current focus.

## When to use this

- **You just created or cloned a project** → register it (survey) and, if it's
  the current focus, mark it active, so other tools/sessions discover it.
- **You need to find a project** ("where's the X repo?", "what Rust projects?") →
  `list` / `search`.
- **You want the current focus** → `active list` or read `active-projects.json`.
- **Housekeeping** ("what's unpushed / not on GitHub / dirty?") → `list` filters.
- **You want a README "what I'm working on" section** → `readme`.

## Register a project (canonical add flow)

A full survey of the root is the safe, documented way — it upserts every project,
dedupes, auto-tags, refreshes git state, and purges nothing it can still see:

```bash
mercator survey ~/code --db "$DB" -o ./mercator_map.json
# LOCAL ONLY (no network). Add --github <user> --gitlab <user>
# (needs GITHUB_TOKEN/GITLAB_TOKEN) to also pull remote-only repos.
```

Then mark it as currently-working-on:

```bash
mercator active add ~/code/<project> --db "$DB" \
  --note "one-line: what it is + repo/install"
```

> Prefer surveying the whole root over a single path — single-path survey
> semantics can shrink the surveyed map. The root survey is fast (no fetch).

## Discover / query

```bash
mercator list   --db "$DB"                 # aligned table in a TTY; TSV when piped
mercator list   --db "$DB" --tech Rust     # by tech-stack entry
mercator list   --db "$DB" --tag ai        # by tag (exact, case-sensitive)
mercator list   --db "$DB" --type GitHub   # Git|Folder|Idea|GitHub|GitLab|Obsidian
mercator search 'cli-tool' --db "$DB"      # FTS5 over name/description/tags (AND)

# "needs attention" filters (AND-combine with --active etc.):
mercator list --db "$DB" --no-git          # local folders worth versioning
mercator list --db "$DB" --no-remote       # nothing pushed anywhere
mercator list --db "$DB" --out-of-sync     # branch ahead/behind upstream (last survey)
mercator list --db "$DB" --active          # only the active set

mercator list --db "$DB" --format json | jq '.[].name'   # JSON for scripts
```

## Active set ("currently working on")

```bash
mercator active add  ~/code/<p> --db "$DB" --note "…"   # add/refresh + note
mercator active list --db "$DB"                          # TSV, most-recent first
mercator active list --db "$DB" --format json            # enriched w/ metadata
mercator active remove ~/code/<p> --db "$DB"
mercator active export --db "$DB"   # rewrite active-projects.json after metadata changes
```

## README "projects" section

Generate a Markdown block of your projects (the active set by default) and splice
it into a profile README between `<!-- MERCATOR:START -->` / `<!-- MERCATOR:END -->`:

```bash
mercator readme --db "$DB"                       # print the block to stdout (table)
mercator readme --db "$DB" --inject ~/me/README.md   # update in place
mercator readme --db "$DB" --all --limit 10      # every project, capped
mercator readme --db "$DB" --list                # bullet list w/ per-project tech emoji
mercator readme --db "$DB" --public-only         # only verifiably-public repos (network)
mercator readme --db "$DB" --list --public-only --title "🚀 Currently Building" --no-emoji
```

- `--list` renders `- <emoji> **[name](url)** — description · tech`; the emoji is
  derived from the project's primary tech (Rust→🦀, Go→🐹, …) — `--no-emoji` to drop it.
- `--public-only` keeps only repos whose remote answers a 2xx unauthenticated GET
  (private → 404 → dropped); repos with no browsable remote are dropped too. Needs
  network; good for a **public** profile README so private work never leaks.

Re-run any time the active set changes; everything outside the markers is left
untouched.

## Other commands

```bash
mercator export ./mercator-export --db "$DB"   # one markdown file per project
mercator serve --port 3000                     # localhost dashboard; http://127.0.0.1:3000
```

## Notes & gotchas

- The map is **git-status aware but does not fetch** — `--out-of-sync` reflects the
  last survey's cached upstream; re-survey after pull/push for fresh state.
- Tech-stack auto-detection can miss some ecosystems; tags are heuristic. Don't
  hand-edit the DB — fix detection in `src/sources.rs` / `src/tags_graph.rs`.
- Operator manual: `CLAUDE.md`; user pitch: `README.md`; roadmap: `docs/STATUS.md`
  + `GOALS.md`.
- **Habit:** after scaffolding a new project, survey so the map stays complete, and
  `active add` it if it's the focus.
