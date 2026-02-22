//! OpenClaw Gateway integration

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Client for OpenClaw Gateway
pub struct GatewayClient {
    port: u16,
    #[allow(dead_code)]
    ws_url: String,
}

impl GatewayClient {
    /// Connect to the gateway
    pub async fn connect(port: u16) -> Result<Self> {
        let ws_url = format!("ws://localhost:{}/ws", port);

        // Verify gateway is running
        let health_url = format!("http://localhost:{}/health", port);
        let client = reqwest::Client::new();

        match client.get(&health_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!("Connected to OpenClaw gateway on port {}", port);
            }
            _ => {
                tracing::warn!(
                    "Could not verify gateway health on port {}. Proceeding anyway.",
                    port
                );
            }
        }

        Ok(Self { port, ws_url })
    }

    /// Spawn a new agent session
    pub async fn spawn_agent(&self, label: &str, context: &str) -> Result<String> {
        // Use HTTP API to spawn session
        let url = format!("http://localhost:{}/api/sessions/spawn", self.port);
        let client = reqwest::Client::new();

        let request = SpawnRequest {
            label: format!("dare-{}", label),
            message: context.to_string(),
            model: None,
        };

        let response = client
            .post(&url)
            .json(&request)
            .send()
            .await
            .context("Failed to spawn agent")?;

        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to spawn agent: {}", error);
        }

        let result: SpawnResponse = response.json().await?;
        tracing::info!("Spawned agent session: {}", result.session_id);

        Ok(result.session_id)
    }

    /// Send a message to an agent session
    pub async fn send_message(&self, session_id: &str, message: &str) -> Result<()> {
        let url = format!(
            "http://localhost:{}/api/sessions/{}/send",
            self.port, session_id
        );
        let client = reqwest::Client::new();

        let request = SendRequest {
            message: message.to_string(),
        };

        let response = client.post(&url).json(&request).send().await?;

        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to send message: {}", error);
        }

        Ok(())
    }

    /// Kill an agent session
    pub async fn kill_session(&self, session_id: &str) -> Result<()> {
        let url = format!(
            "http://localhost:{}/api/sessions/{}/kill",
            self.port, session_id
        );
        let client = reqwest::Client::new();

        let response = client.post(&url).send().await?;

        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to kill session: {}", error);
        }

        tracing::info!("Killed session: {}", session_id);
        Ok(())
    }

    /// List active sessions
    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let url = format!("http://localhost:{}/api/sessions", self.port);
        let client = reqwest::Client::new();

        let response = client.get(&url).send().await?;

        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to list sessions: {}", error);
        }

        let sessions: Vec<SessionInfo> = response.json().await?;
        Ok(sessions)
    }

    /// Subscribe to session output via WebSocket
    pub async fn subscribe_output(
        &self,
        session_id: &str,
    ) -> Result<impl StreamExt<Item = Result<String, tokio_tungstenite::tungstenite::Error>>> {
        let url = format!(
            "ws://localhost:{}/api/sessions/{}/ws",
            self.port, session_id
        );

        let (ws_stream, _) = connect_async(&url)
            .await
            .context("Failed to connect to session WebSocket")?;

        let (_, read) = ws_stream.split();

        Ok(read.filter_map(|msg| async move {
            match msg {
                Ok(Message::Text(text)) => Some(Ok(text)),
                Ok(Message::Binary(data)) => {
                    String::from_utf8(data).ok().map(Ok)
                }
                Err(e) => Some(Err(e)),
                _ => None,
            }
        }))
    }
}

#[derive(Debug, Serialize)]
struct SpawnRequest {
    label: String,
    message: String,
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SpawnResponse {
    session_id: String,
}

#[derive(Debug, Serialize)]
struct SendRequest {
    message: String,
}

#[derive(Debug, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub label: Option<String>,
    pub status: String,
    pub created_at: String,
}
