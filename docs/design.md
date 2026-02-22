# dare.run — Design Document

> **Version:** 0.2.0  
> **Author:** Ngonidzashe Mangudya (@iamngoni)  
> **Date:** 2026-02-22

## Overview

**dare** (Shona: "council" — a gathering where decisions are made collectively) is a multi-agent orchestration tool for OpenClaw/Claude agents. It transforms complex tasks into dependency graphs and executes them through coordinated parallel workers.

### Vision

Software development tasks are inherently parallelizable. A PRD describing a new feature can be decomposed into:
- Database migrations (independent)
- API endpoints (depends on migrations)
- Frontend components (some independent, some depend on API)
- Tests (depend on implementation)
- Documentation (depends on everything)

dare.run makes this decomposition and parallel execution automatic. Instead of one agent working sequentially, a council of agents collaborate — each tackling their piece while the orchestrator observes progress passively.

---

## Key Design Principle: Passive Observation

**Agents work naturally — dare observes passively.**

Unlike traditional orchestration systems that require agents to emit special markers or follow protocols, dare uses **passive observation** through the OpenClaw gateway:

### How It Works

1. **Session Lifecycle = Task Lifecycle** — When a spawned session completes (success or failure), the task is done. The gateway tells us the outcome.

2. **Gateway Events** — We monitor WebSocket events:
   - `agent` events for state changes (spawning, running, completed)
   - `presence` events for session activity
   - `chat` events for message content (optional logging)

3. **File Scope Tracking** — The orchestrator tracks which files belong to each task based on the plan. No agent coordination needed — we prevent conflicts by not assigning overlapping files to concurrent tasks.

4. **Natural Agent Behavior** — Agents receive their task and context, then work exactly as they would for any normal task. No special output format, no markers, no protocol.

### Benefits

- **Zero agent overhead** — Agents don't need to learn anything special
- **Works with any model** — No prompt engineering for markers
- **Robust** — Can't fail due to missing/malformed markers
- **Simpler** — Less code, fewer edge cases

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                         dare CLI                                 │
│  dare run | dare plan | dare status | dare logs | dare kill     │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                      Orchestrator Core                           │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐  │
│  │ DAG Planner │  │Wave Executor│  │    Result Bus           │  │
│  │             │  │             │  │  (inter-task outputs)   │  │
│  └─────────────┘  └─────────────┘  └─────────────────────────┘  │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐  │
│  │ File Scope  │  │State Manager│  │  OpenClaw Gateway       │  │
│  │  Tracker    │  │  (SQLite)   │  │  Client (WebSocket)     │  │
│  └─────────────┘  └─────────────┘  └─────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
                              │
              ┌───────────────┼───────────────┐
              ▼               ▼               ▼
        ┌──────────┐   ┌──────────┐    ┌──────────┐
        │ Agent 1  │   │ Agent 2  │    │ Agent N  │
        │ (worker) │   │ (worker) │    │ (worker) │
        └──────────┘   └──────────┘    └──────────┘
              │ Works naturally, no special protocol
              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Web Dashboard (HTMX)                          │
│         Real-time view • SSE updates • Task management           │
└─────────────────────────────────────────────────────────────────┘
```

### Components

1. **CLI** — User interface for running, monitoring, and controlling tasks
2. **Orchestrator Core** — The brain that coordinates everything
3. **DAG Planner** — Decomposes PRDs into dependency graphs (via LLM or YAML)
4. **Wave Executor** — Runs independent tasks in parallel waves
5. **Result Bus** — Makes task outputs available to dependent tasks
6. **File Scope Tracker** — Tracks which files each task owns (prevents conflicts)
7. **State Manager** — SQLite-backed persistence
8. **OpenClaw Gateway Client** — Spawns agents, monitors completion via WebSocket
9. **Web Dashboard** — Real-time visualization

---

## Core Concepts

### 1. DAG Planner

The DAG (Directed Acyclic Graph) Planner analyzes a PRD or task description and produces a dependency graph.

#### Input Format

```yaml
# dare.yaml
name: "Add user authentication"
description: |
  Implement JWT-based authentication with login, logout, and protected routes.

tasks:
  - id: migration
    description: "Create users table migration"
    files: [migrations/001_users.sql]
    
  - id: user_model
    description: "Implement User model with password hashing"
    depends_on: [migration]
    files: [src/models/user.rs]
    
  - id: auth_routes
    description: "Implement /login, /logout, /me endpoints"
    depends_on: [user_model]
    files: [src/routes/auth.rs]
    
  - id: middleware
    description: "JWT validation middleware"
    depends_on: [user_model]
    files: [src/middleware/auth.rs]
    
  - id: tests
    description: "Integration tests for auth flow"
    depends_on: [auth_routes, middleware]
    files: [tests/auth_test.rs]
```

Note: `files` declares the scope of each task. The orchestrator ensures no two concurrent tasks work on the same files.

#### Auto-Planning Mode

When given a raw PRD (no explicit tasks), dare uses an LLM to decompose it:

```bash
dare plan --auto "Add user authentication with JWT, including login, logout, and protected routes"
```

This spawns a "planner agent" that:
1. Analyzes the codebase structure
2. Identifies required changes
3. Infers dependencies based on file relationships
4. Outputs a YAML task plan

#### DAG Representation

```rust
pub struct TaskNode {
    pub id: TaskId,
    pub description: String,
    pub depends_on: Vec<TaskId>,
    pub files: Vec<PathBuf>,  // Files this task owns
    pub status: TaskStatus,
}

pub struct TaskGraph {
    pub nodes: HashMap<TaskId, TaskNode>,
    pub edges: Vec<(TaskId, TaskId)>, // (from, to) = dependency
}
```

### 2. Wave Executor

The Wave Executor runs tasks in parallel waves. Each wave contains all tasks whose dependencies are satisfied.

```
Wave 0: [migration]           ─────────────► Done
Wave 1: [user_model]          ─────────────► Done  
Wave 2: [auth_routes, middleware]  ────────► Done (parallel!)
Wave 3: [tests]               ─────────────► Done
```

#### Execution Flow

```rust
async fn execute_waves(graph: &TaskGraph, gateway: &GatewayClient) {
    let mut completed: HashSet<TaskId> = HashSet::new();
    
    loop {
        // Find all tasks with satisfied dependencies
        let ready: Vec<&TaskNode> = graph.nodes.values()
            .filter(|n| n.status == Pending)
            .filter(|n| n.depends_on.iter().all(|dep| completed.contains(dep)))
            .collect();
        
        if ready.is_empty() {
            break; // All done
        }
        
        // Spawn all ready tasks in parallel via gateway
        let sessions: Vec<_> = ready.iter()
            .map(|task| gateway.spawn_agent(task))
            .collect();
        
        // Monitor sessions via gateway WebSocket
        // Sessions complete naturally - we just observe
        for session in sessions {
            match gateway.wait_for_completion(&session).await {
                Completed => completed.insert(session.task_id),
                Failed(e) => handle_failure(session.task_id, e),
            }
        }
    }
}
```

#### Concurrency Limits

```toml
# dare.toml
[execution]
max_parallel_agents = 4      # Don't overwhelm the system
wave_timeout_seconds = 300   # Per-wave timeout
task_timeout_seconds = 120   # Per-task timeout
```

### 3. OpenClaw Gateway Integration

dare connects to OpenClaw via WebSocket for real-time session monitoring.

#### Authentication

Uses the same Ed25519 device identity as OpenClaw CLI:
- Device identity: `~/.openclaw/identity/device.json`
- Auth token: `~/.openclaw/identity/device-auth.json`
- Gateway password: `~/.openclaw/openclaw.json`

```rust
pub struct GatewayClient {
    ws: WebSocketStream,
    pending_sessions: HashMap<SessionKey, TaskId>,
}

impl GatewayClient {
    /// Spawn a subagent via the gateway
    pub async fn spawn_session(&self, task: &TaskNode, context: &str) -> Result<SessionKey> {
        self.request("sessions.spawn", json!({
            "label": format!("dare-{}", task.id),
            "message": context,
            "model": None,  // Use default
        })).await
    }
    
    /// Monitor for session completion events
    /// Returns when session completes (success or failure)
    pub async fn wait_for_completion(&self, session: &SessionKey) -> SessionResult {
        // Listen for agent events on WebSocket
        // Session state: spawned -> running -> completed/failed
        loop {
            match self.next_event().await {
                Event::Agent { session_key, state: "completed" } if session_key == session => {
                    return SessionResult::Completed;
                }
                Event::Agent { session_key, state: "failed", error } if session_key == session => {
                    return SessionResult::Failed(error);
                }
                _ => continue,
            }
        }
    }
}
```

### 4. Result Bus (Inter-Task Data Passing)

When tasks depend on each other, the dependent task needs context from completed tasks.

```rust
pub struct ResultBus {
    results: HashMap<TaskId, TaskResult>,
}

pub struct TaskResult {
    pub task_id: TaskId,
    pub files_modified: Vec<PathBuf>,
    pub summary: String,  // Brief summary of what was done
}

impl ResultBus {
    /// Store result when task completes
    pub fn store(&mut self, task_id: TaskId, result: TaskResult) {
        self.results.insert(task_id, result);
    }
    
    /// Get results for a task's dependencies
    pub fn get_dependency_context(&self, task: &TaskNode) -> String {
        let mut context = String::new();
        for dep_id in &task.depends_on {
            if let Some(result) = self.results.get(dep_id) {
                context.push_str(&format!(
                    "## {} (completed)\n{}\nFiles: {:?}\n\n",
                    dep_id, result.summary, result.files_modified
                ));
            }
        }
        context
    }
}
```

### 5. File Scope Tracking

Prevents concurrent tasks from modifying the same files.

```rust
pub struct FileScopeTracker {
    // Maps file paths to the task that owns them
    file_owners: HashMap<PathBuf, TaskId>,
}

impl FileScopeTracker {
    /// Check if a task can be scheduled (no file conflicts)
    pub fn can_schedule(&self, task: &TaskNode, active_tasks: &[TaskId]) -> bool {
        for file in &task.files {
            if let Some(owner) = self.file_owners.get(file) {
                if active_tasks.contains(owner) {
                    return false;  // File is locked by active task
                }
            }
        }
        true
    }
    
    /// Claim files for a task
    pub fn claim(&mut self, task: &TaskNode) {
        for file in &task.files {
            self.file_owners.insert(file.clone(), task.id.clone());
        }
    }
    
    /// Release files when task completes
    pub fn release(&mut self, task_id: &TaskId) {
        self.file_owners.retain(|_, owner| owner != task_id);
    }
}
```

---

## Agent Context (What Agents Receive)

Agents receive a simple, natural prompt:

```markdown
# Task: {task_id}

{task_description}

## Files You're Working On
{file_list}

## Context from Completed Tasks
{dependency_results}

## Instructions
Complete this task. When you're done, the task is complete — no special markers needed.
Just do your work naturally and ensure the files listed above are properly created/modified.
```

**Note:** No special protocol, no markers, no coordination requirements. The agent works exactly as it would for any normal task.

---

## CLI Interface

### Commands

```bash
# Run a task/PRD
dare run task.yaml              # From file
dare run --auto "Build a REST API for todos"  # Auto-plan from description
dare run --continue run_abc123  # Resume interrupted run

# Planning
dare plan task.yaml             # Show execution plan without running
dare plan --auto "..."          # Auto-generate plan
dare plan --validate            # Check for cycles, missing deps

# Monitoring
dare status                     # Current run status
dare status run_abc123          # Specific run
dare status --watch             # Live updates (TUI)

# Logs
dare logs                       # All logs
dare logs --task migration      # Specific task
dare logs --follow              # Stream logs

# Control
dare pause                      # Pause after current wave
dare resume                     # Resume paused run
dare kill                       # Kill all agents, abort run
dare kill --task auth_routes    # Kill specific task

# Dashboard
dare dashboard                  # Open web dashboard
dare dashboard --port 8080      # Custom port

# Configuration
dare init                       # Create dare.toml in current dir
dare config                     # Show current config
```

---

## Web Dashboard

### Tech Stack
- **Backend:** Rust (Axum)
- **Frontend:** HTMX + Tailwind CSS
- **Real-time:** Server-Sent Events (SSE)

### Views

1. **Run Overview** — Current run status, wave progress, task list
2. **DAG Visualization** — Interactive graph view with status colors
3. **Task Monitor** — Per-task status, files, time elapsed
4. **Logs View** — Aggregated logs from all agents

---

## Data Model (SQLite)

### Schema

```sql
-- Runs: top-level execution units
CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    started_at DATETIME,
    completed_at DATETIME,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Tasks: individual work items
CREATE TABLE tasks (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    description TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    wave INTEGER NOT NULL,
    session_key TEXT,  -- OpenClaw session key
    files_json TEXT,   -- Files this task owns
    result_json TEXT,  -- Task result/summary
    started_at DATETIME,
    completed_at DATETIME,
    error TEXT,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Task dependencies
CREATE TABLE task_dependencies (
    task_id TEXT NOT NULL REFERENCES tasks(id),
    depends_on TEXT NOT NULL REFERENCES tasks(id),
    PRIMARY KEY (task_id, depends_on)
);
```

---

## Configuration

### dare.toml

```toml
[general]
workspace = "."
database = ".dare/dare.db"

[execution]
max_parallel_agents = 4
wave_timeout_seconds = 300
task_timeout_seconds = 120

[gateway]
url = "ws://127.0.0.1:18789"

[planning]
model = "claude-sonnet-4-20250514"

[dashboard]
enabled = true
port = 8765
open_browser = true
```

---

## Future Enhancements

### Phase 2
- **Smart retry** — Analyze failures, adjust approach
- **Cost tracking** — Token usage per task
- **Progress estimation** — Based on file complexity

### Phase 3
- **Git integration** — Auto-commit per task, branch per run
- **Caching** — Skip unchanged tasks
- **Templates** — Reusable task patterns

---

*End of Design Document*
