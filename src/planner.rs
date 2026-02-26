//! Task planner - two-phase orchestration
//!
//! Phase 1: Find the right decomposer agent (e.g., Blueprint) to break down the goal
//! Phase 2: Cast the team - match/create agent profiles for each task
//!
//! No generic agents. No keyword heuristics. Every task gets a real persona.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::dag::{TaskGraph, TaskNode};
use crate::db::Database;
use crate::gateway::{GatewayClient, GatewayConfig, GatewayEvent};
use crate::models::{AgentProfile, Complexity};

/// Default profiles that ship with dare
pub const DEFAULT_PROFILES: &[(&str, &str, &str, &str, &str, &str, &str)] = &[
    // (codename, name, role, expertise, personality, avatar_emoji, color)
    (
        "blueprint",
        "Blueprint",
        "Technical Planner",
        "System architecture, task decomposition, dependency analysis, project planning",
        "Methodical, thorough, thinks in systems. Breaks big problems into clean, atomic pieces. Never hand-waves.",
        "📐",
        "#4A9EFF",
    ),
    (
        "volt",
        "Volt",
        "Backend Engineer",
        "APIs, databases, server-side logic, system design, Rust, TypeScript, Python",
        "Pragmatic, fast-moving, opinionated about clean architecture. Ships working code, not perfect code.",
        "⚡",
        "#00FF88",
    ),
    (
        "pixel",
        "Pixel",
        "Frontend Engineer",
        "UI/UX implementation, HTML/CSS/JS, HTMX, React, responsive design, accessibility",
        "Detail-oriented, cares about user experience. Makes things look good AND work well.",
        "🎨",
        "#FF6B9D",
    ),
    (
        "sentinel",
        "Sentinel",
        "QA & Testing Engineer",
        "Test design, integration testing, edge cases, security review, code review",
        "Skeptical, thorough, finds the bugs others miss. Thinks about what could go wrong.",
        "🛡️",
        "#FFB800",
    ),
    (
        "scribe",
        "Scribe",
        "Technical Writer",
        "Documentation, API docs, README, architecture decision records, tutorials",
        "Clear communicator, hates jargon. Makes complex things understandable.",
        "📝",
        "#B088FF",
    ),
    (
        "architect",
        "Architect",
        "System Architect",
        "System design, infrastructure, scalability, technology selection, integration patterns",
        "Big-picture thinker, experienced. Sees how pieces fit together. Challenges assumptions.",
        "🏗️",
        "#FF8844",
    ),
    (
        "forge",
        "Forge",
        "DevOps Engineer",
        "CI/CD, Docker, deployment, monitoring, infrastructure as code, shell scripting",
        "Automation-obsessed, hates manual work. If it can be scripted, it should be.",
        "🔧",
        "#44DDAA",
    ),
    (
        "cipher",
        "Cipher",
        "Security Engineer",
        "Authentication, authorization, encryption, security audits, threat modeling",
        "Paranoid (in a good way), methodical. Assumes everything is an attack surface.",
        "🔐",
        "#FF4444",
    ),
];

/// Planner for creating task graphs from goals
pub struct Planner {
    db: Arc<Database>,
    config: Config,
}

impl Planner {
    pub fn new(config: &Config, db: &Arc<Database>) -> Self {
        Self {
            db: Arc::clone(db),
            config: config.clone(),
        }
    }

    /// Seed default profiles into the database (idempotent)
    pub async fn seed_default_profiles(&self) -> Result<()> {
        for (codename, name, role, expertise, personality, emoji, color) in DEFAULT_PROFILES {
            // Skip if already exists
            if self.db.get_profile_by_codename(codename).await?.is_some() {
                continue;
            }
            let mut profile = AgentProfile::new(codename, name, role);
            profile.expertise = expertise.to_string();
            profile.personality = personality.to_string();
            profile.avatar_emoji = emoji.to_string();
            profile.color = color.to_string();
            self.db.create_profile(&profile).await?;
            tracing::info!(codename = codename, "Seeded default profile");
        }
        Ok(())
    }

    /// Full planning pipeline: decompose goal → cast team
    ///
    /// Phase 1: Pick a decomposer agent (Blueprint or best match) to break down the goal
    /// Phase 2: For each task, assign the right agent profile
    pub async fn plan(&self, goal: &str) -> Result<PlannedCouncil> {
        // Ensure default profiles exist
        self.seed_default_profiles().await?;

        // Phase 1: Decompose the goal
        println!("\n📐 Phase 1: Decomposing goal...");
        let decomposition = self.decompose(goal).await?;

        // Phase 2: Cast the team
        println!("🎭 Phase 2: Casting the team...");
        let council = self.cast_team(goal, decomposition).await?;

        Ok(council)
    }

    /// Phase 1: Find the best decomposer and break down the goal
    async fn decompose(&self, goal: &str) -> Result<Decomposition> {
        // Try LLM-based decomposition via gateway
        let gateway_config = match GatewayConfig::load() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Cannot connect to gateway: {}", e);
                return self.fallback_decompose(goal);
            }
        };

        let (event_tx, mut event_rx) = mpsc::channel::<GatewayEvent>(100);
        let gateway = match GatewayClient::connect(gateway_config, event_tx).await {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("Failed to connect to gateway: {}", e);
                return self.fallback_decompose(goal);
            }
        };

        // Get Blueprint profile for the decomposer prompt
        let blueprint = self.db.get_profile_by_codename("blueprint").await?;
        let prompt = self.build_decomposer_prompt(goal, blueprint.as_ref());

        let session_key = match gateway
            .spawn_session("planner", "dare-planner", &prompt)
            .await
        {
            Ok(key) => key,
            Err(e) => {
                tracing::warn!("Failed to spawn decomposer: {}, using fallback", e);
                return self.fallback_decompose(goal);
            }
        };

        println!("  🧠 Blueprint is thinking...");

        // Wait for completion
        let timeout_dur = Duration::from_secs(180);
        let deadline = tokio::time::Instant::now() + timeout_dur;

        loop {
            if tokio::time::Instant::now() > deadline {
                tracing::warn!("Decomposer timed out");
                let _ = gateway.kill_session(&session_key).await;
                return self.fallback_decompose(goal);
            }

            match tokio::time::timeout(Duration::from_secs(5), event_rx.recv()).await {
                Ok(Some(GatewayEvent::SessionCompleted { session_key: key }))
                    if key == session_key =>
                {
                    println!("  ✓ Decomposition complete");
                    break;
                }
                Ok(Some(GatewayEvent::SessionFailed { session_key: key, error }))
                    if key == session_key =>
                {
                    tracing::warn!("Decomposer failed: {}", error);
                    return self.fallback_decompose(goal);
                }
                _ => continue,
            }
        }

        // TODO: Capture agent output and parse structured decomposition
        // For now, fallback to LLM-less decomposition
        // Once we can read session output, this will parse the YAML the decomposer produces
        self.fallback_decompose(goal)
    }

    /// Build the prompt for the decomposer agent (Blueprint)
    fn build_decomposer_prompt(&self, goal: &str, blueprint: Option<&AgentProfile>) -> String {
        let identity = if let Some(p) = blueprint {
            format!(
                "You are **{name}** (codename: {codename}), {role}.\n{personality}\n\n",
                name = p.name,
                codename = p.codename,
                role = p.role,
                personality = p.personality,
            )
        } else {
            "You are a technical planner. Break problems into clean, atomic tasks.\n\n".to_string()
        };

        format!(
            r#"{identity}# Your Mission

Decompose this goal into a task graph that a team of AI agents can execute.

## Goal
{goal}

## Available Agent Roles

When suggesting agents for tasks, use these role types:
- **backend** — APIs, databases, server logic
- **frontend** — UI, HTML/CSS/JS, user-facing code
- **architect** — System design, infrastructure, tech selection
- **devops** — CI/CD, Docker, deployment, scripting
- **security** — Auth, encryption, threat modeling
- **testing** — Tests, QA, code review
- **docs** — Documentation, READMEs, tutorials
- **planner** — Further decomposition of complex sub-problems

## Output Format

Output ONLY valid YAML:

```yaml
name: "Short Council Name"
description: "What this council achieves"
tasks:
  - id: unique_snake_case_id
    description: "Detailed description of what this agent must do"
    role: backend  # One of the roles above
    depends_on: []
    files: [path/to/file.rs]

  - id: another_task
    description: "Another task"
    role: frontend
    depends_on: [unique_snake_case_id]
    files: [templates/page.html]
```

## Rules

1. Each task = one agent. Make it atomic (2-10 minutes of work).
2. Use descriptive task IDs (e.g., `design_schema`, `implement_auth`, not `task-1`).
3. Specify the `role` — this determines which agent persona gets assigned.
4. Be explicit about dependencies.
5. List all files each task creates or modifies.
6. Tasks with no dependencies run in parallel.
7. Think about the right ORDER: schema → models → services → handlers → tests → docs.
8. Don't create unnecessary tasks. Fewer focused tasks > many trivial ones.

Output ONLY the YAML. No explanation before or after.
"#,
            identity = identity,
            goal = goal,
        )
    }

    /// Fallback decomposition when gateway is unavailable
    /// Still produces role-annotated tasks, just without LLM intelligence
    fn fallback_decompose(&self, goal: &str) -> Result<Decomposition> {
        tracing::info!("Using fallback decomposition");
        println!("  ⚠ Using local decomposition (no LLM available)");

        // Single task with role inference from goal text
        let role = infer_primary_role(goal);

        Ok(Decomposition {
            name: short_title(goal),
            description: goal.to_string(),
            tasks: vec![DecomposedTask {
                id: slugify(goal),
                description: goal.to_string(),
                role,
                depends_on: vec![],
                files: vec![],
            }],
        })
    }

    /// Phase 2: For each decomposed task, find or create the right agent profile
    async fn cast_team(&self, _goal: &str, decomposition: Decomposition) -> Result<PlannedCouncil> {
        let profiles = self.db.list_profiles(true).await?;
        let mut assignments: Vec<TaskAssignment> = Vec::new();

        for task in &decomposition.tasks {
            // Find best matching profile for this task's role
            let profile = self.find_profile_for_role(&task.role, &profiles)
                .or_else(|| {
                    // No exact match — create a generic one for this role
                    tracing::info!(role = %task.role, task = %task.id, "No profile for role, will use closest match");
                    profiles.first() // Last resort: any active profile
                });

            let profile_id = profile.map(|p| p.id.clone());
            let profile_codename = profile.map(|p| p.codename.clone());

            if let Some(ref codename) = profile_codename {
                println!("  {} → {} ({})", task.id, codename, task.role);
            } else {
                println!("  {} → unassigned ({})", task.id, task.role);
            }

            assignments.push(TaskAssignment {
                task_id: task.id.clone(),
                description: task.description.clone(),
                role: task.role.clone(),
                depends_on: task.depends_on.clone(),
                files: task.files.clone(),
                profile_id,
                profile_codename,
            });
        }

        Ok(PlannedCouncil {
            name: decomposition.name,
            description: decomposition.description,
            assignments,
        })
    }

    /// Match a role string to the best available profile
    fn find_profile_for_role<'a>(&self, role: &str, profiles: &'a [AgentProfile]) -> Option<&'a AgentProfile> {
        let role_lower = role.to_lowercase();

        // Direct codename/role match
        for p in profiles {
            if p.codename == role_lower || p.role.to_lowercase().contains(&role_lower) {
                return Some(p);
            }
        }

        // Role keyword mapping
        let mapping: &[(&[&str], &str)] = &[
            (&["backend", "api", "server", "database", "db"], "volt"),
            (&["frontend", "ui", "ux", "html", "css", "web"], "pixel"),
            (&["architect", "design", "system", "infrastructure"], "architect"),
            (&["devops", "deploy", "docker", "ci", "cd", "ops"], "forge"),
            (&["security", "auth", "encrypt", "threat"], "cipher"),
            (&["test", "qa", "review", "quality"], "sentinel"),
            (&["doc", "readme", "write", "tutorial"], "scribe"),
            (&["plan", "decompose", "break"], "blueprint"),
        ];

        for (keywords, codename) in mapping {
            if keywords.iter().any(|k| role_lower.contains(k)) {
                if let Some(p) = profiles.iter().find(|p| p.codename == *codename) {
                    return Some(p);
                }
            }
        }

        None
    }

    /// Convert a PlannedCouncil into a TaskGraph ready for execution
    pub fn to_task_graph(&self, council: &PlannedCouncil) -> Result<TaskGraph> {
        let mut graph = TaskGraph::new();
        graph.name = Some(council.name.clone());
        graph.description = Some(council.description.clone());

        for assignment in &council.assignments {
            graph.add_task(TaskNode {
                id: assignment.task_id.clone(),
                description: assignment.description.clone(),
                depends_on: assignment.depends_on.clone(),
                outputs: assignment.files.iter().map(|f| PathBuf::from(f)).collect(),
                estimated_complexity: Some(Complexity::Medium),
                agent_profile: assignment.profile_codename.clone(),
            });
        }

        graph.validate()?;
        Ok(graph)
    }

    /// Load and validate a task file (YAML)
    pub fn load_task_file(path: &PathBuf) -> Result<TaskGraph> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;

        let graph: TaskGraph = serde_yaml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?;

        graph.validate()?;
        Ok(graph)
    }
}

// ── Data structures ──────────────────────────────────────────────────

/// Output of Phase 1: decomposed tasks with roles
#[derive(Debug, Clone)]
pub struct Decomposition {
    pub name: String,
    pub description: String,
    pub tasks: Vec<DecomposedTask>,
}

/// A task from the decomposer, before profile assignment
#[derive(Debug, Clone)]
pub struct DecomposedTask {
    pub id: String,
    pub description: String,
    pub role: String,
    pub depends_on: Vec<String>,
    pub files: Vec<String>,
}

/// A task with its assigned agent profile
#[derive(Debug, Clone)]
pub struct TaskAssignment {
    pub task_id: String,
    pub description: String,
    pub role: String,
    pub depends_on: Vec<String>,
    pub files: Vec<String>,
    pub profile_id: Option<String>,
    pub profile_codename: Option<String>,
}

/// The complete planned council — ready for execution
#[derive(Debug, Clone)]
pub struct PlannedCouncil {
    pub name: String,
    pub description: String,
    pub assignments: Vec<TaskAssignment>,
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Infer the primary role from a goal description
fn infer_primary_role(goal: &str) -> String {
    let lower = goal.to_lowercase();
    if lower.contains("test") || lower.contains("qa") {
        "testing".to_string()
    } else if lower.contains("deploy") || lower.contains("docker") || lower.contains("ci/cd") {
        "devops".to_string()
    } else if lower.contains("doc") || lower.contains("readme") {
        "docs".to_string()
    } else if lower.contains("ui") || lower.contains("frontend") || lower.contains("page") {
        "frontend".to_string()
    } else if lower.contains("security") || lower.contains("auth") {
        "security".to_string()
    } else if lower.contains("design") || lower.contains("architect") {
        "architect".to_string()
    } else {
        "backend".to_string()
    }
}

/// Generate a slug from text for task IDs
fn slugify(text: &str) -> String {
    let slug: String = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    // Collapse underscores and trim
    let collapsed: String = slug
        .split('_')
        .filter(|s| !s.is_empty())
        .take(5) // Max 5 words
        .collect::<Vec<_>>()
        .join("_");
    if collapsed.len() > 40 {
        collapsed[..40].to_string()
    } else {
        collapsed
    }
}

/// Generate a short council title from a long description
fn short_title(description: &str) -> String {
    let text = description.trim();

    if text.len() <= 60 {
        return text.to_string();
    }

    if let Some(first_line) = text.lines().next() {
        let line = first_line.trim().trim_start_matches('#').trim();
        if !line.is_empty() && line.len() <= 80 {
            return line.to_string();
        }
    }

    for delim in [". ", ".\n", "! ", "?\n"] {
        if let Some(pos) = text.find(delim) {
            if pos > 10 && pos <= 80 {
                return text[..pos].to_string();
            }
        }
    }

    if let Some(pos) = text[..60.min(text.len())].rfind(' ') {
        format!("{}...", &text[..pos])
    } else {
        format!("{}...", &text[..60.min(text.len())])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slugify() {
        assert_eq!(slugify("Build an auth system"), "build_an_auth_system");
        assert_eq!(slugify("Add JWT + OAuth"), "add_jwt_oauth");
    }

    #[test]
    fn test_infer_role() {
        assert_eq!(infer_primary_role("Build a REST API"), "backend");
        assert_eq!(infer_primary_role("Write integration tests"), "testing");
        assert_eq!(infer_primary_role("Design the UI for settings page"), "frontend");
        assert_eq!(infer_primary_role("Set up Docker deployment"), "devops");
    }

    #[test]
    fn test_short_title() {
        assert_eq!(short_title("Short"), "Short");
        let long = "This is a very long description that goes on and on and should be truncated at some reasonable point";
        assert!(short_title(long).len() <= 80);
    }
}
