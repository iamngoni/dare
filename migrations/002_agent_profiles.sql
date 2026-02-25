-- Agent profiles table
CREATE TABLE IF NOT EXISTS agent_profiles (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    codename TEXT NOT NULL UNIQUE,
    role TEXT NOT NULL,
    expertise TEXT NOT NULL DEFAULT '',
    personality TEXT NOT NULL DEFAULT '',
    system_context TEXT NOT NULL DEFAULT '',
    model TEXT,
    avatar_emoji TEXT NOT NULL DEFAULT '🤖',
    color TEXT NOT NULL DEFAULT '#00FF88',
    is_active INTEGER NOT NULL DEFAULT 1,
    tasks_completed INTEGER NOT NULL DEFAULT 0,
    tasks_failed INTEGER NOT NULL DEFAULT 0,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_profiles_codename ON agent_profiles(codename);
CREATE INDEX IF NOT EXISTS idx_profiles_active ON agent_profiles(is_active);

-- Link tasks to agent profiles
ALTER TABLE tasks ADD COLUMN agent_profile_id TEXT REFERENCES agent_profiles(id);
