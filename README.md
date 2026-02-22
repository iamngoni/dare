# dare.run

> **Dare** (Shona): *council* — a gathering where decisions are made collectively

**Multi-agent orchestration for OpenClaw/Claude agents.**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)

---

## What is dare?

dare.run transforms complex tasks into coordinated agent swarms. Instead of one AI assistant working sequentially through a long task, dare:

1. **Decomposes** your PRD/task into a dependency graph (DAG)
2. **Parallelizes** independent work across multiple agents
3. **Observes** agent progress passively via OpenClaw gateway
4. **Visualizes** progress in real-time via web dashboard

Think of it as `make` for AI agents — but smarter.

## Key Design: Passive Observation

**Agents work naturally — dare observes passively.**

Unlike traditional orchestration that requires agents to emit special markers or follow protocols, dare uses **passive observation** through the OpenClaw gateway:

- **Session lifecycle = task lifecycle** — When a session completes, the task is done
- **Gateway events** — We monitor WebSocket events for state changes
- **File scope tracking** — Orchestrator prevents concurrent file conflicts
- **Natural agent behavior** — No special output format or protocol needed

## Why?

AI agents are powerful but slow. A typical feature implementation might take 10+ minutes with a single agent. But most tasks are parallelizable:

```
Sequential (10 min):
  migration → model → routes → middleware → tests

Parallel with dare (3 min):
  Wave 0: migration ─────────┐
  Wave 1: model ─────────────┤
  Wave 2: routes + middleware┤ (parallel!)
  Wave 3: tests ─────────────┘
```

## Installation

```bash
# From source
cargo install --path .

# Or build locally
cargo build --release
./target/release/dare --help
```

## Quick Start

### 1. Initialize a project

```bash
cd your-project
dare init
```

### 2. Create a task file

```yaml
# dare.yaml
name: "Add user authentication"
description: "JWT-based auth with login, logout, and protected routes"

tasks:
  - id: migration
    description: "Create users table"
    outputs: [migrations/001_users.sql]
    
  - id: user_model
    description: "User model with password hashing"
    depends_on: [migration]
    outputs: [src/models/user.rs]
    
  - id: auth_routes
    description: "Login/logout endpoints"
    depends_on: [user_model]
    outputs: [src/routes/auth.rs]
```

### 3. Preview the plan

```bash
dare plan dare.yaml
```

```
dare.run v0.1.0 — Execution Plan
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

📋 Plan: Add user authentication
📊 Summary: 3 tasks • 3 waves

Wave 1 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  ○ migration 
    Create users table
    → files: migrations/001_users.sql

Wave 2 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  ○ user_model 
    User model with password hashing
    ↳ depends on: migration
    → files: src/models/user.rs

Wave 3 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  ○ auth_routes 
    Login/logout endpoints
    ↳ depends on: user_model
    → files: src/routes/auth.rs
```

### 4. Run it

```bash
dare run dare.yaml
```

### 5. Or let AI do the planning

```bash
dare run --auto "Add user authentication with JWT, login, logout, and protected routes"
```

## Features

### 🎯 DAG-based Execution
Tasks are organized into waves. Independent tasks run in parallel; dependent tasks wait.

### 📡 Passive Observation
Monitor agent sessions via OpenClaw gateway WebSocket — no special agent protocol needed.

### 🔒 File Scope Tracking
Prevents conflicts by tracking which files each task owns. No concurrent access to the same files.

### 📊 Real-time Dashboard
Web-based visualization with HTMX + SSE:
- DAG with live status
- Task progress
- Live event stream
- Pause/resume/kill controls

### ⚡ OpenClaw Integration
Built on OpenClaw's agent infrastructure:
- Ed25519 device authentication
- WebSocket session monitoring
- Subagent spawning

## CLI Reference

```bash
# Execute
dare run <file.yaml>           # Run from task file
dare run --auto "description"  # Auto-plan and run
dare run --continue <run-id>   # Resume interrupted run

# Planning
dare plan <file.yaml>          # Preview execution plan
dare plan --auto "..."         # Generate plan from description
dare plan --validate           # Check for cycles/errors
dare plan --format yaml        # Output as YAML

# Monitoring
dare status                    # Current run status
dare status <run-id>           # Specific run
dare logs                      # View logs
dare logs --follow             # Stream logs
dare logs --task <id>          # Filter by task

# Control
dare pause                     # Pause after current wave
dare resume                    # Resume paused run
dare kill                      # Abort all agents
dare kill --task <id>          # Kill specific task

# Dashboard
dare dashboard                 # Open web UI (default: port 8765)

# Setup
dare init                      # Create dare.toml
dare config                    # Show configuration
```

## Configuration

```toml
# dare.toml
[general]
workspace = "."
database = ".dare/dare.db"

[execution]
max_parallel_agents = 4
wave_timeout_seconds = 300
task_timeout_seconds = 120

[gateway]
host = "localhost"
port = 18789

[planning]
model = "claude-sonnet-4-20250514"

[dashboard]
port = 8765
open_browser = true
```

## Architecture

```
┌──────────────────────────────────────────────────┐
│                   dare CLI                        │
│  run | plan | status | logs | dashboard          │
└──────────────────────────────────────────────────┘
                        │
                        ▼
┌──────────────────────────────────────────────────┐
│              Orchestrator Core                    │
│  ┌────────────┐ ┌────────────┐ ┌──────────────┐  │
│  │ DAG Planner│ │Wave Executor│ │ Result Bus  │  │
│  └────────────┘ └────────────┘ └──────────────┘  │
│  ┌────────────┐ ┌────────────┐ ┌──────────────┐  │
│  │File Tracker│ │State (SQLite)│ │Gateway Client│  │
│  └────────────┘ └────────────┘ └──────────────┘  │
└──────────────────────────────────────────────────┘
                        │
         ┌──────────────┼──────────────┐
         ▼              ▼              ▼
    ┌─────────┐   ┌─────────┐   ┌─────────┐
    │ Agent 1 │   │ Agent 2 │   │ Agent N │
    └─────────┘   └─────────┘   └─────────┘
    (work naturally, no special protocol)
```

## Status

### ✅ Implemented
- [x] Design document (passive observation model)
- [x] DAG planner (manual YAML + auto via LLM)
- [x] Wave executor with parallel execution
- [x] Gateway client (WebSocket + Ed25519 auth)
- [x] Result bus for inter-task context
- [x] File scope tracking
- [x] SQLite persistence (runs, tasks, logs)
- [x] CLI commands (run, plan, status, logs, etc.)
- [x] Web dashboard (HTMX + SSE)
- [x] Pause/resume/kill controls

### 🔜 Planned
- [ ] Smart retry on failures
- [ ] Progress estimation
- [ ] Git integration (auto-commit per task)
- [ ] Cost tracking (token usage)
- [ ] Distributed execution
- [ ] Task templates

## Tech Stack

- **Language:** Rust
- **Web:** Axum + HTMX + Tailwind CSS
- **Database:** SQLite (via sqlx)
- **Real-time:** SSE (Server-Sent Events)
- **Agent Runtime:** OpenClaw Gateway

## Requirements

- OpenClaw installed and configured (`~/.openclaw/identity/device.json`)
- OpenClaw gateway running (`ws://127.0.0.1:18789`)
- Rust 1.75+

## Contributing

Contributions welcome! Please read the [design document](docs/design.md) first to understand the architecture.

## License

MIT License. See [LICENSE](LICENSE).

## Author

**Ngonidzashe Mangudya** ([@iamngoni](https://github.com/iamngoni))

---

*dare.run — Let agents work together, not alone.*
