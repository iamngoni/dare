# dare.run — Design Document

> **Version:** 0.1.0  
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

dare.run makes this decomposition and parallel execution automatic. Instead of one agent working sequentially, a council of agents collaborate — each tackling their piece while staying coordinated through a shared message bus.

### Inspiration

- [pi-messenger](https://x.com/nicopreme/status/2019523000866074830) — agents coordinating in a shared chat room
- Make/Ninja build systems — DAG-based parallel execution
- Kubernetes — declarative state, reconciliation loops
- Git — file locking and conflict resolution

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
│  │ DAG Planner │  │Wave Executor│  │    Message Bus          │  │
│  │             │  │             │  │  (pub/sub + direct)     │  │
│  └─────────────┘  └─────────────┘  └─────────────────────────┘  │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐  │
│  │ File Locker │  │State Manager│  │  OpenClaw Gateway       │  │
│  │             │  │  (SQLite)   │  │  Integration            │  │
│  └─────────────┘  └─────────────┘  └─────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
                              │
              ┌───────────────┼───────────────┐
              ▼               ▼               ▼
        ┌──────────┐   ┌──────────┐    ┌──────────┐
        │ Agent 1  │   │ Agent 2  │    │ Agent N  │
        │ (worker) │   │ (worker) │    │ (worker) │
        └──────────┘   └──────────┘    └──────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Web Dashboard (HTMX)                          │
│         Real-time view • SSE updates • Task management           │
└─────────────────────────────────────────────────────────────────┘
```

### Components

1. **CLI** — User interface for running, monitoring, and controlling tasks
2. **Orchestrator Core** — The brain that coordinates everything
3. **DAG Planner** — Decomposes PRDs into dependency graphs
4. **Wave Executor** — Runs independent tasks in parallel waves
5. **Message Bus** — Inter-agent communication channel
6. **File Locker** — Prevents conflicts on shared resources
7. **State Manager** — SQLite-backed persistence
8. **OpenClaw Gateway Integration** — Spawns and controls agents
9. **Web Dashboard** — Real-time visualization

---

## Core Concepts

### 1. DAG Planner

The DAG (Directed Acyclic Graph) Planner analyzes a PRD or task description and produces a dependency graph.

#### Input Format

```yaml
# dare.yaml or inline PRD
name: "Add user authentication"
description: |
  Implement JWT-based authentication with login, logout, and protected routes.

tasks:
  - id: migration
    description: "Create users table migration"
    outputs: [migrations/001_users.sql]
    
  - id: user_model
    description: "Implement User model with password hashing"
    depends_on: [migration]
    outputs: [src/models/user.rs]
    
  - id: auth_routes
    description: "Implement /login, /logout, /me endpoints"
    depends_on: [user_model]
    outputs: [src/routes/auth.rs]
    
  - id: middleware
    description: "JWT validation middleware"
    depends_on: [user_model]
    outputs: [src/middleware/auth.rs]
    
  - id: tests
    description: "Integration tests for auth flow"
    depends_on: [auth_routes, middleware]
    outputs: [tests/auth_test.rs]
```

#### Auto-Planning Mode

When given a raw PRD (no explicit tasks), dare uses an LLM to decompose it:

```bash
dare plan --auto "Add user authentication with JWT, including login, logout, and protected routes"
```

This spawns a "planner agent" that:
1. Analyzes the codebase structure
2. Identifies required changes
3. Infers dependencies based on file relationships
4. Outputs a `dare.yaml`

#### DAG Representation

```rust
pub struct TaskNode {
    pub id: TaskId,
    pub description: String,
    pub depends_on: Vec<TaskId>,
    pub outputs: Vec<PathBuf>,
    pub estimated_complexity: Complexity, // S, M, L, XL
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

#### Algorithm

```rust
fn execute_waves(graph: &TaskGraph) {
    let mut completed: HashSet<TaskId> = HashSet::new();
    
    loop {
        // Find all tasks with satisfied dependencies
        let ready: Vec<&TaskNode> = graph.nodes.values()
            .filter(|n| n.status == Pending)
            .filter(|n| n.depends_on.iter().all(|dep| completed.contains(dep)))
            .collect();
        
        if ready.is_empty() {
            break; // All done or deadlock
        }
        
        // Spawn all ready tasks in parallel
        let handles: Vec<_> = ready.iter()
            .map(|task| spawn_agent(task))
            .collect();
        
        // Wait for wave to complete
        for handle in handles {
            let result = handle.await;
            completed.insert(result.task_id);
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

### 3. Message Bus

Agents communicate through a shared message bus. This enables:
- **Status updates** — "I'm 50% done with auth_routes"
- **Help requests** — "I need the User struct signature"
- **Result sharing** — "Migration complete, schema is X"
- **Conflict alerts** — "I need to modify the same file"

#### Message Types

```rust
pub enum BusMessage {
    // Status
    Progress { task_id: TaskId, percent: u8, detail: String },
    Completed { task_id: TaskId, outputs: Vec<PathBuf> },
    Failed { task_id: TaskId, error: String },
    
    // Coordination
    HelpRequest { from: TaskId, question: String },
    HelpResponse { to: TaskId, answer: String },
    
    // File operations
    FileLockRequest { task_id: TaskId, path: PathBuf },
    FileLockGranted { task_id: TaskId, path: PathBuf },
    FileLockDenied { task_id: TaskId, path: PathBuf, held_by: TaskId },
    FileReleased { task_id: TaskId, path: PathBuf },
    
    // System
    Broadcast { message: String },
    AgentLog { task_id: TaskId, level: LogLevel, message: String },
}
```

#### Implementation

The message bus is implemented using:
1. **SQLite** for persistence (messages survive crashes)
2. **In-memory broadcast** for real-time delivery
3. **SSE** for web dashboard updates

```rust
pub struct MessageBus {
    db: SqlitePool,
    subscribers: HashMap<TaskId, mpsc::Sender<BusMessage>>,
    broadcast_tx: broadcast::Sender<BusMessage>,
}

impl MessageBus {
    pub async fn publish(&self, msg: BusMessage) {
        // Persist
        self.db.insert_message(&msg).await;
        
        // Broadcast to all subscribers
        let _ = self.broadcast_tx.send(msg.clone());
        
        // Direct delivery if addressed
        if let Some(target) = msg.target() {
            if let Some(tx) = self.subscribers.get(&target) {
                let _ = tx.send(msg).await;
            }
        }
    }
    
    pub fn subscribe(&mut self, task_id: TaskId) -> mpsc::Receiver<BusMessage> {
        let (tx, rx) = mpsc::channel(100);
        self.subscribers.insert(task_id, tx);
        rx
    }
}
```

### 4. File Locking

Prevents multiple agents from modifying the same file simultaneously.

#### Lock Types

- **Exclusive** — Only one agent can hold (for writes)
- **Shared** — Multiple agents can hold (for reads)

#### Protocol

```
Agent A: FileLockRequest(src/lib.rs, Exclusive)
Bus:     FileLockGranted(A, src/lib.rs)
Agent A: [modifies file]
Agent A: FileReleased(src/lib.rs)

Agent B: FileLockRequest(src/lib.rs, Exclusive)  // While A holds
Bus:     FileLockDenied(B, src/lib.rs, held_by=A)
Agent B: [waits or works on something else]
```

#### Deadlock Prevention

1. **Timeout-based release** — Locks auto-release after 60s
2. **Lock ordering** — Agents must acquire locks in alphabetical path order
3. **Voluntary yield** — Agents can release locks early if blocked too long

```rust
pub struct FileLock {
    pub path: PathBuf,
    pub held_by: TaskId,
    pub lock_type: LockType,
    pub acquired_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}
```

---

## Agent Lifecycle

### States

```
┌─────────┐     ┌─────────┐     ┌───────────┐     ┌───────────┐
│ PENDING │────►│ SPAWNED │────►│ EXECUTING │────►│ COMPLETED │
└─────────┘     └─────────┘     └───────────┘     └───────────┘
                    │                 │
                    │                 ▼
                    │           ┌──────────┐
                    └──────────►│  FAILED  │
                                └──────────┘
```

### Spawn Process

1. **Prepare context** — Gather relevant files, dependency outputs
2. **Create session** — Call OpenClaw `sessions_spawn`
3. **Inject instructions** — Task description + coordination protocol
4. **Monitor** — Watch for messages, progress updates
5. **Collect results** — Verify outputs, update state

### Agent Instructions Template

Each agent receives a system prompt:

```markdown
# dare.run Worker Agent

You are agent `{task_id}` in a coordinated multi-agent task.

## Your Task
{task_description}

## Expected Outputs
{outputs_list}

## Coordination Protocol

### Message Bus
You can communicate with the orchestrator and sibling agents:
- To report progress: `[DARE:PROGRESS:50] Halfway done with schema`
- To request help: `[DARE:HELP] What's the User struct signature?`
- To log: `[DARE:LOG:INFO] Starting implementation`
- When complete: `[DARE:COMPLETE] Finished successfully`
- On failure: `[DARE:FAILED] Error: {reason}`

### File Locking
Before modifying a file:
1. Request: `[DARE:LOCK:src/lib.rs]`
2. Wait for: `[DARE:LOCK_OK:src/lib.rs]` or `[DARE:LOCK_DENIED:src/lib.rs:held_by_task_xyz]`
3. After done: `[DARE:UNLOCK:src/lib.rs]`

### Context from Dependencies
{dependency_outputs}

## Rules
1. Only modify files in your outputs list unless coordinating
2. Report progress at least every 30 seconds
3. If stuck >60s, request help
4. Always complete or fail explicitly — don't hang
```

### Result Collection

When an agent completes:
1. Verify all expected outputs exist
2. Run any validation (syntax check, tests)
3. Store outputs in state for dependent tasks
4. Update DAG status
5. Trigger next wave if applicable

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

### Output Examples

```bash
$ dare run auth.yaml

dare.run v0.1.0
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

📋 Plan: Add user authentication
   5 tasks • 4 waves • Est. 3-5 min

Wave 0 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  ✓ migration (12s)

Wave 1 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  ⠋ user_model (running 8s)

Wave 2 (pending) ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  ○ auth_routes
  ○ middleware

Wave 3 (pending) ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  ○ tests

Press 'd' for dashboard • 'l' for logs • 'q' to quit
```

---

## Web Dashboard

### Tech Stack
- **Backend:** Rust (Axum)
- **Frontend:** HTMX + Tailwind CSS
- **Real-time:** Server-Sent Events (SSE)

### Views

#### 1. Run Overview
- Current run status
- Wave progress visualization
- Task list with status indicators
- Elapsed time, ETA

#### 2. DAG Visualization
- Interactive graph view
- Node colors by status (pending/running/done/failed)
- Click to see task details
- Dependency arrows

#### 3. Agent Monitor
- Per-agent logs (streaming)
- Progress bars
- File locks held
- Message bus activity

#### 4. Message Bus View
- Chronological message log
- Filter by task, type
- Help requests highlighted

### Wireframe

```
┌─────────────────────────────────────────────────────────────────┐
│  dare.run                                    [Pause] [Kill]     │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Run: Add user authentication          Status: ● Running        │
│  Started: 2 min ago                     Wave: 2/4               │
│                                                                  │
│  ┌─────────────────────────────────────────────────────────────┐│
│  │                    DAG Visualization                        ││
│  │                                                             ││
│  │         [migration]                                         ││
│  │              │                                              ││
│  │              ▼                                              ││
│  │        [user_model]                                         ││
│  │           /    \                                            ││
│  │          ▼      ▼                                           ││
│  │  [auth_routes] [middleware]                                 ││
│  │          \      /                                           ││
│  │           ▼    ▼                                            ││
│  │          [tests]                                            ││
│  │                                                             ││
│  └─────────────────────────────────────────────────────────────┘│
│                                                                  │
│  Tasks                                                           │
│  ├─ ✓ migration ─────────────────────────── 12s                 │
│  ├─ ✓ user_model ────────────────────────── 45s                 │
│  ├─ ⠋ auth_routes ───────────────── [████░░░░░░] 40%           │
│  ├─ ⠋ middleware ────────────────── [██████░░░░] 60%           │
│  └─ ○ tests ─────────────────────────────── pending             │
│                                                                  │
│  Message Bus (live)                                              │
│  ┌─────────────────────────────────────────────────────────────┐│
│  │ 14:23:45 [auth_routes] PROGRESS 40% - Implementing /login   ││
│  │ 14:23:42 [middleware]  PROGRESS 60% - JWT validation done   ││
│  │ 14:23:30 [middleware]  LOCK_OK src/middleware/mod.rs        ││
│  │ 14:23:28 [auth_routes] HELP How do I hash passwords in Rust?││
│  └─────────────────────────────────────────────────────────────┘│
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## OpenClaw Gateway Integration

### Connection

dare connects to the OpenClaw gateway via WebSocket:

```rust
pub struct GatewayClient {
    ws: WebSocketStream<...>,
    base_url: String,
}

impl GatewayClient {
    pub async fn connect(port: u16) -> Result<Self> {
        let url = format!("ws://localhost:{}/ws", port);
        let ws = connect_async(&url).await?;
        Ok(Self { ws, base_url: format!("http://localhost:{}", port) })
    }
    
    pub async fn spawn_agent(&self, task: &TaskNode, context: &str) -> Result<SessionId> {
        // Use sessions_spawn equivalent
        let req = SpawnRequest {
            label: format!("dare-{}", task.id),
            message: context,
            model: None, // Use default
        };
        // Send via gateway
        ...
    }
    
    pub async fn send_message(&self, session: SessionId, msg: &str) -> Result<()> {
        // Use sessions_send equivalent
        ...
    }
    
    pub async fn list_sessions(&self) -> Result<Vec<Session>> {
        // Use subagents list equivalent
        ...
    }
    
    pub async fn kill_session(&self, session: SessionId) -> Result<()> {
        // Use subagents kill equivalent
        ...
    }
}
```

### Message Parsing

Agents communicate via structured text markers in their output:

```rust
pub fn parse_agent_output(output: &str) -> Vec<BusMessage> {
    let mut messages = vec![];
    
    for line in output.lines() {
        if let Some(caps) = PROGRESS_RE.captures(line) {
            messages.push(BusMessage::Progress {
                percent: caps["pct"].parse().unwrap(),
                detail: caps["detail"].to_string(),
            });
        }
        // ... other patterns
    }
    
    messages
}

lazy_static! {
    static ref PROGRESS_RE: Regex = 
        Regex::new(r"\[DARE:PROGRESS:(\d+)\]\s*(.*)").unwrap();
    static ref COMPLETE_RE: Regex = 
        Regex::new(r"\[DARE:COMPLETE\]").unwrap();
    static ref FAILED_RE: Regex = 
        Regex::new(r"\[DARE:FAILED\]\s*(.*)").unwrap();
    // ...
}
```

---

## Data Model (SQLite)

### Schema

```sql
-- Runs: top-level execution units
CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    status TEXT NOT NULL DEFAULT 'pending',  -- pending, running, paused, completed, failed
    config_json TEXT,  -- serialized RunConfig
    started_at DATETIME,
    completed_at DATETIME,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Tasks: individual work items
CREATE TABLE tasks (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    description TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',  -- pending, spawned, executing, completed, failed
    wave INTEGER NOT NULL,  -- which wave this task belongs to
    agent_session_id TEXT,  -- OpenClaw session ID when spawned
    outputs_json TEXT,  -- expected output paths
    result_json TEXT,  -- actual results
    started_at DATETIME,
    completed_at DATETIME,
    error TEXT,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Task dependencies
CREATE TABLE task_dependencies (
    task_id TEXT NOT NULL REFERENCES tasks(id),
    depends_on_task_id TEXT NOT NULL REFERENCES tasks(id),
    PRIMARY KEY (task_id, depends_on_task_id)
);

-- Message bus log
CREATE TABLE messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL REFERENCES runs(id),
    task_id TEXT REFERENCES tasks(id),  -- NULL for broadcast
    message_type TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX idx_messages_run ON messages(run_id);
CREATE INDEX idx_messages_task ON messages(task_id);

-- File locks
CREATE TABLE file_locks (
    path TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    task_id TEXT NOT NULL REFERENCES tasks(id),
    lock_type TEXT NOT NULL,  -- exclusive, shared
    acquired_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at DATETIME NOT NULL
);
CREATE INDEX idx_locks_run ON file_locks(run_id);

-- Agent logs (for debugging)
CREATE TABLE agent_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL REFERENCES runs(id),
    task_id TEXT NOT NULL REFERENCES tasks(id),
    level TEXT NOT NULL,  -- debug, info, warn, error
    message TEXT NOT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX idx_agent_logs_task ON agent_logs(task_id);
```

### Indexes for Performance

```sql
CREATE INDEX idx_tasks_run_status ON tasks(run_id, status);
CREATE INDEX idx_tasks_wave ON tasks(run_id, wave);
CREATE INDEX idx_messages_created ON messages(created_at);
```

---

## Security Considerations

### 1. Agent Sandboxing

Agents inherit OpenClaw's security model:
- File system access controlled by workspace config
- Network access controlled by policy
- No direct shell access unless explicitly enabled

### 2. File Lock Abuse Prevention

- Lock TTL (60s default) prevents indefinite holds
- Lock audit log for debugging
- Admin can force-release locks

### 3. Resource Limits

```toml
[security]
max_agents_per_run = 10
max_concurrent_runs = 3
max_run_duration_minutes = 60
max_file_size_mb = 10
```

### 4. Input Validation

- Task descriptions sanitized before agent injection
- File paths validated (no path traversal)
- DAG validated for cycles before execution

### 5. Audit Trail

All actions logged:
- Run start/stop
- Agent spawn/kill
- File modifications
- Lock acquire/release

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
retry_failed_tasks = true
max_retries = 2

[gateway]
host = "localhost"
port = 18789

[planning]
auto_plan_model = "claude-sonnet-4-20250514"  # For auto-planning
complexity_estimation = true

[dashboard]
enabled = true
port = 8765
open_browser = true

[security]
max_agents_per_run = 10
max_concurrent_runs = 3
max_run_duration_minutes = 60

[logging]
level = "info"
file = ".dare/dare.log"
```

---

## Future Enhancements

### Phase 2
- **Smart retry** — Analyze failures, adjust approach
- **Cost tracking** — Token usage per task
- **Templates** — Reusable task patterns
- **Hooks** — Pre/post task scripts

### Phase 3
- **Distributed execution** — Multiple machines
- **Git integration** — Auto-commit, branch per run
- **CI/CD integration** — Trigger on PR
- **Caching** — Skip unchanged tasks

### Phase 4
- **Learning** — Improve decomposition based on history
- **Collaboration** — Multiple humans coordinating agents
- **Plugin system** — Custom task types

---

## Appendix A: Message Protocol Reference

| Pattern | Description |
|---------|-------------|
| `[DARE:PROGRESS:N]` | Progress update (N = 0-100) |
| `[DARE:COMPLETE]` | Task completed successfully |
| `[DARE:FAILED] reason` | Task failed with reason |
| `[DARE:HELP] question` | Request help from orchestrator |
| `[DARE:LOG:LEVEL] msg` | Log message (DEBUG/INFO/WARN/ERROR) |
| `[DARE:LOCK:path]` | Request file lock |
| `[DARE:UNLOCK:path]` | Release file lock |
| `[DARE:LOCK_OK:path]` | Lock granted (from orchestrator) |
| `[DARE:LOCK_DENIED:path:holder]` | Lock denied (from orchestrator) |

---

## Appendix B: Example Run

### Input PRD

```
Build a REST API for a todo list app with:
- CRUD operations for todos
- SQLite persistence
- JSON API with proper error handling
```

### Auto-generated Plan

```yaml
name: Todo API
tasks:
  - id: db_schema
    description: Create SQLite schema for todos table
    outputs: [migrations/001_todos.sql]
    
  - id: db_module
    description: Database connection and query functions
    depends_on: [db_schema]
    outputs: [src/db.rs]
    
  - id: models
    description: Todo struct and serialization
    outputs: [src/models.rs]
    
  - id: handlers
    description: CRUD HTTP handlers
    depends_on: [db_module, models]
    outputs: [src/handlers.rs]
    
  - id: routes
    description: Route configuration and main.rs
    depends_on: [handlers]
    outputs: [src/routes.rs, src/main.rs]
    
  - id: tests
    description: Integration tests
    depends_on: [routes]
    outputs: [tests/api_test.rs]
```

### Execution Waves

```
Wave 0: [db_schema, models]     # Independent
Wave 1: [db_module]             # Depends on schema
Wave 2: [handlers]              # Depends on db + models
Wave 3: [routes]                # Depends on handlers
Wave 4: [tests]                 # Depends on routes
```

### Timeline

```
00:00  Wave 0 started
00:00  ├─ Agent db_schema spawned
00:00  └─ Agent models spawned
00:15  ├─ models COMPLETE
00:20  └─ db_schema COMPLETE
00:20  Wave 1 started
00:20  └─ Agent db_module spawned
00:45  └─ db_module COMPLETE
00:45  Wave 2 started
00:45  └─ Agent handlers spawned
01:30  └─ handlers COMPLETE
01:30  Wave 3 started
01:30  └─ Agent routes spawned
02:00  └─ routes COMPLETE
02:00  Wave 4 started
02:00  └─ Agent tests spawned
02:45  └─ tests COMPLETE
02:45  Run COMPLETED (2m 45s)
```

---

*End of Design Document*
