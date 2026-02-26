//! Task planner — single LLM call that decomposes AND casts
//!
//! Flow:
//! 1. Spawn Blueprint agent with the goal + available profiles
//! 2. Blueprint decomposes into tasks and assigns profiles
//! 3. Blueprint posts structured JSON to council API
//! 4. Planner reads council messages, parses the plan
//!
//! No keyword heuristics. No string matching. The LLM decides everything.

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

    /// Plan a council: decompose goal + assign agent profiles in one LLM call
    pub async fn plan(&self, goal: &str) -> Result<PlannedCouncil> {
        self.seed_default_profiles().await?;

        // Load available profiles for the prompt
        let profiles = self.db.list_profiles(true).await?;

        // Try LLM-based planning via gateway
        println!("\n📐 Planning council...");
        match self.plan_via_llm(goal, &profiles).await {
            Ok(council) => Ok(council),
            Err(e) => {
                tracing::warn!(error = %e, "LLM planning failed, using fallback");
                println!("  ⚠ LLM planning unavailable, using minimal fallback");
                self.fallback_plan(goal)
            }
        }
    }

    /// Spawn Blueprint agent to decompose + cast via council messages
    async fn plan_via_llm(&self, goal: &str, profiles: &[AgentProfile]) -> Result<PlannedCouncil> {
        let gateway_config = GatewayConfig::load().context("Gateway config not available")?;
        let (event_tx, mut event_rx) = mpsc::channel::<GatewayEvent>(100);
        let gateway = GatewayClient::connect(gateway_config, event_tx)
            .await
            .context("Failed to connect to gateway")?;

        // Create a temporary run so Blueprint can post to council
        let temp_run_id = format!("plan-{}", uuid::Uuid::new_v4().simple());
        let temp_run = crate::models::Run::new(&format!("Planning: {}", short_title(goal)));
        let run_id = temp_run.id.clone();
        self.db.create_run(&temp_run).await?;

        // Build the planning prompt
        let prompt = self.build_planning_prompt(goal, profiles, &run_id);

        // Spawn the planner agent
        let session_key = gateway
            .spawn_task("blueprint-planner", "dare-planner", &prompt)
            .await
            .context("Failed to spawn planner agent")?;

        println!("  🧠 Blueprint is planning...");

        // Wait for completion
        let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
        let mut completed = false;

        loop {
            if tokio::time::Instant::now() > deadline {
                let _ = gateway.kill_session(&session_key).await;
                anyhow::bail!("Planner agent timed out after 180s");
            }

            match tokio::time::timeout(Duration::from_secs(5), event_rx.recv()).await {
                Ok(Some(GatewayEvent::SessionCompleted { session_key: key }))
                    if key == session_key || session_key.contains(&key) || key.contains(&session_key) =>
                {
                    completed = true;
                    break;
                }
                Ok(Some(GatewayEvent::SessionFailed { session_key: key, error }))
                    if key == session_key || session_key.contains(&key) || key.contains(&session_key) =>
                {
                    anyhow::bail!("Planner agent failed: {}", error);
                }
                Ok(None) => {
                    anyhow::bail!("Gateway event channel closed");
                }
                _ => continue,
            }
        }

        if !completed {
            anyhow::bail!("Planner did not complete");
        }

        println!("  ✓ Blueprint finished planning");

        // Read the plan from council messages
        // get_messages returns (id, task_id, msg_type, payload_json, created_at)
        let messages = self.db.get_messages(&run_id, 50).await?;

        // Find the structured plan message (tagged as "plan" in payload)
        let plan_text = messages
            .iter()
            .rev()
            .find_map(|(_id, _task_id, _msg_type, payload, _created)| {
                let tag = payload.get("tag").and_then(|v| v.as_str());
                let text = payload.get("text").and_then(|v| v.as_str());
                if tag == Some("plan") {
                    text.map(|t| t.to_string())
                } else {
                    None
                }
            })
            .or_else(|| {
                // Fallback: last message's text field
                messages.last().and_then(|(_id, _task_id, _msg_type, payload, _created)| {
                    payload.get("text").and_then(|v| v.as_str()).map(|t| t.to_string())
                })
            });

        let plan_text = match plan_text {
            Some(text) => text,
            None => {
                tracing::warn!("No plan message found in council, falling back");
                anyhow::bail!("Blueprint didn't post a plan to the council");
            }
        };

        // Parse the structured plan
        self.parse_plan_response(&plan_text, profiles)
    }

    /// Build the prompt that tells Blueprint to decompose AND assign profiles
    fn build_planning_prompt(&self, goal: &str, profiles: &[AgentProfile], run_id: &str) -> String {
        let dashboard_port = self.config.dashboard.port;
        let council_api = format!("http://127.0.0.1:{}/api/council/{}", dashboard_port, run_id);

        // Format available profiles
        let profiles_list = profiles
            .iter()
            .map(|p| {
                format!(
                    "- **{}** (`{}`): {} — {}",
                    p.name, p.codename, p.role, p.expertise
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        format!(
            r#"# You are Blueprint — dare.run's Technical Planner

You decompose goals into tasks and assign the right agent to each task.

## Your Goal
{goal}

## Available Agent Profiles
{profiles_list}

## What You Must Do

1. **Analyze** the goal — understand what needs to be built
2. **Decompose** into atomic tasks (each completable by one agent in 2-10 minutes)
3. **Assign** each task to the best agent profile from the list above
4. **Post your plan** as structured JSON to the council API

## Output

Post your plan to the council with tag "plan" as a JSON object. Use this exact curl command:

```bash
curl -s -X POST {council_api}/post \
  -H "Content-Type: application/json" \
  -d '<YOUR_JSON_PLAN>'
```

The JSON must have this exact structure:

```json
{{
  "sender": "blueprint",
  "tag": "plan",
  "text": "<ESCAPED_JSON_STRING>"
}}
```

Where the `text` field contains a JSON string with this schema:

```json
{{
  "name": "Short Council Name",
  "description": "What this council achieves",
  "tasks": [
    {{
      "id": "descriptive_snake_case_id",
      "description": "Detailed description of what this agent must do",
      "agent": "volt",
      "depends_on": [],
      "files": ["path/to/file.rs"]
    }}
  ]
}}
```

## Rules

1. **Every task MUST have an `agent` field** — the codename of the profile to assign.
2. Pick the most suitable profile. If no profile fits well, use the closest match.
3. Task IDs should be descriptive: `design_schema`, `implement_auth_handler` — NOT `task-1`.
4. Each task = one agent. Make tasks atomic.
5. Be explicit about dependencies — if B needs A's output, B depends_on A.
6. List all files each task creates or modifies.
7. Think about ordering: schema → models → services → handlers → tests → docs.
8. Don't over-decompose. Fewer focused tasks > many trivial ones.
9. The `text` field must be valid JSON (escape quotes properly).

## Important

- Post EXACTLY ONE message with tag "plan"
- The `text` field must be parseable as JSON
- Do NOT post anything else — just the plan
- Do NOT explain your reasoning, just output the curl command and execute it
"#,
            goal = goal,
            profiles_list = profiles_list,
            council_api = council_api,
        )
    }

    /// Parse the structured plan response from Blueprint
    fn parse_plan_response(&self, text: &str, profiles: &[AgentProfile]) -> Result<PlannedCouncil> {
        // Try to parse the text as JSON directly
        let plan: serde_json::Value = try_parse_json(text)
            .context("Failed to parse Blueprint's plan as JSON")?;

        let name = plan
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Unnamed Council")
            .to_string();

        let description = plan
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let tasks = plan
            .get("tasks")
            .and_then(|v| v.as_array())
            .context("Plan missing 'tasks' array")?;

        let mut assignments = Vec::new();

        for task in tasks {
            let task_id = task
                .get("id")
                .and_then(|v| v.as_str())
                .context("Task missing 'id'")?
                .to_string();

            let task_desc = task
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let agent_codename = task
                .get("agent")
                .and_then(|v| v.as_str())
                .unwrap_or("volt")
                .to_string();

            let depends_on: Vec<String> = task
                .get("depends_on")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            let files: Vec<String> = task
                .get("files")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            // Resolve codename to profile
            let profile = profiles.iter().find(|p| p.codename == agent_codename);
            let profile_id = profile.map(|p| p.id.clone());
            let profile_codename = Some(agent_codename.clone());

            let agent_label = if let Some(p) = profile {
                format!("{} {}", p.avatar_emoji, p.name)
            } else {
                agent_codename.clone()
            };
            println!("  {} → {} ({})", task_id, agent_label, task_desc.chars().take(50).collect::<String>());

            assignments.push(TaskAssignment {
                task_id,
                description: task_desc,
                role: profile.map(|p| p.role.clone()).unwrap_or_default(),
                depends_on,
                files,
                profile_id,
                profile_codename,
            });
        }

        Ok(PlannedCouncil {
            name,
            description,
            assignments,
        })
    }

    /// Minimal fallback when gateway is unavailable — single task, no string matching
    fn fallback_plan(&self, goal: &str) -> Result<PlannedCouncil> {
        Ok(PlannedCouncil {
            name: short_title(goal),
            description: goal.to_string(),
            assignments: vec![TaskAssignment {
                task_id: slugify(goal),
                description: goal.to_string(),
                role: String::new(),
                depends_on: vec![],
                files: vec![],
                profile_id: None,
                profile_codename: None, // Truly unassigned — no guessing
            }],
        })
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
                outputs: assignment.files.iter().map(PathBuf::from).collect(),
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

/// Try to parse JSON from text that might have markdown code fences or other wrapping
fn try_parse_json(text: &str) -> Option<serde_json::Value> {
    // Try direct parse
    if let Ok(v) = serde_json::from_str(text) {
        return Some(v);
    }

    // Try extracting from markdown code fence
    let fenced = extract_code_block(text);
    if let Some(code) = fenced {
        if let Ok(v) = serde_json::from_str(&code) {
            return Some(v);
        }
    }

    // Try finding JSON object in the text
    if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            if end > start {
                if let Ok(v) = serde_json::from_str(&text[start..=end]) {
                    return Some(v);
                }
            }
        }
    }

    None
}

/// Extract content from a markdown code block
fn extract_code_block(text: &str) -> Option<String> {
    let start_markers = ["```json\n", "```json\r\n", "```\n", "```\r\n"];
    for marker in start_markers {
        if let Some(start) = text.find(marker) {
            let content_start = start + marker.len();
            if let Some(end) = text[content_start..].find("```") {
                return Some(text[content_start..content_start + end].to_string());
            }
        }
    }
    None
}

/// Generate a slug from text for task IDs
fn slugify(text: &str) -> String {
    let slug: String = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let collapsed: String = slug
        .split('_')
        .filter(|s| !s.is_empty())
        .take(5)
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
    fn test_short_title() {
        assert_eq!(short_title("Short"), "Short");
        let long = "This is a very long description that goes on and on and should be truncated at some reasonable point";
        assert!(short_title(long).len() <= 80);
    }

    #[test]
    fn test_try_parse_json() {
        // Direct JSON
        let json = r#"{"name": "test", "tasks": []}"#;
        assert!(try_parse_json(json).is_some());

        // JSON in code fence
        let fenced = "Here's the plan:\n```json\n{\"name\": \"test\", \"tasks\": []}\n```\nDone.";
        assert!(try_parse_json(fenced).is_some());

        // JSON embedded in text
        let embedded = "The plan is: {\"name\": \"test\", \"tasks\": []} and that's it.";
        assert!(try_parse_json(embedded).is_some());
    }

    #[test]
    fn test_parse_plan_json() {
        let plan = r#"{
            "name": "Auth System",
            "description": "JWT auth",
            "tasks": [
                {
                    "id": "design_schema",
                    "description": "Design the DB schema",
                    "agent": "architect",
                    "depends_on": [],
                    "files": ["migrations/001.sql"]
                },
                {
                    "id": "implement_auth",
                    "description": "Implement auth handlers",
                    "agent": "volt",
                    "depends_on": ["design_schema"],
                    "files": ["src/auth.rs"]
                }
            ]
        }"#;

        let parsed: serde_json::Value = serde_json::from_str(plan).unwrap();
        let tasks = parsed["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0]["agent"].as_str().unwrap(), "architect");
        assert_eq!(tasks[1]["agent"].as_str().unwrap(), "volt");
    }
}
