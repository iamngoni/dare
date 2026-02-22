//! DAG (Directed Acyclic Graph) data structures and algorithms

use anyhow::{bail, Result};
use petgraph::algo::toposort;
use petgraph::graph::{DiGraph, NodeIndex};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::models::{Complexity, TaskId};

/// A node in the task graph
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNode {
    pub id: TaskId,
    pub description: String,
    #[serde(default)]
    pub depends_on: Vec<TaskId>,
    #[serde(default)]
    pub outputs: Vec<PathBuf>,
    #[serde(default)]
    pub estimated_complexity: Option<Complexity>,
}

/// The complete task graph
#[derive(Debug, Clone, Serialize)]
pub struct TaskGraph {
    pub name: Option<String>,
    pub description: Option<String>,
    #[serde(rename = "tasks")]
    pub nodes: HashMap<TaskId, TaskNode>,
}

impl TaskGraph {
    /// Create a new empty task graph
    pub fn new() -> Self {
        Self {
            name: None,
            description: None,
            nodes: HashMap::new(),
        }
    }

    /// Add a task node to the graph
    pub fn add_task(&mut self, node: TaskNode) {
        self.nodes.insert(node.id.clone(), node);
    }

    /// Validate the graph for cycles and missing dependencies
    pub fn validate(&self) -> Result<()> {
        // Check for missing dependencies
        for (task_id, node) in &self.nodes {
            for dep in &node.depends_on {
                if !self.nodes.contains_key(dep) {
                    bail!(
                        "Task '{}' depends on '{}' which does not exist",
                        task_id,
                        dep
                    );
                }
            }
        }

        // Check for cycles using petgraph
        let graph = self.to_petgraph();
        if toposort(&graph, None).is_err() {
            bail!("Task graph contains a cycle");
        }

        // Check for duplicate outputs
        let mut output_owners: HashMap<&PathBuf, &TaskId> = HashMap::new();
        for (task_id, node) in &self.nodes {
            for output in &node.outputs {
                if let Some(existing) = output_owners.get(output) {
                    bail!(
                        "Output '{}' is claimed by both '{}' and '{}'",
                        output.display(),
                        existing,
                        task_id
                    );
                }
                output_owners.insert(output, task_id);
            }
        }

        Ok(())
    }

    /// Convert to petgraph for algorithms
    fn to_petgraph(&self) -> DiGraph<&TaskId, ()> {
        let mut graph = DiGraph::new();
        let mut indices: HashMap<&TaskId, NodeIndex> = HashMap::new();

        // Add all nodes
        for task_id in self.nodes.keys() {
            let idx = graph.add_node(task_id);
            indices.insert(task_id, idx);
        }

        // Add edges (dependencies)
        for (task_id, node) in &self.nodes {
            let to_idx = indices[task_id];
            for dep_id in &node.depends_on {
                if let Some(&from_idx) = indices.get(dep_id) {
                    graph.add_edge(from_idx, to_idx, ());
                }
            }
        }

        graph
    }

    /// Compute execution waves (tasks grouped by when they can run)
    pub fn compute_waves(&self) -> Vec<Vec<TaskId>> {
        let mut waves: Vec<Vec<TaskId>> = Vec::new();
        let mut completed: HashSet<TaskId> = HashSet::new();
        let mut remaining: HashSet<TaskId> = self.nodes.keys().cloned().collect();

        while !remaining.is_empty() {
            // Find all tasks whose dependencies are complete
            let ready: Vec<TaskId> = remaining
                .iter()
                .filter(|task_id| {
                    let node = &self.nodes[*task_id];
                    node.depends_on.iter().all(|dep| completed.contains(dep))
                })
                .cloned()
                .collect();

            if ready.is_empty() {
                // This shouldn't happen if validate() passed
                tracing::error!("Deadlock detected in task graph");
                break;
            }

            // Move ready tasks from remaining to completed
            for task_id in &ready {
                remaining.remove(task_id);
                completed.insert(task_id.clone());
            }

            waves.push(ready);
        }

        waves
    }

    /// Get the number of execution waves
    pub fn wave_count(&self) -> usize {
        self.compute_waves().len()
    }

    /// Get tasks in a specific wave
    pub fn get_wave(&self, wave_num: usize) -> Vec<&TaskNode> {
        let waves = self.compute_waves();
        waves
            .get(wave_num)
            .map(|task_ids| {
                task_ids
                    .iter()
                    .filter_map(|id| self.nodes.get(id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get all tasks that depend on a given task
    pub fn dependents(&self, task_id: &TaskId) -> Vec<&TaskId> {
        self.nodes
            .iter()
            .filter(|(_, node)| node.depends_on.contains(task_id))
            .map(|(id, _)| id)
            .collect()
    }

    /// Get all dependencies of a task (transitive)
    pub fn all_dependencies(&self, task_id: &TaskId) -> HashSet<TaskId> {
        let mut deps = HashSet::new();
        let mut to_process: Vec<&TaskId> = vec![task_id];

        while let Some(current) = to_process.pop() {
            if let Some(node) = self.nodes.get(current) {
                for dep in &node.depends_on {
                    if deps.insert(dep.clone()) {
                        to_process.push(dep);
                    }
                }
            }
        }

        deps
    }
}

impl Default for TaskGraph {
    fn default() -> Self {
        Self::new()
    }
}

// Allow deserializing from a flat list of tasks
impl<'de> Deserialize<'de> for TaskGraph {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct TaskGraphHelper {
            name: Option<String>,
            description: Option<String>,
            tasks: Vec<TaskNode>,
        }

        let helper = TaskGraphHelper::deserialize(deserializer)?;

        let mut nodes = HashMap::new();
        for task in helper.tasks {
            nodes.insert(task.id.clone(), task);
        }

        Ok(TaskGraph {
            name: helper.name,
            description: helper.description,
            nodes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_missing_dependency() {
        let mut graph = TaskGraph::new();
        graph.add_task(TaskNode {
            id: "a".to_string(),
            description: "Task A".to_string(),
            depends_on: vec!["nonexistent".to_string()],
            outputs: vec![],
            estimated_complexity: None,
        });

        assert!(graph.validate().is_err());
    }

    #[test]
    fn test_validate_cycle() {
        let mut graph = TaskGraph::new();
        graph.add_task(TaskNode {
            id: "a".to_string(),
            description: "Task A".to_string(),
            depends_on: vec!["b".to_string()],
            outputs: vec![],
            estimated_complexity: None,
        });
        graph.add_task(TaskNode {
            id: "b".to_string(),
            description: "Task B".to_string(),
            depends_on: vec!["a".to_string()],
            outputs: vec![],
            estimated_complexity: None,
        });

        assert!(graph.validate().is_err());
    }

    #[test]
    fn test_compute_waves() {
        let mut graph = TaskGraph::new();
        graph.add_task(TaskNode {
            id: "a".to_string(),
            description: "Task A".to_string(),
            depends_on: vec![],
            outputs: vec![],
            estimated_complexity: None,
        });
        graph.add_task(TaskNode {
            id: "b".to_string(),
            description: "Task B".to_string(),
            depends_on: vec!["a".to_string()],
            outputs: vec![],
            estimated_complexity: None,
        });
        graph.add_task(TaskNode {
            id: "c".to_string(),
            description: "Task C".to_string(),
            depends_on: vec![],
            outputs: vec![],
            estimated_complexity: None,
        });

        let waves = graph.compute_waves();
        assert_eq!(waves.len(), 2);
        assert!(waves[0].contains(&"a".to_string()));
        assert!(waves[0].contains(&"c".to_string()));
        assert!(waves[1].contains(&"b".to_string()));
    }
}
