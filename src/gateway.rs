//! OpenClaw Gateway WebSocket client
//!
//! Connects to the OpenClaw Gateway, handles Ed25519 authentication,
//! spawns agent sessions via cron API, and monitors session completion.

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{SecretKey, Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use tokio::time::timeout;
use tokio_tungstenite::{
    connect_async, tungstenite::protocol::Message, MaybeTlsStream, WebSocketStream,
};
use tracing::{debug, error, info, warn};

/// Gateway configuration loaded from OpenClaw files
#[derive(Clone, Debug)]
pub struct GatewayConfig {
    pub url: String,
    pub password: String,
    pub device_id: String,
    pub private_key: SecretKey,
    pub public_key_b64: String,
    pub auth_token: String,
}

impl GatewayConfig {
    /// Load configuration from OpenClaw identity files
    pub fn load() -> Result<Self> {
        let home = std::env::var("HOME").context("HOME not set")?;
        let openclaw_dir = PathBuf::from(&home).join(".openclaw");

        // Load gateway URL (default to local)
        let url = std::env::var("OPENCLAW_GATEWAY_URL")
            .unwrap_or_else(|_| "ws://127.0.0.1:18789".to_string());

        // Load gateway password from openclaw.json
        let password = {
            let config_path = openclaw_dir.join("openclaw.json");
            if let Ok(content) = std::fs::read_to_string(&config_path) {
                if let Ok(json) = serde_json::from_str::<Value>(&content) {
                    json.get("gateway")
                        .and_then(|g| g.get("auth"))
                        .and_then(|a| a.get("password"))
                        .and_then(|p| p.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_default()
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        };

        // Load device identity
        let device_path = openclaw_dir.join("identity/device.json");
        let device_content = std::fs::read_to_string(&device_path)
            .context("Failed to read device.json - is OpenClaw set up?")?;
        let device_json: DeviceJson =
            serde_json::from_str(&device_content).context("Failed to parse device.json")?;

        // Load auth token
        let auth_path = openclaw_dir.join("identity/device-auth.json");
        let auth_content = std::fs::read_to_string(&auth_path)
            .context("Failed to read device-auth.json")?;
        let auth_json: DeviceAuthJson =
            serde_json::from_str(&auth_content).context("Failed to parse device-auth.json")?;

        let auth_token = auth_json
            .tokens
            .get("operator")
            .map(|t| t.token.clone())
            .unwrap_or_default();

        // Parse Ed25519 private key from PEM
        let private_key = parse_ed25519_private_key(&device_json.private_key_pem)?;

        // Derive public key
        let signing_key = SigningKey::from_bytes(&private_key);
        let public_key = signing_key.verifying_key();
        let public_key_b64 = URL_SAFE_NO_PAD.encode(public_key.as_bytes());

        Ok(Self {
            url,
            password,
            device_id: device_json.device_id,
            private_key,
            public_key_b64,
            auth_token,
        })
    }
}

#[derive(Debug, Deserialize)]
struct DeviceJson {
    #[serde(rename = "deviceId")]
    device_id: String,
    #[serde(rename = "privateKeyPem")]
    private_key_pem: String,
}

#[derive(Debug, Deserialize)]
struct DeviceAuthJson {
    tokens: HashMap<String, TokenInfo>,
}

#[derive(Debug, Deserialize)]
struct TokenInfo {
    token: String,
}

/// Parse Ed25519 private key from PEM format
fn parse_ed25519_private_key(pem: &str) -> Result<SecretKey> {
    let b64 = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect::<String>();

    let der = base64::engine::general_purpose::STANDARD
        .decode(&b64)
        .context("Failed to decode private key base64")?;

    // PKCS#8 Ed25519 key: 32-byte key is at offset 16
    if der.len() < 48 {
        return Err(anyhow!("Private key DER too short"));
    }

    let key_bytes: [u8; 32] = der[16..48]
        .try_into()
        .context("Failed to extract 32-byte key")?;

    Ok(key_bytes)
}

/// Session state for tracking spawned agents
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    Spawning,
    Running,
    Completed,
    Failed(String),
}

/// Information about a spawned session via cron job
#[derive(Debug, Clone)]
pub struct SpawnedTask {
    pub task_id: String,
    pub cron_job_id: String,
    pub session_key: Option<String>,
    pub state: SessionState,
}

/// Commands to send to the WebSocket writer
enum WsCommand {
    Send(String),
    Shutdown,
}

/// Gateway event received via WebSocket
#[derive(Debug, Clone)]
pub enum GatewayEvent {
    /// Cron job triggered (agent session spawned)
    CronTriggered {
        job_id: String,
        session_key: String,
    },
    /// Session state changed
    SessionStateChanged {
        session_key: String,
        state: String,
    },
    /// Session completed
    SessionCompleted {
        session_key: String,
    },
    /// Session failed
    SessionFailed {
        session_key: String,
        error: String,
    },
    /// Connection established
    Connected,
}

/// Pending requests waiting for response
type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value>>>>>;

/// OpenClaw Gateway WebSocket client using cron-based spawning
pub struct GatewayClient {
    config: GatewayConfig,
    command_tx: Arc<Mutex<Option<mpsc::Sender<WsCommand>>>>,
    pending: PendingMap,
    event_tx: mpsc::Sender<GatewayEvent>,
    /// Maps cron job ID -> task info
    tasks: Arc<RwLock<HashMap<String, SpawnedTask>>>,
    /// Maps session key -> cron job ID (for event correlation)
    session_to_job: Arc<RwLock<HashMap<String, String>>>,
    connected: Arc<RwLock<bool>>,
}

impl GatewayClient {
    /// Create a new gateway client and connect
    pub async fn connect(
        config: GatewayConfig,
        event_tx: mpsc::Sender<GatewayEvent>,
    ) -> Result<Arc<Self>> {
        let client = Arc::new(Self {
            config,
            command_tx: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            event_tx,
            tasks: Arc::new(RwLock::new(HashMap::new())),
            session_to_job: Arc::new(RwLock::new(HashMap::new())),
            connected: Arc::new(RwLock::new(false)),
        });

        // Start connection in background
        let client_clone = Arc::clone(&client);
        tokio::spawn(async move {
            if let Err(e) = client_clone.connect_and_run().await {
                error!("Gateway connection error: {}", e);
            }
        });

        // Wait for connection with timeout
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if *client.connected.read().await {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                return Err(anyhow!("Timeout waiting for gateway connection"));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        Ok(client)
    }

    /// Connect and run the WebSocket message loop
    async fn connect_and_run(self: &Arc<Self>) -> Result<()> {
        info!(url = %self.config.url, "Connecting to OpenClaw Gateway...");

        let (ws_stream, _) = connect_async(&self.config.url)
            .await
            .context("Failed to connect to gateway")?;

        info!("WebSocket connected, authenticating...");

        let (write, read) = ws_stream.split();

        // Create command channel
        let (cmd_tx, cmd_rx) = mpsc::channel::<WsCommand>(100);
        *self.command_tx.lock().await = Some(cmd_tx.clone());

        // Spawn writer task
        let writer_handle = tokio::spawn(Self::writer_task(write, cmd_rx));

        // Run reader loop
        let result = self.reader_loop(read, cmd_tx.clone()).await;

        // Cleanup
        let _ = cmd_tx.send(WsCommand::Shutdown).await;
        let _ = writer_handle.await;
        *self.command_tx.lock().await = None;
        *self.connected.write().await = false;

        result
    }

    /// Writer task - sends messages from command channel
    async fn writer_task(
        mut write: futures_util::stream::SplitSink<
            WebSocketStream<MaybeTlsStream<TcpStream>>,
            Message,
        >,
        mut cmd_rx: mpsc::Receiver<WsCommand>,
    ) {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                WsCommand::Send(msg) => {
                    if let Err(e) = write.send(Message::Text(msg.into())).await {
                        error!("Failed to send WebSocket message: {}", e);
                        break;
                    }
                }
                WsCommand::Shutdown => {
                    debug!("Writer task shutdown");
                    break;
                }
            }
        }
    }

    /// Reader loop - handles challenge, auth, and messages
    async fn reader_loop(
        self: &Arc<Self>,
        mut read: futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
        cmd_tx: mpsc::Sender<WsCommand>,
    ) -> Result<()> {
        // Wait for challenge
        let challenge_msg = timeout(Duration::from_secs(10), read.next())
            .await
            .context("Timeout waiting for challenge")?
            .ok_or_else(|| anyhow!("Connection closed before challenge"))?
            .context("Failed to read challenge")?;

        let nonce = self.handle_challenge(challenge_msg)?;
        debug!("Received challenge nonce: {}", nonce);

        // Send connect request
        let connect_req = self.build_connect_request(&nonce)?;
        cmd_tx
            .send(WsCommand::Send(connect_req))
            .await
            .context("Failed to send connect request")?;

        // Wait for connect response
        let connect_res = timeout(Duration::from_secs(10), read.next())
            .await
            .context("Timeout waiting for connect response")?
            .ok_or_else(|| anyhow!("Connection closed before connect response"))?
            .context("Failed to read connect response")?;

        self.handle_connect_response(connect_res)?;
        info!("Gateway authenticated successfully!");
        
        *self.connected.write().await = true;
        let _ = self.event_tx.send(GatewayEvent::Connected).await;

        // Main message loop
        while let Some(msg_result) = read.next().await {
            match msg_result {
                Ok(Message::Text(text)) => {
                    if let Err(e) = self.handle_message(&text).await {
                        warn!("Error handling message: {}", e);
                    }
                }
                Ok(Message::Close(_)) => {
                    info!("Gateway sent close frame");
                    break;
                }
                Err(e) => {
                    error!("WebSocket read error: {}", e);
                    break;
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Handle the challenge event
    fn handle_challenge(&self, msg: Message) -> Result<String> {
        let text = match msg {
            Message::Text(t) => t.to_string(),
            _ => return Err(anyhow!("Expected text message for challenge")),
        };

        let json: Value = serde_json::from_str(&text).context("Failed to parse challenge")?;

        if json.get("type").and_then(|t| t.as_str()) != Some("event")
            || json.get("event").and_then(|e| e.as_str()) != Some("connect.challenge")
        {
            return Err(anyhow!("Expected connect.challenge event"));
        }

        json.get("payload")
            .and_then(|p| p.get("nonce"))
            .and_then(|n| n.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("Missing nonce in challenge"))
    }

    /// Build the connect request with device signature
    fn build_connect_request(&self, nonce: &str) -> Result<String> {
        let signed_at = chrono::Utc::now().timestamp_millis();
        let client_id = "cli";  // Must match gateway-allowed client IDs
        let client_mode = "cli";
        let role = "operator";
        let scopes = "operator.admin";

        // Build message to sign (v2 format)
        let sign_message = format!(
            "v2|{}|{}|{}|{}|{}|{}|{}|{}",
            self.config.device_id,
            client_id,
            client_mode,
            role,
            scopes,
            signed_at,
            self.config.auth_token,
            nonce
        );

        // Sign with Ed25519
        let signing_key = SigningKey::from_bytes(&self.config.private_key);
        let signature = signing_key.sign(sign_message.as_bytes());
        let signature_b64 = URL_SAFE_NO_PAD.encode(signature.to_bytes());

        let request = json!({
            "type": "req",
            "id": "connect-1",
            "method": "connect",
            "params": {
                "minProtocol": 3,
                "maxProtocol": 3,
                "client": {
                    "id": client_id,
                    "version": env!("CARGO_PKG_VERSION"),
                    "platform": std::env::consts::OS,
                    "mode": client_mode,
                    "displayName": "dare.run"
                },
                "auth": {
                    "token": self.config.auth_token,
                    "password": self.config.password
                },
                "role": role,
                "scopes": ["operator.admin"],
                "device": {
                    "id": self.config.device_id,
                    "publicKey": self.config.public_key_b64,
                    "signature": signature_b64,
                    "signedAt": signed_at,
                    "nonce": nonce
                },
                "caps": ["tool-events"]
            }
        });

        Ok(serde_json::to_string(&request)?)
    }

    /// Handle the connect response
    fn handle_connect_response(&self, msg: Message) -> Result<()> {
        let text = match msg {
            Message::Text(t) => t.to_string(),
            _ => return Err(anyhow!("Expected text message for connect response")),
        };

        let json: Value =
            serde_json::from_str(&text).context("Failed to parse connect response")?;

        if json.get("type").and_then(|t| t.as_str()) != Some("res") {
            return Err(anyhow!("Expected response type"));
        }

        let ok = json.get("ok").and_then(|o| o.as_bool()).unwrap_or(false);
        if !ok {
            let error = json
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("Unknown error");
            return Err(anyhow!("Connect failed: {}", error));
        }

        Ok(())
    }

    /// Handle an incoming message
    async fn handle_message(&self, text: &str) -> Result<()> {
        let json: Value = serde_json::from_str(text)?;
        let msg_type = json.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match msg_type {
            "event" => self.handle_event(&json).await,
            "res" => self.handle_response(&json).await,
            _ => Ok(()),
        }
    }

    /// Handle an event from the gateway
    async fn handle_event(&self, json: &Value) -> Result<()> {
        let event_name = json
            .get("event")
            .and_then(|e| e.as_str())
            .unwrap_or("unknown");
        let payload = json.get("payload").cloned().unwrap_or(Value::Null);

        // Skip tick events
        if event_name == "tick" {
            return Ok(());
        }

        // Log all events for debugging
        info!("Gateway event: {} payload_keys={:?}", event_name, 
            payload.as_object().map(|o| o.keys().collect::<Vec<_>>()).unwrap_or_default());

        match event_name {
            "cron" => {
                // Cron job events
                let action = payload.get("action").and_then(|a| a.as_str()).unwrap_or("");
                let job_id = payload.get("jobId").and_then(|j| j.as_str()).unwrap_or("");
                info!("Cron event: action={} jobId={}", action, job_id);
                
                // Extract session key if present
                let session_key = payload.get("sessionKey").and_then(|s| s.as_str()).unwrap_or("");
                
                match action {
                    "triggered" | "executed" | "fired" | "started" => {
                        if !session_key.is_empty() {
                            // Map session to job
                            {
                                let mut s2j = self.session_to_job.write().await;
                                s2j.insert(session_key.to_string(), job_id.to_string());
                            }
                            
                            // Update task state
                            {
                                let mut tasks = self.tasks.write().await;
                                if let Some(task) = tasks.get_mut(job_id) {
                                    task.session_key = Some(session_key.to_string());
                                    task.state = SessionState::Running;
                                }
                            }
                            
                            let _ = self.event_tx.send(GatewayEvent::CronTriggered {
                                job_id: job_id.to_string(),
                                session_key: session_key.to_string(),
                            }).await;
                        }
                    }
                    "finished" | "completed" => {
                        // Cron job finished - check status
                        let status = payload.get("status").and_then(|s| s.as_str()).unwrap_or("unknown");
                        info!("Cron job finished: jobId={} status={} sessionKey={}", job_id, status, session_key);
                        
                        // Update task state
                        {
                            let mut tasks = self.tasks.write().await;
                            if let Some(task) = tasks.get_mut(job_id) {
                                if status == "success" || status == "ok" || status.is_empty() {
                                    task.state = SessionState::Completed;
                                } else {
                                    task.state = SessionState::Failed(status.to_string());
                                }
                            }
                        }
                        
                        // Send completion event
                        if status == "success" || status == "ok" || status.is_empty() {
                            let _ = self.event_tx.send(GatewayEvent::SessionCompleted {
                                session_key: session_key.to_string(),
                            }).await;
                        } else {
                            let _ = self.event_tx.send(GatewayEvent::SessionFailed {
                                session_key: session_key.to_string(),
                                error: status.to_string(),
                            }).await;
                        }
                    }
                    _ => {}
                }
            }
            "agent" => {
                // Agent session state changes
                let session_key = payload.get("sessionKey").and_then(|s| s.as_str()).unwrap_or("");
                let state = payload.get("state").and_then(|s| s.as_str()).unwrap_or("unknown");
                
                // Check if this is one of our sessions
                let job_id = {
                    let s2j = self.session_to_job.read().await;
                    s2j.get(session_key).cloned()
                };
                
                // Also check if session key contains "dare-" prefix (our spawned sessions)
                let is_ours = job_id.is_some() || session_key.contains("dare-");
                
                if is_ours {
                    debug!("Agent event for our session: {} state={}", session_key, state);
                    
                    match state {
                        "completed" | "done" | "idle" => {
                            // Update task state
                            if let Some(ref jid) = job_id {
                                let mut tasks = self.tasks.write().await;
                                if let Some(task) = tasks.get_mut(jid) {
                                    task.state = SessionState::Completed;
                                }
                            }
                            
                            let _ = self.event_tx.send(GatewayEvent::SessionCompleted {
                                session_key: session_key.to_string(),
                            }).await;
                        }
                        "failed" | "error" => {
                            let error = payload.get("error")
                                .and_then(|e| e.as_str())
                                .unwrap_or("Unknown error")
                                .to_string();
                            
                            if let Some(ref jid) = job_id {
                                let mut tasks = self.tasks.write().await;
                                if let Some(task) = tasks.get_mut(jid) {
                                    task.state = SessionState::Failed(error.clone());
                                }
                            }
                            
                            let _ = self.event_tx.send(GatewayEvent::SessionFailed {
                                session_key: session_key.to_string(),
                                error,
                            }).await;
                        }
                        _ => {
                            let _ = self.event_tx.send(GatewayEvent::SessionStateChanged {
                                session_key: session_key.to_string(),
                                state: state.to_string(),
                            }).await;
                        }
                    }
                }
            }
            _ => {}
        }

        Ok(())
    }

    /// Handle a response to a request
    async fn handle_response(&self, json: &Value) -> Result<()> {
        let id = json
            .get("id")
            .and_then(|i| i.as_str())
            .unwrap_or("")
            .to_string();

        let mut pending = self.pending.lock().await;
        if let Some(tx) = pending.remove(&id) {
            let ok = json.get("ok").and_then(|o| o.as_bool()).unwrap_or(false);
            let result = if ok {
                Ok(json.get("payload").cloned().unwrap_or(Value::Null))
            } else {
                let error = json
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown error");
                Err(anyhow!("{}", error))
            };
            let _ = tx.send(result);
        }

        Ok(())
    }

    /// Send a request and wait for response
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = uuid::Uuid::new_v4().to_string();

        let request = json!({
            "type": "req",
            "id": &id,
            "method": method,
            "params": params
        });

        // Set up response channel
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id.clone(), tx);
        }

        // Send request
        let cmd_tx = self.command_tx.lock().await;
        let cmd_tx = cmd_tx
            .as_ref()
            .ok_or_else(|| anyhow!("Not connected to gateway"))?;
        cmd_tx
            .send(WsCommand::Send(serde_json::to_string(&request)?))
            .await
            .context("Failed to send request")?;

        // Wait for response with timeout
        let result = timeout(Duration::from_secs(30), rx)
            .await
            .context("Request timeout")?
            .context("Response channel closed")?;

        result
    }

    // ========================================================================
    // Public API - Cron-based task spawning
    // ========================================================================

    /// Spawn an agent session for a task
    /// 
    /// Strategy:
    /// 1. Try subagents.spawn first (preferred, direct approach)
    /// 2. Fall back to cron-based spawning if needed
    pub async fn spawn_task(&self, task_id: &str, label: &str, message: &str) -> Result<String> {
        info!(task_id = %task_id, label = %label, "Spawning agent task");

        // Try direct subagent spawn first
        let subagent_result = self.request("subagents.spawn", json!({
            "label": label,
            "message": message
        })).await;

        if let Ok(payload) = subagent_result {
            let session_key = payload
                .get("sessionKey")
                .or_else(|| payload.get("session"))
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("subagent:{}", task_id));

            // Track this session
            {
                let mut s2j = self.session_to_job.write().await;
                s2j.insert(session_key.clone(), task_id.to_string());
            }

            info!(session_key = %session_key, task_id = %task_id, "Subagent spawned directly");
            return Ok(session_key);
        }

        // Fall back to cron-based approach
        info!(task_id = %task_id, "Subagent spawn failed, falling back to cron");

        // Create unique job ID
        let job_id = format!("dare-{}-{}", task_id, uuid::Uuid::new_v4().simple());

        // Create the cron job with agentTurn payload
        // Using a one-shot schedule that triggers far in the future (we'll trigger manually)
        let add_result = self.request("cron.add", json!({
            "name": job_id,
            "schedule": {
                "kind": "cron",
                "expr": "0 0 1 1 *"  // Jan 1st at midnight - never naturally triggers
            },
            "enabled": true,
            "sessionTarget": "isolated",
            "payload": {
                "kind": "agentTurn",
                "message": message
            }
        })).await;

        match add_result {
            Ok(payload) => {
                debug!("Cron job created: {:?}", payload);
                
                // Extract the actual job ID from the response (may differ from name)
                let actual_job_id = payload.get("jobId")
                    .or_else(|| payload.get("id"))
                    .and_then(|id| id.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| job_id.clone());
                
                info!(job_id = %actual_job_id, name = %job_id, "Cron job created");
                
                // Track the task
                {
                    let mut tasks = self.tasks.write().await;
                    tasks.insert(actual_job_id.clone(), SpawnedTask {
                        task_id: task_id.to_string(),
                        cron_job_id: actual_job_id.clone(),
                        session_key: None,
                        state: SessionState::Spawning,
                    });
                }

                // Trigger the job immediately
                let run_result = self.request("cron.run", json!({
                    "jobId": actual_job_id
                })).await;

                match run_result {
                    Ok(run_payload) => {
                        debug!("Cron job triggered: {:?}", run_payload);
                        
                        // Extract session key if provided in response
                        if let Some(session_key) = run_payload.get("sessionKey").and_then(|s| s.as_str()) {
                            let mut tasks = self.tasks.write().await;
                            if let Some(task) = tasks.get_mut(&actual_job_id) {
                                task.session_key = Some(session_key.to_string());
                                task.state = SessionState::Running;
                            }
                            
                            let mut s2j = self.session_to_job.write().await;
                            s2j.insert(session_key.to_string(), actual_job_id.clone());
                        }
                        
                        Ok(actual_job_id)
                    }
                    Err(e) => {
                        // Clean up the cron job
                        let _ = self.remove_job(&actual_job_id).await;
                        Err(anyhow!("Failed to trigger cron job: {}", e))
                    }
                }
            }
            Err(e) => {
                Err(anyhow!("Failed to create cron job: {}", e))
            }
        }
    }

    /// Remove a cron job (cleanup after task completion)
    pub async fn remove_job(&self, job_id: &str) -> Result<()> {
        debug!(job_id = %job_id, "Removing cron job");
        
        self.request("cron.remove", json!({
            "jobId": job_id
        })).await?;
        
        // Clean up tracking
        {
            let mut tasks = self.tasks.write().await;
            tasks.remove(job_id);
        }
        
        Ok(())
    }

    /// Get the current state of a task
    pub async fn get_task_state(&self, job_id: &str) -> Option<SessionState> {
        let tasks = self.tasks.read().await;
        tasks.get(job_id).map(|t| t.state.clone())
    }

    /// Get task info by job ID
    pub async fn get_task(&self, job_id: &str) -> Option<SpawnedTask> {
        let tasks = self.tasks.read().await;
        tasks.get(job_id).cloned()
    }

    /// Find task by session key
    pub async fn find_task_by_session(&self, session_key: &str) -> Option<SpawnedTask> {
        let s2j = self.session_to_job.read().await;
        if let Some(job_id) = s2j.get(session_key) {
            let tasks = self.tasks.read().await;
            return tasks.get(job_id).cloned();
        }
        None
    }

    /// List active sessions
    pub async fn list_sessions(&self) -> Result<Value> {
        self.request("sessions.list", json!({})).await
    }

    /// Kill a session
    pub async fn kill_session(&self, session_key: &str) -> Result<()> {
        info!(session_key = %session_key, "Killing session");
        
        self.request("sessions.kill", json!({ "sessionKey": session_key }))
            .await?;
        
        // Clean up tracking if this was one of our sessions
        let job_id = {
            let s2j = self.session_to_job.read().await;
            s2j.get(session_key).cloned()
        };
        
        if let Some(job_id) = job_id {
            let mut tasks = self.tasks.write().await;
            tasks.remove(&job_id);
            
            let mut s2j = self.session_to_job.write().await;
            s2j.remove(session_key);
        }
        
        Ok(())
    }

    /// Spawn a session directly 
    /// Uses subagents.spawn to create an isolated agent session
    pub async fn spawn_session(&self, task_id: &str, label: &str, message: &str) -> Result<String> {
        info!(task_id = %task_id, label = %label, "Spawning subagent session");

        // Try subagents.spawn - this spawns an isolated subagent
        let result = self.request("subagents.spawn", json!({
            "label": label,
            "message": message
        })).await;

        match result {
            Ok(payload) => {
                // The response may have sessionKey or just confirm the spawn
                let session_key = payload
                    .get("sessionKey")
                    .or_else(|| payload.get("session"))
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("subagent:{}", task_id));

                // Track this session
                {
                    let mut s2j = self.session_to_job.write().await;
                    s2j.insert(session_key.clone(), task_id.to_string());
                }

                info!(session_key = %session_key, task_id = %task_id, "Subagent session spawned");
                Ok(session_key)
            }
            Err(e) => {
                // If subagents.spawn doesn't work, we're stuck
                Err(anyhow!("Failed to spawn session: {}", e))
            }
        }
    }

    /// Check if connected to gateway
    pub async fn is_connected(&self) -> bool {
        *self.connected.read().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_loading_fails_gracefully() {
        // This test just ensures the config loading doesn't panic
        // In CI without OpenClaw setup, this will return an error
        let result = GatewayConfig::load();
        println!("Config load result: {:?}", result.is_ok());
    }
}
