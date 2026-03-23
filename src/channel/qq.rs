//! QQ Bot channel adapter implementation
//!
//! Implements WebSocket-based connection to QQ Bot Open Platform.
//! Supports C2C (single chat) and Group (@bot) message scenarios.

use crate::channel::{ChannelAdapter, ChatId, RawIncoming};
use anyhow::{Context, Result, anyhow};
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime};
use tokio::sync::{RwLock, mpsc};
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tracing::{debug, error, info, warn};

/// Static registry: QQ bot_id → channel_name.
/// Populated at startup when each QqAdapter connects and fetches its own bot_id.
static QQ_BOT_REGISTRY: LazyLock<DashMap<String, String>> = LazyLock::new(DashMap::new);

/// Message sequence generator
static MSG_SEQ: AtomicU64 = AtomicU64::new(1);

/// QQ single message max length (recommendation)
const MAX_MESSAGE_LENGTH: usize = 4096;

fn generate_msg_seq() -> u64 {
    MSG_SEQ.fetch_add(1, Ordering::SeqCst)
}

/// QQ Bot adapter
#[derive(Debug)]
pub struct QqAdapter {
    http_client: HttpClient,
    token_manager: Arc<TokenManager>,
    /// Recent message IDs for passive reply (chat_id -> msg_id)
    recent_msg_ids: Arc<RwLock<HashMap<String, String>>>,
    channel_name: String,
}

impl QqAdapter {
    pub fn new(app_id: String, client_secret: String, channel_name: String) -> Self {
        let http_client = HttpClient::new();
        let token_manager = Arc::new(TokenManager::new(
            app_id,
            client_secret,
            http_client.clone(),
        ));
        Self {
            http_client,
            token_manager,
            recent_msg_ids: Arc::new(RwLock::new(HashMap::new())),
            channel_name,
        }
    }

    /// Get the bot registry for mention formatting
    pub fn get_registry() -> &'static DashMap<String, String> {
        &QQ_BOT_REGISTRY
    }

    /// Fetch and register bot ID in the registry
    async fn register_bot_id(&self) {
        // Fetch bot ID using users/@me endpoint
        #[derive(Deserialize)]
        struct Me {
            id: String,
        }

        if let Ok(token) = self.token_manager.get_token().await {
            let resp = self
                .http_client
                .get("https://api.sgroup.qq.com/users/@me")
                .header("Authorization", format!("QQBot {}", token))
                .send()
                .await;

            if let Ok(resp) = resp
                && resp.status().is_success()
                && let Ok(me) = resp.json::<Me>().await
            {
                let bot_id = me.id.clone();
                QQ_BOT_REGISTRY.insert(me.id, self.channel_name.clone());
                info!(channel = %self.channel_name, bot_id = %bot_id, "Registered QQ bot in registry");
            } else {
                warn!(channel = %self.channel_name, "Could not fetch QQ bot id; mentions will use plain text");
            }
        }
    }
}

#[async_trait::async_trait]
impl ChannelAdapter for QqAdapter {
    fn platform(&self) -> &'static str {
        "qq"
    }

    fn max_message_length(&self) -> usize {
        MAX_MESSAGE_LENGTH
    }

    async fn run_incoming(
        &self,
        incoming_tx: mpsc::Sender<RawIncoming>,
    ) -> anyhow::Result<()> {
        // Register bot ID in registry for native mentions
        self.register_bot_id().await;

        let session_state = Arc::new(SessionState::new());

        let mut reconnect_delay = Duration::from_secs(1);
        const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);

        loop {
            match run_websocket_connection(
                &self.token_manager,
                &self.recent_msg_ids,
                &session_state,
                incoming_tx.clone(),
            )
            .await
            {
                Ok(()) => {
                    info!(
                        "WebSocket connection closed gracefully, reconnecting in {:?}",
                        reconnect_delay
                    );
                    sleep(reconnect_delay).await;
                    reconnect_delay = std::cmp::min(reconnect_delay * 2, MAX_RECONNECT_DELAY);
                }
                Err(e) => {
                    error!(error = %e, "WebSocket connection error, reconnecting in {:?}", reconnect_delay);
                    sleep(reconnect_delay).await;
                    reconnect_delay = std::cmp::min(reconnect_delay * 2, MAX_RECONNECT_DELAY);
                }
            }
        }
    }

    async fn send_text(&self, chat_id: &ChatId, text: &str) -> Result<()> {
        send_message(
            &self.http_client,
            &self.token_manager,
            chat_id,
            text,
            &self.recent_msg_ids,
        )
        .await
    }

    fn format_mention(
        &self,
        target_bot: &str,
        target_channel_name: Option<&str>,
        message: &str,
    ) -> String {
        // Look up target's bot_id via the static registry
        let bot_id = target_channel_name.and_then(|cn| {
            QQ_BOT_REGISTRY
                .iter()
                .find(|e| e.value() == cn)
                .map(|e| e.key().clone())
        });
        match bot_id {
            Some(id) => format!("<@!{}> {}", id, message),
            None => {
                warn!(target_bot = %target_bot, "Target bot id not in QQ registry, falling back to plain mention");
                format!("@{} {}", target_bot, message)
            }
        }
    }
}

/// Access token manager with automatic refresh and thundering herd protection
#[derive(Debug)]
struct TokenManager {
    app_id: String,
    client_secret: String,
    http_client: HttpClient,
    token: RwLock<Option<TokenInfo>>,
    refresh_lock: tokio::sync::Mutex<()>,
}

#[derive(Clone, Debug)]
struct TokenInfo {
    access_token: String,
    expires_at: SystemTime,
}

impl TokenManager {
    fn new(app_id: String, client_secret: String, http_client: HttpClient) -> Self {
        Self {
            app_id,
            client_secret,
            http_client,
            token: RwLock::new(None),
            refresh_lock: tokio::sync::Mutex::new(()),
        }
    }

    async fn get_token(&self) -> Result<String> {
        // Fast path: Check if we have a valid cached token
        {
            let token_guard = self.token.read().await;
            if let Some(token) = token_guard.as_ref()
                && SystemTime::now() + Duration::from_secs(300) < token.expires_at
            {
                return Ok(token.access_token.clone());
            }
        }

        // Slow path: Need to fetch new token
        let _guard = self.refresh_lock.lock().await;

        // Double-check after acquiring lock
        {
            let token_guard = self.token.read().await;
            if let Some(token) = token_guard.as_ref()
                && SystemTime::now() + Duration::from_secs(300) < token.expires_at
            {
                return Ok(token.access_token.clone());
            }
        }

        // Fetch new token
        self.fetch_token().await
    }

    async fn fetch_token(&self) -> Result<String> {
        let url = "https://bots.qq.com/app/getAppAccessToken";
        let body = serde_json::json!({
            "appId": self.app_id,
            "clientSecret": self.client_secret,
        });

        let response = self
            .http_client
            .post(url)
            .json(&body)
            .send()
            .await
            .context("Failed to send token request")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("Token request failed: {} - {}", status, text));
        }

        let token_response: TokenResponse = response
            .json()
            .await
            .context("Failed to parse token response")?;

        let expires_in: u64 = token_response.expires_in.parse().unwrap_or(0);
        let expires_at = SystemTime::now() + Duration::from_secs(expires_in);
        let token_info = TokenInfo {
            access_token: token_response.access_token.clone(),
            expires_at,
        };

        let mut token_guard = self.token.write().await;
        *token_guard = Some(token_info);

        info!("Successfully obtained QQ Bot access token");
        Ok(token_response.access_token)
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: String,
}

/// WebSocket gateway payload
#[derive(Debug, Serialize, Deserialize)]
struct GatewayPayload {
    #[serde(rename = "op")]
    op_code: u8,
    #[serde(rename = "d", skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    #[serde(rename = "s", skip_serializing_if = "Option::is_none")]
    sequence: Option<u64>,
    #[serde(rename = "t", skip_serializing_if = "Option::is_none")]
    event_type: Option<String>,
}

/// WebSocket op codes
mod op {
    pub const DISPATCH: u8 = 0;
    pub const HEARTBEAT: u8 = 1;
    pub const IDENTIFY: u8 = 2;
    pub const RESUME: u8 = 6;
    pub const RECONNECT: u8 = 7;
    pub const INVALID_SESSION: u8 = 9;
    pub const HELLO: u8 = 10;
    pub const HEARTBEAT_ACK: u8 = 11;
}

/// Session state for resume support
#[derive(Clone, Default)]
struct SessionState {
    session_id: Arc<RwLock<Option<String>>>,
    last_sequence: Arc<RwLock<Option<u64>>>,
}

impl SessionState {
    fn new() -> Self {
        Self::default()
    }

    async fn get(&self) -> (Option<String>, Option<u64>) {
        let session_id = self.session_id.read().await.clone();
        let last_seq = *self.last_sequence.read().await;
        (session_id, last_seq)
    }

    async fn set(&self, session_id: Option<String>, last_seq: Option<u64>) {
        *self.session_id.write().await = session_id;
        if let Some(seq) = last_seq {
            *self.last_sequence.write().await = Some(seq);
        }
    }

    async fn update_sequence(&self, seq: u64) {
        *self.last_sequence.write().await = Some(seq);
    }

    async fn clear(&self) {
        *self.session_id.write().await = None;
        *self.last_sequence.write().await = None;
    }
}

/// Intents for events we want to receive
const INTENTS: u32 = 1 << 25; // GROUP_AND_C2C_EVENT

async fn run_websocket_connection(
    token_manager: &TokenManager,
    recent_msg_ids: &Arc<RwLock<HashMap<String, String>>>,
    session_state: &Arc<SessionState>,
    incoming_tx: mpsc::Sender<RawIncoming>,
) -> Result<()> {
    // Get access token
    let access_token = token_manager.get_token().await?;

    // Get gateway URL
    let gateway_url = get_gateway_url(&access_token).await?;
    info!(url = %gateway_url, "Connecting to QQ Bot gateway");

    // Connect WebSocket
    let (ws_stream, _) = connect_async(&gateway_url)
        .await
        .context("Failed to connect WebSocket")?;

    let (mut write, mut read) = ws_stream.split();

    // Wait for Hello
    let hello = read
        .next()
        .await
        .context("Expected Hello message")?
        .context("WebSocket error")?;

    let hello_payload: GatewayPayload = parse_ws_message(&hello)?;
    if hello_payload.op_code != op::HELLO {
        return Err(anyhow!("Expected Hello, got op={}", hello_payload.op_code));
    }

    let heartbeat_interval = hello_payload
        .data
        .as_ref()
        .and_then(|d| d.get("heartbeat_interval"))
        .and_then(|v| v.as_u64())
        .unwrap_or(45000);

    info!(interval = heartbeat_interval, "Received Hello");

    // Check if we have a session to resume
    let (existing_session_id, existing_last_seq) = session_state.get().await;

    if let Some(ref session_id) = existing_session_id {
        // Try to resume existing session
        info!(session_id = %session_id, "Attempting session resume");
        let resume = GatewayPayload {
            op_code: op::RESUME,
            data: Some(serde_json::json!({
                "token": format!("QQBot {}", access_token),
                "session_id": session_id,
                "seq": existing_last_seq.unwrap_or(0),
            })),
            sequence: None,
            event_type: None,
        };

        write
            .send(WsMessage::Text(serde_json::to_string(&resume)?.into()))
            .await
            .context("Failed to send Resume")?;
    } else {
        // New session - send Identify
        let identify = GatewayPayload {
            op_code: op::IDENTIFY,
            data: Some(serde_json::json!({
                "token": format!("QQBot {}", access_token),
                "intents": INTENTS,
                "shard": [0, 1],
                "properties": {
                    "$os": "linux",
                    "$browser": "acpbridge",
                    "$device": "acpbridge"
                }
            })),
            sequence: None,
            event_type: None,
        };

        write
            .send(WsMessage::Text(serde_json::to_string(&identify)?.into()))
            .await
            .context("Failed to send Identify")?;
    }

    // Wait for Ready or Resumed
    let ready = read
        .next()
        .await
        .context("Expected Ready/Resumed message")?
        .context("WebSocket error")?;

    let ready_payload: GatewayPayload = parse_ws_message(&ready)?;

    let session_id = match ready_payload.event_type.as_deref() {
        Some("READY") => {
            if ready_payload.op_code != op::DISPATCH {
                return Err(anyhow!(
                    "Expected Ready dispatch, got op={}",
                    ready_payload.op_code
                ));
            }
            let sid = ready_payload
                .data
                .as_ref()
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            info!(session_id = %sid, "QQ Bot ready (new session)");
            session_state.set(Some(sid.clone()), None).await;
            sid
        }
        Some("RESUMED") => {
            info!("QQ Bot session resumed");
            existing_session_id.unwrap_or_default()
        }
        _ => {
            if ready_payload.op_code == op::INVALID_SESSION {
                warn!("Session invalidated, will create new session on reconnect");
                session_state.clear().await;
            }
            return Err(anyhow!(
                "Expected Ready/Resumed, got op={:?} t={:?}",
                ready_payload.op_code,
                ready_payload.event_type
            ));
        }
    };

    // Start heartbeat loop
    let heartbeat_interval = Duration::from_millis(heartbeat_interval);
    let mut heartbeat_timer = tokio::time::interval(heartbeat_interval);
    let mut last_sequence: Option<u64> = existing_last_seq;

    loop {
        tokio::select! {
            _ = heartbeat_timer.tick() => {
                let heartbeat = GatewayPayload {
                    op_code: op::HEARTBEAT,
                    data: last_sequence.map(|s| serde_json::json!(s)),
                    sequence: None,
                    event_type: None,
                };

                if let Err(e) = write.send(WsMessage::Text(serde_json::to_string(&heartbeat)?.into())).await {
                    error!(error = %e, "Failed to send heartbeat");
                    break;
                }
                debug!("Sent heartbeat");
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(msg)) => {
                        if msg.is_close() {
                            info!("WebSocket closed by server");
                            break;
                        }

                        match parse_ws_message::<GatewayPayload>(&msg) {
                            Ok(payload) => {
                                if payload.op_code == op::HEARTBEAT_ACK {
                                    debug!("Received heartbeat ACK");
                                    continue;
                                }

                                if payload.op_code == op::DISPATCH {
                                    if let Some(seq) = payload.sequence {
                                        last_sequence = Some(seq);
                                        session_state.update_sequence(seq).await;
                                    }

                                    // Handle events
                                    if let Err(e) = handle_event(
                                        &payload,
                                        recent_msg_ids,
                                        &incoming_tx,
                                    ).await {
                                        error!(error = %e, "Failed to handle event");
                                    }
                                } else if payload.op_code == op::RECONNECT {
                                    info!("Server requested reconnect");
                                    break;
                                } else if payload.op_code == op::INVALID_SESSION {
                                    warn!("Session invalidated by server");
                                    session_state.clear().await;
                                    break;
                                }
                            }
                            Err(e) => {
                                error!(error = %e, "Failed to parse WebSocket message");
                            }
                        }
                    }
                    Some(Err(e)) => {
                        error!(error = %e, "WebSocket error");
                        break;
                    }
                    None => {
                        info!("WebSocket stream ended");
                        break;
                    }
                }
            }
        }
    }

    // Save session state before returning for potential resume
    session_state.set(Some(session_id), last_sequence).await;
    Ok(())
}

async fn get_gateway_url(access_token: &str) -> Result<String> {
    let url = "https://api.sgroup.qq.com/gateway";
    let client = HttpClient::new();

    let response = client
        .get(url)
        .header("Authorization", format!("QQBot {}", access_token))
        .send()
        .await
        .context("Failed to get gateway URL")?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(anyhow!("Gateway request failed: {} - {}", status, text));
    }

    let gateway_resp: GatewayResponse = response
        .json()
        .await
        .context("Failed to parse gateway response")?;

    Ok(gateway_resp.url)
}

#[derive(Debug, Deserialize)]
struct GatewayResponse {
    url: String,
}

fn parse_ws_message<T: serde::de::DeserializeOwned>(msg: &WsMessage) -> Result<T> {
    match msg {
        WsMessage::Text(text) => serde_json::from_str(text).context("Failed to parse JSON"),
        WsMessage::Binary(bin) => {
            serde_json::from_slice(bin).context("Failed to parse binary JSON")
        }
        _ => Err(anyhow!("Unsupported message type")),
    }
}

async fn handle_event(
    payload: &GatewayPayload,
    recent_msg_ids: &Arc<RwLock<HashMap<String, String>>>,
    incoming_tx: &mpsc::Sender<RawIncoming>,
) -> Result<()> {
    let event_type = payload.event_type.as_deref().unwrap_or("");
    let data = payload
        .data
        .as_ref()
        .ok_or_else(|| anyhow!("Missing event data"))?;

    match event_type {
        "C2C_MESSAGE_CREATE" => {
            let msg: QqMessage = serde_json::from_value(data.clone())?;
            let chat_id = ChatId(format!("c2c:{}", msg.author.user_openid));

            // Store message ID for passive reply
            {
                let mut ids = recent_msg_ids.write().await;
                ids.insert(chat_id.0.clone(), msg.id.clone());
            }

            let raw = RawIncoming {
                chat_id,
                text: msg.content,
                is_group: false,
                is_mentioned: false,
            };

            let _ = incoming_tx.send(raw).await;
        }
        "GROUP_AT_MESSAGE_CREATE" => {
            let msg: QqMessage = serde_json::from_value(data.clone())?;
            let chat_id = ChatId(format!(
                "group:{}",
                msg.group_openid.as_deref().unwrap_or("")
            ));

            // Store message ID for passive reply
            {
                let mut ids = recent_msg_ids.write().await;
                ids.insert(chat_id.0.clone(), msg.id.clone());
            }

            // For group @ messages, the bot is always mentioned
            let raw = RawIncoming {
                chat_id,
                text: msg.content,
                is_group: true,
                is_mentioned: true,
            };

            let _ = incoming_tx.send(raw).await;
        }
        "READY" | "RESUMED" => {
            info!(event = %event_type, "QQ Bot session event");
        }
        _ => {
            debug!(event = %event_type, "Unhandled event");
        }
    }

    Ok(())
}

#[derive(Debug, Deserialize)]
struct QqMessage {
    id: String,
    content: String,
    #[serde(rename = "author")]
    author: Author,
    #[serde(rename = "group_openid")]
    group_openid: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Author {
    #[serde(rename = "user_openid")]
    user_openid: String,
}

async fn send_message(
    http_client: &HttpClient,
    token_manager: &TokenManager,
    chat_id: &ChatId,
    content: &str,
    recent_msg_ids: &Arc<RwLock<HashMap<String, String>>>,
) -> Result<()> {
    let access_token = token_manager.get_token().await?;

    // Parse chat_id format: "c2c:openid" or "group:group_openid"
    let (msg_type, openid) = if let Some(id) = chat_id.0.strip_prefix("c2c:") {
        ("users", id)
    } else if let Some(id) = chat_id.0.strip_prefix("group:") {
        ("groups", id)
    } else {
        return Err(anyhow!("Invalid chat_id format: {}", chat_id.0));
    };

    // Get the message ID for passive reply
    let msg_id = {
        let ids = recent_msg_ids.read().await;
        ids.get(&chat_id.0).cloned()
    };

    // Build API URL
    let url = if msg_type == "users" {
        format!("https://api.sgroup.qq.com/v2/users/{}/messages", openid)
    } else {
        format!("https://api.sgroup.qq.com/v2/groups/{}/messages", openid)
    };

    // Send message (truncate if too long, QQ recommends 2000 chars)
    let content = if content.chars().count() > 2000 {
        let truncated: String = content.chars().take(1997).collect();
        format!("{}...", truncated)
    } else {
        content.to_string()
    };

    let mut body = serde_json::json!({
        "content": content,
        "msg_type": 0,
        "msg_seq": generate_msg_seq(),
    });

    // Add msg_id for passive reply if available
    if let Some(id) = msg_id {
        body["msg_id"] = serde_json::json!(id);
    }

    let response = http_client
        .post(&url)
        .header("Authorization", format!("QQBot {}", access_token))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .context("Failed to send message request")?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(anyhow!("Send message failed: {} - {}", status, text));
    }

    debug!("Message sent successfully");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_id_parsing() {
        let c2c_id = ChatId("c2c:test_user_openid".to_string());
        assert!(c2c_id.0.starts_with("c2c:"));

        let group_id = ChatId("group:test_group_openid".to_string());
        assert!(group_id.0.starts_with("group:"));
    }

    #[test]
    fn test_qq_adapter_new() {
        let adapter = QqAdapter::new("app_id".to_string(), "secret".to_string(), "test-channel".to_string());
        assert_eq!(adapter.platform(), "qq");
        assert_eq!(adapter.max_message_length(), 4096);
    }
}
