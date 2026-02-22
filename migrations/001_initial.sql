-- dare.run initial schema
-- Version: 001

-- Runs: top-level execution units
CREATE TABLE IF NOT EXISTS runs (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    status TEXT NOT NULL DEFAULT 'pending',  -- pending, running, paused, completed, failed
    config_json TEXT,  -- serialized RunConfig
    started_at DATETIME,
    completed_at DATETIME,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Index for listing runs
CREATE INDEX IF NOT EXISTS idx_runs_created ON runs(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_runs_status ON runs(status);

-- Tasks: individual work items within a run
CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    description TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',  -- pending, spawned, executing, completed, failed
    wave INTEGER NOT NULL,  -- which wave this task belongs to
    agent_session_id TEXT,  -- OpenClaw session ID when spawned
    outputs_json TEXT,  -- expected output paths (JSON array)
    result_json TEXT,  -- actual results
    started_at DATETIME,
    completed_at DATETIME,
    error TEXT,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Indexes for task queries
CREATE INDEX IF NOT EXISTS idx_tasks_run ON tasks(run_id);
CREATE INDEX IF NOT EXISTS idx_tasks_run_status ON tasks(run_id, status);
CREATE INDEX IF NOT EXISTS idx_tasks_wave ON tasks(run_id, wave);

-- Task dependencies: defines the DAG edges
CREATE TABLE IF NOT EXISTS task_dependencies (
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    depends_on_task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    PRIMARY KEY (task_id, depends_on_task_id)
);

-- Index for dependency lookups
CREATE INDEX IF NOT EXISTS idx_deps_depends_on ON task_dependencies(depends_on_task_id);

-- Message bus: persisted messages between agents
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,  -- NULL for broadcast
    message_type TEXT NOT NULL,  -- progress, completed, failed, help_request, etc.
    payload_json TEXT NOT NULL,  -- message-specific data
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Indexes for message queries
CREATE INDEX IF NOT EXISTS idx_messages_run ON messages(run_id);
CREATE INDEX IF NOT EXISTS idx_messages_task ON messages(task_id);
CREATE INDEX IF NOT EXISTS idx_messages_created ON messages(created_at);
CREATE INDEX IF NOT EXISTS idx_messages_type ON messages(run_id, message_type);

-- File locks: tracks which files are locked by which tasks
CREATE TABLE IF NOT EXISTS file_locks (
    path TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    lock_type TEXT NOT NULL,  -- exclusive, shared
    acquired_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at DATETIME NOT NULL
);

-- Index for lock queries
CREATE INDEX IF NOT EXISTS idx_locks_run ON file_locks(run_id);
CREATE INDEX IF NOT EXISTS idx_locks_task ON file_locks(task_id);
CREATE INDEX IF NOT EXISTS idx_locks_expires ON file_locks(expires_at);

-- Agent logs: detailed logs from each agent
CREATE TABLE IF NOT EXISTS agent_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    level TEXT NOT NULL,  -- debug, info, warn, error
    message TEXT NOT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Indexes for log queries
CREATE INDEX IF NOT EXISTS idx_agent_logs_run ON agent_logs(run_id);
CREATE INDEX IF NOT EXISTS idx_agent_logs_task ON agent_logs(task_id);
CREATE INDEX IF NOT EXISTS idx_agent_logs_created ON agent_logs(created_at);
CREATE INDEX IF NOT EXISTS idx_agent_logs_level ON agent_logs(run_id, level);
