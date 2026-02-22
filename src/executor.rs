//! Wave executor - runs tasks in parallel waves

use anyhow::Result;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::config::Config;
use crate::dag::{TaskGraph, TaskNode};
use crate::db::Database;
use crate::gateway::GatewayClient;
use crate::message_bus::{BusMessage, MessageBus};
use crate::models::{Run, RunStatus, TaskId, TaskStatus};

/// Executes task graphs in parallel waves
pub struct WaveExecutor<'a> {
    config: &'a Config,
    db: &'a Database,
}

impl<'a> WaveExecutor<'a> {
    pub fn new(config: &'a Config, db: &'a Database) -> Self {
        Self { config, db }
    }

    /// Execute the entire task graph
    pub async fn execute(&self, run: &Run, graph: &TaskGraph) -> Result<()> {
        tracing::info!("Starting execution of run {}", run.id);

        // Connect to gateway
        let gateway = GatewayClient::connect(self.config.gateway.port).await?;

        // Initialize message bus
        let (bus_tx, mut bus_rx) = mpsc::channel::<BusMessage>(100);
        let _bus = MessageBus::new(bus_tx.clone());

        // Compute waves
        let waves = graph.compute_waves();
        tracing::info!("Execution plan: {} waves", waves.len());

        // Execute each wave
        for (wave_num, task_ids) in waves.iter().enumerate() {
            // Check if paused
            if let Some(current_run) = self.db.get_run(&run.id).await? {
                if current_run.status == RunStatus::Paused {
                    tracing::info!("Run paused after wave {}", wave_num);
                    return Ok(());
                }
            }

            tracing::info!(
                "Starting wave {} with {} tasks: {:?}",
                wave_num,
                task_ids.len(),
                task_ids
            );

            println!(
                "Wave {} ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━",
                wave_num
            );

            // Get task nodes
            let tasks: Vec<&TaskNode> = task_ids
                .iter()
                .filter_map(|id| graph.nodes.get(id))
                .collect();

            // Limit parallelism
            let max_parallel = self.config.execution.max_parallel_agents.min(tasks.len());

            // Execute wave
            let wave_result = self
                .execute_wave(
                    run,
                    &tasks,
                    max_parallel,
                    &gateway,
                    &bus_tx,
                    &mut bus_rx,
                )
                .await;

            if let Err(e) = wave_result {
                tracing::error!("Wave {} failed: {}", wave_num, e);
                return Err(e);
            }

            println!();
        }

        Ok(())
    }

    /// Execute a single wave of tasks
    async fn execute_wave(
        &self,
        run: &Run,
        tasks: &[&TaskNode],
        max_parallel: usize,
        gateway: &GatewayClient,
        bus_tx: &mpsc::Sender<BusMessage>,
        bus_rx: &mut mpsc::Receiver<BusMessage>,
    ) -> Result<()> {
        let wave_timeout = Duration::from_secs(self.config.execution.wave_timeout_seconds);

        // Track active tasks
        let mut active: HashMap<TaskId, AgentHandle> = HashMap::new();
        let mut pending: Vec<&TaskNode> = tasks.to_vec();
        let mut completed: Vec<TaskId> = Vec::new();
        let mut failed: Vec<(TaskId, String)> = Vec::new();

        // Main execution loop
        let result = timeout(wave_timeout, async {
            loop {
                // Spawn tasks up to max_parallel
                while active.len() < max_parallel && !pending.is_empty() {
                    let task = pending.remove(0);
                    let handle = self
                        .spawn_agent(run, task, gateway, bus_tx.clone())
                        .await?;
                    active.insert(task.id.clone(), handle);
                    println!("  ⠋ {} (spawned)", task.id);
                }

                // Wait for messages
                if let Some(msg) = bus_rx.recv().await {
                    match msg {
                        BusMessage::Completed { task_id } => {
                            active.remove(&task_id);
                            completed.push(task_id.clone());
                            println!("  ✓ {} (completed)", task_id);
                        }
                        BusMessage::Failed { task_id, error } => {
                            active.remove(&task_id);
                            failed.push((task_id.clone(), error.clone()));
                            println!("  ✗ {} (failed: {})", task_id, error);
                        }
                        BusMessage::Progress { task_id, percent, detail } => {
                            println!("  ⠋ {} [{}%] {}", task_id, percent, detail);
                        }
                        _ => {}
                    }
                }

                // Check if done
                if active.is_empty() && pending.is_empty() {
                    break;
                }
            }

            Ok::<_, anyhow::Error>(())
        })
        .await;

        match result {
            Ok(Ok(())) => {
                if !failed.is_empty() {
                    anyhow::bail!("Wave failed: {} tasks failed", failed.len());
                }
                Ok(())
            }
            Ok(Err(e)) => Err(e),
            Err(_) => anyhow::bail!("Wave timed out after {:?}", wave_timeout),
        }
    }

    /// Spawn a single agent for a task
    async fn spawn_agent(
        &self,
        _run: &Run,
        task: &TaskNode,
        gateway: &GatewayClient,
        bus_tx: mpsc::Sender<BusMessage>,
    ) -> Result<AgentHandle> {
        // Build agent context
        let context = self.build_agent_context(task);

        // Spawn via gateway
        let session_id = gateway.spawn_agent(&task.id, &context).await?;

        // Start monitoring task
        let task_id = task.id.clone();
        let task_timeout = Duration::from_secs(self.config.execution.task_timeout_seconds);

        let handle = tokio::spawn(async move {
            // TODO: Monitor agent output, parse DARE markers, send to bus
            // For now, simulate completion
            tokio::time::sleep(Duration::from_secs(2)).await;
            let _ = bus_tx
                .send(BusMessage::Completed {
                    task_id: task_id.clone(),
                })
                .await;
        });

        Ok(AgentHandle {
            session_id,
            join_handle: handle,
        })
    }

    /// Build the context/prompt for an agent
    fn build_agent_context(&self, task: &TaskNode) -> String {
        format!(
            r#"# dare.run Worker Agent

You are agent `{task_id}` in a coordinated multi-agent task.

## Your Task
{description}

## Expected Outputs
{outputs}

## Coordination Protocol

### Message Markers
- To report progress: `[DARE:PROGRESS:50] Halfway done`
- When complete: `[DARE:COMPLETE]`
- On failure: `[DARE:FAILED] Error reason`
- To log: `[DARE:LOG:INFO] Message`

### File Locking
Before modifying a file:
1. Output: `[DARE:LOCK:path/to/file]`
2. Wait for: `[DARE:LOCK_OK:path/to/file]`
3. After done: `[DARE:UNLOCK:path/to/file]`

## Rules
1. Only modify files in your outputs list
2. Report progress at least every 30 seconds
3. Always complete or fail explicitly
"#,
            task_id = task.id,
            description = task.description,
            outputs = task
                .outputs
                .iter()
                .map(|p| format!("- {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

/// Handle to a spawned agent
struct AgentHandle {
    #[allow(dead_code)]
    session_id: String,
    #[allow(dead_code)]
    join_handle: tokio::task::JoinHandle<()>,
}
