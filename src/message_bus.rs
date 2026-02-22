//! Result bus for inter-task communication
//!
//! This module handles passing results between tasks when one depends on another.
//! Note: This is purely internal orchestration - agents don't need to know about this.
//! The executor observes session completion via the gateway.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use crate::models::TaskId;

/// A message about task lifecycle (internal to orchestrator)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TaskMessage {
    /// Task was spawned
    Spawned {
        task_id: TaskId,
        session_key: String,
    },
    /// Task is now running (session is active)
    Running {
        task_id: TaskId,
    },
    /// Task completed successfully
    Completed {
        task_id: TaskId,
        summary: String,
    },
    /// Task failed
    Failed {
        task_id: TaskId,
        error: String,
    },
}

impl TaskMessage {
    /// Get the task ID from the message
    pub fn task_id(&self) -> &TaskId {
        match self {
            TaskMessage::Spawned { task_id, .. } => task_id,
            TaskMessage::Running { task_id } => task_id,
            TaskMessage::Completed { task_id, .. } => task_id,
            TaskMessage::Failed { task_id, .. } => task_id,
        }
    }
}

/// Result from a completed task (for dependent tasks)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub task_id: TaskId,
    pub files_modified: Vec<String>,
    pub summary: String,
}

/// Bus for task-to-task result passing
/// 
/// This is used by the executor to:
/// 1. Store results when tasks complete
/// 2. Provide context to dependent tasks
pub struct TaskBus {
    /// Results from completed tasks
    results: Arc<RwLock<HashMap<TaskId, TaskResult>>>,
    
    /// Broadcast channel for task lifecycle events
    broadcast_tx: broadcast::Sender<TaskMessage>,
}

impl TaskBus {
    /// Create a new task bus
    pub fn new() -> Self {
        let (broadcast_tx, _) = broadcast::channel(100);
        
        Self {
            results: Arc::new(RwLock::new(HashMap::new())),
            broadcast_tx,
        }
    }

    /// Store a result for a completed task
    pub async fn store_result(&self, result: TaskResult) {
        let task_id = result.task_id.clone();
        let summary = result.summary.clone();
        
        self.results.write().await.insert(task_id.clone(), result);
        
        // Broadcast completion
        let _ = self.broadcast_tx.send(TaskMessage::Completed {
            task_id,
            summary,
        });
    }

    /// Get result for a specific task
    pub async fn get_result(&self, task_id: &TaskId) -> Option<TaskResult> {
        self.results.read().await.get(task_id).cloned()
    }

    /// Get all results for a set of task IDs (for dependency context)
    pub async fn get_dependency_results(&self, task_ids: &[TaskId]) -> Vec<TaskResult> {
        let results = self.results.read().await;
        task_ids
            .iter()
            .filter_map(|id| results.get(id).cloned())
            .collect()
    }

    /// Build context string from dependency results
    pub async fn build_context(&self, depends_on: &[TaskId]) -> String {
        let results = self.get_dependency_results(depends_on).await;
        
        if results.is_empty() {
            return String::new();
        }

        let mut context = String::from("## Context from Completed Dependencies\n\n");
        
        for result in results {
            context.push_str(&format!(
                "### Task: {}\n{}\n",
                result.task_id, result.summary
            ));
            
            if !result.files_modified.is_empty() {
                context.push_str("Files modified:\n");
                for file in &result.files_modified {
                    context.push_str(&format!("- {}\n", file));
                }
            }
            context.push('\n');
        }

        context
    }

    /// Subscribe to task lifecycle events
    pub fn subscribe(&self) -> broadcast::Receiver<TaskMessage> {
        self.broadcast_tx.subscribe()
    }

    /// Publish a task lifecycle event
    pub fn publish(&self, msg: TaskMessage) {
        let _ = self.broadcast_tx.send(msg);
    }

    /// Get all stored results
    pub async fn all_results(&self) -> HashMap<TaskId, TaskResult> {
        self.results.read().await.clone()
    }

    /// Clear all results (for new runs)
    pub async fn clear(&self) {
        self.results.write().await.clear();
    }
}

impl Default for TaskBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_store_and_get_result() {
        let bus = TaskBus::new();
        
        let result = TaskResult {
            task_id: "task1".to_string(),
            files_modified: vec!["src/main.rs".to_string()],
            summary: "Implemented main function".to_string(),
        };
        
        bus.store_result(result.clone()).await;
        
        let retrieved = bus.get_result(&"task1".to_string()).await;
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().summary, "Implemented main function");
    }

    #[tokio::test]
    async fn test_build_context() {
        let bus = TaskBus::new();
        
        bus.store_result(TaskResult {
            task_id: "migration".to_string(),
            files_modified: vec!["migrations/001.sql".to_string()],
            summary: "Created users table".to_string(),
        }).await;
        
        bus.store_result(TaskResult {
            task_id: "model".to_string(),
            files_modified: vec!["src/models/user.rs".to_string()],
            summary: "Implemented User model".to_string(),
        }).await;
        
        let context = bus.build_context(&["migration".to_string(), "model".to_string()]).await;
        
        assert!(context.contains("migration"));
        assert!(context.contains("model"));
        assert!(context.contains("Created users table"));
        assert!(context.contains("User model"));
    }

    #[tokio::test]
    async fn test_subscription() {
        let bus = TaskBus::new();
        let mut rx = bus.subscribe();
        
        bus.publish(TaskMessage::Spawned {
            task_id: "task1".to_string(),
            session_key: "session-123".to_string(),
        });
        
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.task_id(), "task1");
    }
}
