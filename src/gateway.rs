//! OpenClaw Gateway WebSocket client
//!
//! Connects to the OpenClaw Gateway, handles Ed25519 authentication,
//! spawns subagent sessions, and monitors session completion via WebSocket events.

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

/// Information about a spawned session
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_key: String,
    pub task_id: String,
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
    /// Session state changed (agent event)
    SessionStateChanged {
        session_key: String,
        state: String,
        error: Option<String>,
    },
    /// Session completed successfully
    SessionCompleted { session_key: String },
    /// Session failed
    SessionFailed {
        session_key: String,
        error: String,
    },
    /// Other event (for logging)
    Other { event_type: String, payload: Value },
}

/// Pending requests waiting for response
type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value>>>>>;

/// OpenClaw Gateway WebSocket client
pub struct GatewayClient {
    config: GatewayConfig,
    command_tx: Arc<Mutex<Option<mpsc::Sender<WsCommand>>>>,
    pending: PendingMap,
    event_tx: mpsc::Sender<GatewayEvent>,
    sessions: Arc<RwLock<HashMap<String, SessionInfo>>>,
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
            sessions: Arc::new(RwLock::new(HashMap::new())),
        });

        // Start connection in background
        let client_clone = Arc::clone(&client);
        tokio::spawn(async move {
            if let Err(e) = client_clone.connect_and_run().await {
                error!("Gateway connection error: {}", e);
            }
        });

        // Wait for connection to establish
        tokio::time::sleep(Duration::from_millis(500)).await;

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
        let client_id = "cli";
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

        debug!("Gateway event: {} {:?}", event_name, payload);

        // Process agent events for session monitoring
        if event_name == "agent" {
            let session_key = payload
                .get("sessionKey")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            let state = payload
                .get("state")
                .and_then(|s| s.as_str())
                .unwrap_or("unknown")
                .to_string();

            // Check if this is one of our sessions
            let is_ours = {
                let sessions = self.sessions.read().await;
                sessions.contains_key(&session_key)
            };

            if is_ours || session_key.contains("dare-") {
                // Update session state
                if state == "completed" || state == "done" {
                    let _ = self
                        .event_tx
                        .send(GatewayEvent::SessionCompleted {
                            session_key: session_key.clone(),
                        })
                        .await;

                    // Update local tracking
                    let mut sessions = self.sessions.write().await;
                    if let Some(info) = sessions.get_mut(&session_key) {
                        info.state = SessionState::Completed;
                    }
                } else if state == "failed" || state == "error" {
                    let error = payload
                        .get("error")
                        .and_then(|e| e.as_str())
                        .unwrap_or("Unknown error")
                        .to_string();
                    let _ = self
                        .event_tx
                        .send(GatewayEvent::SessionFailed {
                            session_key: session_key.clone(),
                            error: error.clone(),
                        })
                        .await;

                    let mut sessions = self.sessions.write().await;
                    if let Some(info) = sessions.get_mut(&session_key) {
                        info.state = SessionState::Failed(error);
                    }
                } else {
                    let _ = self
                        .event_tx
                        .send(GatewayEvent::SessionStateChanged {
                            session_key: session_key.clone(),
                            state: state.clone(),
                            error: None,
                        })
                        .await;

                    let mut sessions = self.sessions.write().await;
                    if let Some(info) = sessions.get_mut(&session_key) {
                        info.state = SessionState::Running;
                    }
                }
            }
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
    // Public API
    // ========================================================================

    /// Spawn a subagent session for a task
    pub async fn spawn_session(&self, task_id: &str, label: &str, message: &str) -> Result<String> {
        info!(task_id = %task_id, label = %label, "Spawning subagent session");

        let response = self
            .request(
                "sessions.spawn",
                json!({
                    "label": label,
                    "message": message,
                    "model": null  // Use default model
                }),
            )
            .await?;

        let session_key = response
            .get("sessionKey")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow!("No sessionKey in spawn response"))?
            .to_string();

        // Track this session
        {
            let mut sessions = self.sessions.write().await;
            sessions.insert(
                session_key.clone(),
                SessionInfo {
                    session_key: session_key.clone(),
                    task_id: task_id.to_string(),
                    state: SessionState::Spawning,
                },
            );
        }

        info!(session_key = %session_key, task_id = %task_id, "Session spawned");
        Ok(session_key)
    }

    /// Get the current state of a session
    pub async fn get_session_state(&self, session_key: &str) -> Option<SessionState> {
        let sessions = self.sessions.read().await;
        sessions.get(session_key).map(|s| s.state.clone())
    }

    /// Kill a session
    pub async fn kill_session(&self, session_key: &str) -> Result<()> {
        info!(session_key = %session_key, "Killing session");

        self.request("sessions.kill", json!({ "sessionKey": session_key }))
            .await?;

        // Remove from tracking
        let mut sessions = self.sessions.write().await;
        sessions.remove(session_key);

        Ok(())
    }

    /// List active sessions
    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let sessions = self.sessions.read().await;
        Ok(sessions.values().cloned().collect())
    }

    /// Check if connected to gateway
    pub async fn is_connected(&self) -> bool {
        self.command_tx.lock().await.is_some()
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
        // We don't assert Ok because CI won't have OpenClaw set up
        println!("Config load result: {:?}", result.is_ok());
    }
}
