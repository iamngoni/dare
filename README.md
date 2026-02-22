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
3. **Coordinates** agents through a shared message bus
4. **Visualizes** progress in real-time via TUI and web dashboard

Think of it as `make` for AI agents — but smarter.

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

Agents coordinate through a shared message bus, requesting file locks, sharing intermediate results, and asking siblings for help when stuck.

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

### 3. Run it

```bash
dare run dare.yaml
```

### 4. Or let AI do the planning

```bash
dare run --auto "Add user authentication with JWT, login, logout, and protected routes"
```

## Features

### 🎯 DAG-based Execution
Tasks are organized into waves. Independent tasks run in parallel; dependent tasks wait.

### 💬 Message Bus
Agents communicate through a shared channel:
- Progress updates
- Help requests
- Result sharing
- File lock coordination

### 🔒 File Locking
Prevents conflicts when multiple agents need the same file. Automatic deadlock prevention with timeouts.

### 📊 Real-time Dashboard
Web-based visualization of:
- DAG with live status
- Per-agent progress bars
- Message bus activity
- Logs streaming

### ⚡ OpenClaw Integration
Built on OpenClaw's agent infrastructure:
- Session spawning
- Message passing
- Resource management

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

# Monitoring
dare status                    # Current run status
dare status --watch            # Live TUI
dare logs                      # View logs
dare logs --follow             # Stream logs

# Control
dare pause                     # Pause after current wave
dare resume                    # Resume paused run
dare kill                      # Abort everything

# Dashboard
dare dashboard                 # Open web UI
```

## Configuration

```toml
# dare.toml
[execution]
max_parallel_agents = 4
wave_timeout_seconds = 300
task_timeout_seconds = 120

[gateway]
port = 18789

[dashboard]
port = 8765
```

## Architecture

```
┌──────────────────────────────────────────────────┐
│                   dare CLI                        │
└──────────────────────────────────────────────────┘
                        │
                        ▼
┌──────────────────────────────────────────────────┐
│              Orchestrator Core                    │
│  ┌────────────┐ ┌────────────┐ ┌──────────────┐  │
│  │ DAG Planner│ │Wave Executor│ │ Message Bus │  │
│  └────────────┘ └────────────┘ └──────────────┘  │
└──────────────────────────────────────────────────┘
                        │
         ┌──────────────┼──────────────┐
         ▼              ▼              ▼
    ┌─────────┐   ┌─────────┐   ┌─────────┐
    │ Agent 1 │   │ Agent 2 │   │ Agent N │
    └─────────┘   └─────────┘   └─────────┘
```

## Roadmap

- [x] Design document
- [ ] Core orchestration engine
- [ ] DAG planner (manual + auto)
- [ ] Wave executor
- [ ] Message bus
- [ ] File locking
- [ ] CLI interface
- [ ] Web dashboard
- [ ] OpenClaw gateway integration
- [ ] Auto-planning with LLM
- [ ] Smart retry on failures
- [ ] Git integration
- [ ] Distributed execution

## Tech Stack

- **Language:** Rust
- **Web:** Axum + HTMX + Tailwind
- **Database:** SQLite
- **Real-time:** SSE (Server-Sent Events)
- **Agent Runtime:** OpenClaw

## Contributing

Contributions welcome! Please read the [design document](docs/design.md) first to understand the architecture.

## License

MIT License. See [LICENSE](LICENSE).

## Author

**Ngonidzashe Mangudya** ([@iamngoni](https://github.com/iamngoni))

---

*dare.run — Let agents work together, not alone.*
