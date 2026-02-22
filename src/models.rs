//! Data models for dare

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// Unique identifier for a task
pub type TaskId = String;

/// Unique identifier for a run
pub type RunId = String;

/// A run represents a single execution of a task graph
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: RunId,
    pub name: String,
    pub description: Option<String>,
    pub status: RunStatus,
    pub config_json: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl Run {
    pub fn new(name: &str) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            description: None,
            status: RunStatus::Pending,
            config_json: None,
            started_at: None,
            completed_at: None,
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Pending,
    Running,
    Paused,
    Completed,
    Failed,
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunStatus::Pending => write!(f, "pending"),
            RunStatus::Running => write!(f, "running"),
            RunStatus::Paused => write!(f, "paused"),
            RunStatus::Completed => write!(f, "completed"),
            RunStatus::Failed => write!(f, "failed"),
        }
    }
}

/// A task represents a single unit of work within a run
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub run_id: RunId,
    pub description: String,
    pub status: TaskStatus,
    pub wave: usize,
    pub agent_session_id: Option<String>,
    pub outputs: Vec<PathBuf>,
    pub result_json: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl Task {
    pub fn new(id: &str, run_id: &str, description: &str, wave: usize) -> Self {
        Self {
            id: id.to_string(),
            run_id: run_id.to_string(),
            description: description.to_string(),
            status: TaskStatus::Pending,
            wave,
            agent_session_id: None,
            outputs: Vec::new(),
            result_json: None,
            started_at: None,
            completed_at: None,
            error: None,
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Spawned,
    Executing,
    Completed,
    Failed,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskStatus::Pending => write!(f, "pending"),
            TaskStatus::Spawned => write!(f, "spawned"),
            TaskStatus::Executing => write!(f, "executing"),
            TaskStatus::Completed => write!(f, "completed"),
            TaskStatus::Failed => write!(f, "failed"),
        }
    }
}

/// Task complexity estimation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Complexity {
    Small,
    Medium,
    Large,
    ExtraLarge,
}

/// Agent log entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLog {
    pub id: i64,
    pub run_id: RunId,
    pub task_id: TaskId,
    pub level: String,
    pub message: String,
    pub created_at: DateTime<Utc>,
}
