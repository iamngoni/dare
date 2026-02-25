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

/// Unique identifier for a todo item
pub type TodoId = String;

/// A todo item for tracking work items in the API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Todo {
    pub id: TodoId,
    pub title: String,
    pub description: Option<String>,
    pub completed: bool,
    pub priority: TodoPriority,
    pub due_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Todo {
    pub fn new(title: &str) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            title: title.to_string(),
            description: None,
            completed: false,
            priority: TodoPriority::Medium,
            due_at: None,
            completed_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn with_description(mut self, description: &str) -> Self {
        self.description = Some(description.to_string());
        self
    }

    pub fn with_priority(mut self, priority: TodoPriority) -> Self {
        self.priority = priority;
        self
    }

    pub fn with_due_at(mut self, due_at: DateTime<Utc>) -> Self {
        self.due_at = Some(due_at);
        self
    }

    pub fn mark_completed(&mut self) {
        self.completed = true;
        self.completed_at = Some(Utc::now());
        self.updated_at = Utc::now();
    }

    pub fn mark_incomplete(&mut self) {
        self.completed = false;
        self.completed_at = None;
        self.updated_at = Utc::now();
    }
}

/// Priority level for todo items
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TodoPriority {
    Low,
    Medium,
    High,
    Urgent,
}

impl Default for TodoPriority {
    fn default() -> Self {
        Self::Medium
    }
}

impl std::fmt::Display for TodoPriority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TodoPriority::Low => write!(f, "low"),
            TodoPriority::Medium => write!(f, "medium"),
            TodoPriority::High => write!(f, "high"),
            TodoPriority::Urgent => write!(f, "urgent"),
        }
    }
}
