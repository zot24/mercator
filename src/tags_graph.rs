//! Auto-tagging + relationship graph computation.
//!
//! Both touch `Project` and only `Project`. Auto-tagging scans names +
//! descriptions + tech-stack against a hardcoded keyword bag (15 buckets);
//! `compute_graph` builds D3-friendly node/edge JSON from name mentions,
//! shared domain keywords, shared tags, and idea↔impl pairs.

use crate::project::{Project, ProjectType};

/// Tag projects in place based on name, description, tech stack, and detected
/// agent. Buckets are coarse on purpose — the dashboard surfaces them as
/// sidebar filters, not a taxonomy.
pub fn auto_tag_projects(projects: &mut [Project]) {
    let tag_keywords: &[(&str, &[&str])] = &[
        (
            "ai",
            &[
                "ai",
                "llm",
                "gpt",
                "claude",
                "codex",
                "agent",
                "neural",
                "ml",
                "machine learning",
                "openai",
                "anthropic",
                "pytorch",
                "tensorflow",
            ],
        ),
        (
            "web",
            &[
                "website",
                "landing",
                "frontend",
                "react",
                "next",
                "svelte",
                "html",
                "css",
                "tailwind",
                "vue",
                "angular",
                "sveltekit",
            ],
        ),
        (
            "api",
            &[
                "api", "rest", "graphql", "endpoint", "server", "express", "axum", "hono",
                "fastapi",
            ],
        ),
        (
            "cli",
            &[
                "cli",
                "terminal",
                "command line",
                "command-line",
                "shell",
                "bash",
            ],
        ),
        (
            "devops",
            &[
                "docker",
                "kubernetes",
                "k8s",
                "ci/cd",
                "deploy",
                "infra",
                "terraform",
                "ansible",
                "helm",
                "umbrel",
            ],
        ),
        (
            "mobile",
            &[
                "ios",
                "android",
                "mobile",
                "swift",
                "kotlin",
                "react native",
                "flutter",
            ],
        ),
        (
            "data",
            &[
                "database",
                "postgres",
                "sqlite",
                "redis",
                "mongo",
                "data",
                "analytics",
                "scraping",
                "crawler",
            ],
        ),
        (
            "blockchain",
            &[
                "blockchain",
                "web3",
                "solana",
                "ethereum",
                "crypto",
                "token",
                "nft",
                "rwa",
                "smart contract",
            ],
        ),
        (
            "seo",
            &["seo", "search engine", "sitemap", "analytics", "tracking"],
        ),
        (
            "auth",
            &[
                "auth",
                "login",
                "oauth",
                "jwt",
                "session",
                "credential",
                "password",
            ],
        ),
        (
            "bot",
            &[
                "bot",
                "telegram",
                "whatsapp",
                "discord",
                "slack",
                "chat",
                "messaging",
            ],
        ),
        (
            "automation",
            &[
                "automat", "workflow", "cron", "schedule", "scrape", "crawl", "hook",
            ],
        ),
        ("game", &["game", "rpg", "player", "level", "score"]),
        (
            "docs",
            &[
                "documentation",
                "readme",
                "wiki",
                "knowledge",
                "obsidian",
                "note",
            ],
        ),
        (
            "finance",
            &[
                "tax",
                "invoice",
                "payment",
                "billing",
                "finance",
                "accounting",
                "portfolio",
            ],
        ),
    ];

    for project in projects.iter_mut() {
        let haystack = format!(
            "{} {} {} {}",
            project.name.to_lowercase(),
            project.description.to_lowercase(),
            project.tech_stack.join(" ").to_lowercase(),
            project.agent_used.as_deref().unwrap_or(""),
        );

        let mut tags = Vec::new();
        for (tag, keywords) in tag_keywords {
            if keywords.iter().any(|kw| haystack.contains(kw)) {
                tags.push(tag.to_string());
            }
        }
        project.tags = tags;
    }
}

/// Extract significant keywords from a description for domain matching.
/// Lowercase, ≥4 chars, dropped if in the stopword list.
pub fn domain_keywords(text: &str) -> Vec<String> {
    let stopwords = [
        "the",
        "and",
        "for",
        "with",
        "from",
        "that",
        "this",
        "are",
        "was",
        "not",
        "you",
        "your",
        "has",
        "have",
        "had",
        "but",
        "all",
        "can",
        "will",
        "one",
        "our",
        "out",
        "use",
        "how",
        "its",
        "let",
        "may",
        "who",
        "did",
        "get",
        "she",
        "her",
        "him",
        "his",
        "old",
        "new",
        "now",
        "way",
        "each",
        "make",
        "like",
        "just",
        "over",
        "such",
        "take",
        "than",
        "them",
        "very",
        "when",
        "what",
        "some",
        "into",
        "been",
        "more",
        "other",
        "which",
        "about",
        "would",
        "their",
        "these",
        "could",
        "project",
        "using",
        "based",
        "built",
        "also",
        "here",
        "https",
        "http",
        "www",
        "com",
        "org",
        "github",
        "description",
        "provided",
        "none",
        "file",
        "code",
    ];
    let lower = text.to_lowercase();
    lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 4 && !stopwords.contains(w))
        .map(|w| w.to_string())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect()
}

/// Compute a relationship graph: nodes for each project, edges weighted by
/// name-mention, shared keywords, shared tags, and idea↔impl pairs. Only
/// edges with `weight >= 4.0` are emitted.
pub fn compute_graph(projects: &[Project]) -> serde_json::Value {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    let keywords: Vec<Vec<String>> = projects
        .iter()
        .map(|p| {
            let desc_cap: String = p.description.chars().take(200).collect();
            domain_keywords(&format!("{} {}", p.name, desc_cap))
        })
        .collect();

    for (i, p) in projects.iter().enumerate() {
        nodes.push(serde_json::json!({
            "id": i,
            "name": p.name,
            "type": p.project_type,
            "tags": p.tags,
            "tech": p.tech_stack,
            "description": p.description,
            "agent": p.agent_used,
            "path": p.path,
            "obsidianUrl": p.obsidian_url,
        }));
    }

    for i in 0..projects.len() {
        for j in (i + 1)..projects.len() {
            let mut weight: f32 = 0.0;
            let mut reasons = Vec::new();

            // 1. One project name appears in the other's description (usage/dependency)
            let ni = projects[i].name.to_lowercase();
            let nj = projects[j].name.to_lowercase();
            let di = projects[i].description.to_lowercase();
            let dj = projects[j].description.to_lowercase();
            if ni.len() >= 4 && dj.contains(&ni) {
                weight += 6.0;
                reasons.push(format!(
                    "{} mentioned in {}",
                    projects[i].name, projects[j].name
                ));
            }
            if nj.len() >= 4 && di.contains(&nj) {
                weight += 6.0;
                reasons.push(format!(
                    "{} mentioned in {}",
                    projects[j].name, projects[i].name
                ));
            }

            // 2. Shared domain keywords from descriptions (goal similarity)
            let shared_kw: Vec<&String> = keywords[i]
                .iter()
                .filter(|kw| keywords[j].contains(kw))
                .collect();
            let kw_score = (shared_kw.len() as f32).min(5.0);
            if kw_score >= 3.0 {
                weight += kw_score;
                reasons.push(format!(
                    "shared: {}",
                    shared_kw
                        .iter()
                        .take(3)
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }

            // 3. Shared tags — only count if there's already another signal
            //    (prevents generic tag-only connections like "both tagged ai")
            let shared_tags: Vec<&String> = projects[i]
                .tags
                .iter()
                .filter(|t| projects[j].tags.contains(t))
                .collect();
            if !shared_tags.is_empty() && weight > 0.0 {
                weight += shared_tags.len() as f32;
                reasons.push(format!(
                    "tags: {}",
                    shared_tags
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }

            // 4. Obsidian idea linked to implementation — only counts when
            //    we already have another signal so two unrelated ideas don't
            //    pair up just because one is an Obsidian note.
            let i_obs = matches!(projects[i].project_type, ProjectType::Obsidian);
            let j_obs = matches!(projects[j].project_type, ProjectType::Obsidian);
            if i_obs != j_obs && weight > 0.0 {
                weight += 3.0;
                reasons.push("idea→impl".to_string());
            }

            if weight >= 4.0 {
                edges.push(serde_json::json!({
                    "source": i,
                    "target": j,
                    "weight": weight,
                    "shared": reasons,
                }));
            }
        }
    }

    serde_json::json!({ "nodes": nodes, "edges": edges })
}
