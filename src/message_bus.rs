//! Message bus for inter-agent communication

use lazy_static::lazy_static;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};

use crate::models::TaskId;

/// A message on the bus
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BusMessage {
    // Status updates
    Progress {
        task_id: TaskId,
        percent: u8,
        detail: String,
    },
    Completed {
        task_id: TaskId,
    },
    Failed {
        task_id: TaskId,
        error: String,
    },

    // Coordination
    HelpRequest {
        from: TaskId,
        question: String,
    },
    HelpResponse {
        to: TaskId,
        answer: String,
    },

    // File operations
    FileLockRequest {
        task_id: TaskId,
        path: PathBuf,
    },
    FileLockGranted {
        task_id: TaskId,
        path: PathBuf,
    },
    FileLockDenied {
        task_id: TaskId,
        path: PathBuf,
        held_by: TaskId,
    },
    FileReleased {
        task_id: TaskId,
        path: PathBuf,
    },

    // System
    Broadcast {
        message: String,
    },
    AgentLog {
        task_id: TaskId,
        level: String,
        message: String,
    },
}

impl BusMessage {
    /// Get the target task ID if this message is directed
    pub fn target(&self) -> Option<&TaskId> {
        match self {
            BusMessage::HelpResponse { to, .. } => Some(to),
            BusMessage::FileLockGranted { task_id, .. } => Some(task_id),
            BusMessage::FileLockDenied { task_id, .. } => Some(task_id),
            _ => None,
        }
    }

    /// Get the source task ID
    pub fn source(&self) -> Option<&TaskId> {
        match self {
            BusMessage::Progress { task_id, .. } => Some(task_id),
            BusMessage::Completed { task_id } => Some(task_id),
            BusMessage::Failed { task_id, .. } => Some(task_id),
            BusMessage::HelpRequest { from, .. } => Some(from),
            BusMessage::FileLockRequest { task_id, .. } => Some(task_id),
            BusMessage::FileReleased { task_id, .. } => Some(task_id),
            BusMessage::AgentLog { task_id, .. } => Some(task_id),
            _ => None,
        }
    }
}

/// The message bus for coordinating agents
pub struct MessageBus {
    /// Sender for new messages
    tx: mpsc::Sender<BusMessage>,

    /// Broadcast channel for subscribers
    broadcast_tx: broadcast::Sender<BusMessage>,

    /// Direct channels to specific tasks
    subscribers: Arc<RwLock<HashMap<TaskId, mpsc::Sender<BusMessage>>>>,
}

impl MessageBus {
    /// Create a new message bus
    pub fn new(tx: mpsc::Sender<BusMessage>) -> Self {
        let (broadcast_tx, _) = broadcast::channel(100);

        Self {
            tx,
            broadcast_tx,
            subscribers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Publish a message to the bus
    pub async fn publish(&self, msg: BusMessage) {
        // Send to main channel
        let _ = self.tx.send(msg.clone()).await;

        // Broadcast to all subscribers
        let _ = self.broadcast_tx.send(msg.clone());

        // Direct delivery if addressed
        if let Some(target) = msg.target() {
            let subscribers = self.subscribers.read().await;
            if let Some(tx) = subscribers.get(target) {
                let _ = tx.send(msg).await;
            }
        }
    }

    /// Subscribe to messages for a specific task
    pub async fn subscribe(&self, task_id: TaskId) -> mpsc::Receiver<BusMessage> {
        let (tx, rx) = mpsc::channel(100);
        self.subscribers.write().await.insert(task_id, tx);
        rx
    }

    /// Subscribe to all broadcast messages
    pub fn subscribe_broadcast(&self) -> broadcast::Receiver<BusMessage> {
        self.broadcast_tx.subscribe()
    }

    /// Unsubscribe a task
    pub async fn unsubscribe(&self, task_id: &TaskId) {
        self.subscribers.write().await.remove(task_id);
    }
}

/// Parser for DARE protocol markers in agent output
pub struct MarkerParser;

lazy_static! {
    static ref PROGRESS_RE: Regex =
        Regex::new(r"\[DARE:PROGRESS:(\d+)\]\s*(.*)").unwrap();
    static ref COMPLETE_RE: Regex =
        Regex::new(r"\[DARE:COMPLETE\]").unwrap();
    static ref FAILED_RE: Regex =
        Regex::new(r"\[DARE:FAILED\]\s*(.*)").unwrap();
    static ref HELP_RE: Regex =
        Regex::new(r"\[DARE:HELP\]\s*(.*)").unwrap();
    static ref LOG_RE: Regex =
        Regex::new(r"\[DARE:LOG:(\w+)\]\s*(.*)").unwrap();
    static ref LOCK_RE: Regex =
        Regex::new(r"\[DARE:LOCK:([^\]]+)\]").unwrap();
    static ref UNLOCK_RE: Regex =
        Regex::new(r"\[DARE:UNLOCK:([^\]]+)\]").unwrap();
}

impl MarkerParser {
    /// Parse agent output for DARE protocol markers
    pub fn parse(task_id: &TaskId, output: &str) -> Vec<BusMessage> {
        let mut messages = Vec::new();

        for line in output.lines() {
            if let Some(caps) = PROGRESS_RE.captures(line) {
                if let Ok(percent) = caps[1].parse() {
                    messages.push(BusMessage::Progress {
                        task_id: task_id.clone(),
                        percent,
                        detail: caps[2].to_string(),
                    });
                }
            } else if COMPLETE_RE.is_match(line) {
                messages.push(BusMessage::Completed {
                    task_id: task_id.clone(),
                });
            } else if let Some(caps) = FAILED_RE.captures(line) {
                messages.push(BusMessage::Failed {
                    task_id: task_id.clone(),
                    error: caps[1].to_string(),
                });
            } else if let Some(caps) = HELP_RE.captures(line) {
                messages.push(BusMessage::HelpRequest {
                    from: task_id.clone(),
                    question: caps[1].to_string(),
                });
            } else if let Some(caps) = LOG_RE.captures(line) {
                messages.push(BusMessage::AgentLog {
                    task_id: task_id.clone(),
                    level: caps[1].to_string(),
                    message: caps[2].to_string(),
                });
            } else if let Some(caps) = LOCK_RE.captures(line) {
                messages.push(BusMessage::FileLockRequest {
                    task_id: task_id.clone(),
                    path: PathBuf::from(&caps[1]),
                });
            } else if let Some(caps) = UNLOCK_RE.captures(line) {
                messages.push(BusMessage::FileReleased {
                    task_id: task_id.clone(),
                    path: PathBuf::from(&caps[1]),
                });
            }
        }

        messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_progress() {
        let output = "[DARE:PROGRESS:50] Halfway done";
        let messages = MarkerParser::parse(&"test".to_string(), output);
        assert_eq!(messages.len(), 1);
        match &messages[0] {
            BusMessage::Progress { percent, detail, .. } => {
                assert_eq!(*percent, 50);
                assert_eq!(detail, "Halfway done");
            }
            _ => panic!("Expected Progress message"),
        }
    }

    #[test]
    fn test_parse_complete() {
        let output = "[DARE:COMPLETE]";
        let messages = MarkerParser::parse(&"test".to_string(), output);
        assert_eq!(messages.len(), 1);
        assert!(matches!(messages[0], BusMessage::Completed { .. }));
    }

    #[test]
    fn test_parse_failed() {
        let output = "[DARE:FAILED] Something went wrong";
        let messages = MarkerParser::parse(&"test".to_string(), output);
        assert_eq!(messages.len(), 1);
        match &messages[0] {
            BusMessage::Failed { error, .. } => {
                assert_eq!(error, "Something went wrong");
            }
            _ => panic!("Expected Failed message"),
        }
    }
}
