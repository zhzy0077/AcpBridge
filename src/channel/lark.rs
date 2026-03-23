//! Lark/Feishu Bot channel adapter implementation
//!
//! Implements WebSocket-based connection to Lark/Feishu Open Platform.
//! Supports p2p (single chat) and Group (@bot) message scenarios.

use crate::channel::{ChannelAdapter, ChatId, RawIncoming};
use anyhow::{Context, Result, anyhow};
use dashmap::DashMap;
use reqwest::Client as HttpClient;
use serde::Deserialize;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime};
use tokio::sync::{RwLock, mpsc};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

/// Static registry: Lark open_id → channel_name.
/// Populated at startup when each LarkAdapter connects and fetches its own open_id.
static LARK_BOT_REGISTRY: LazyLock<DashMap<String, String>> = LazyLock::new(DashMap::new);

/// Lark single message max length
const MAX_MESSAGE_LENGTH: usize = 4000;

/// Lark Bot adapter
#[derive(Debug)]
pub struct LarkAdapter {
    http_client: HttpClient,
    token_manager: Arc<TokenManager>,
    bot_id_cache: Arc<tokio::sync::OnceCell<Option<String>>>,
    channel_name: String,
}

impl LarkAdapter {
    pub fn new(app_id: String, app_secret: String, base_url: String, channel_name: String) -> Self {
        let http_client = HttpClient::new();
        let token_manager = Arc::new(TokenManager::new(
            app_id,
            app_secret,
            base_url,
            http_client.clone(),
        ));
        Self {
            http_client,
            token_manager,
            bot_id_cache: Arc::new(tokio::sync::OnceCell::new()),
            channel_name,
        }
    }

    /// Get the bot registry for mention formatting
    pub fn get_registry() -> &'static DashMap<String, String> {
        &LARK_BOT_REGISTRY
    }

    /// Get the base URL from the token manager
    pub fn base_url(&self) -> &str {
        &self.token_manager.base_url
    }

    /// Fetch and register bot open_id in the registry
    async fn register_bot_id(&self) {
        if let Some(open_id) = get_bot_open_id(&self.token_manager, &self.bot_id_cache).await {
            LARK_BOT_REGISTRY.insert(open_id, self.channel_name.clone());
            info!(channel = %self.channel_name, "Registered Lark bot in registry");
        } else {
            warn!(channel = %self.channel_name, "Could not fetch Lark bot open_id; mentions will use plain text");
        }
    }
}

#[async_trait::async_trait]
impl ChannelAdapter for LarkAdapter {
    fn platform(&self) -> &'static str {
        "lark"
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

        let mut reconnect_delay = Duration::from_secs(1);
        const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);

        loop {
            match run_websocket_connection(
                &self.token_manager,
                &self.bot_id_cache,
                incoming_tx.clone(),
            )
            .await
            {
                Ok(()) => {
                    error!(
                        "Lark WebSocket returned Ok unexpectedly, reconnecting in {:?}",
                        reconnect_delay
                    );
                    sleep(reconnect_delay).await;
                    reconnect_delay = std::cmp::min(reconnect_delay * 2, MAX_RECONNECT_DELAY);
                }
                Err(e) => {
                    error!(error = %e, "Lark WebSocket connection error, reconnecting in {:?}", reconnect_delay);
                    sleep(reconnect_delay).await;
                    reconnect_delay = std::cmp::min(reconnect_delay * 2, MAX_RECONNECT_DELAY);
                }
            }
        }
    }

    async fn send_text(&self, chat_id: &ChatId, text: &str) -> Result<()> {
        send_long_message(&self.http_client, &self.token_manager, chat_id, text).await
    }

    fn format_mention(
        &self,
        target_bot: &str,
        target_channel_name: Option<&str>,
        message: &str,
    ) -> String {
        // Look up target's open_id via the static registry
        let open_id = target_channel_name.and_then(|cn| {
            LARK_BOT_REGISTRY
                .iter()
                .find(|e| e.value() == cn)
                .map(|e| e.key().clone())
        });
        match open_id {
            Some(id) => format!(
                "<at user_id=\"{}\">@{}</at> {}",
                id, target_bot, message
            ),
            None => {
                warn!(target_bot = %target_bot, "Target bot open_id not in registry, falling back to plain mention");
                format!("@{} {}", target_bot, message)
            }
        }
    }
}

/// Access token manager with automatic refresh and thundering herd protection
#[derive(Debug)]
struct TokenManager {
    app_id: String,
    app_secret: String,
    base_url: String,
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
    fn new(app_id: String, app_secret: String, base_url: String, http_client: HttpClient) -> Self {
        Self {
            app_id,
            app_secret,
            base_url,
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
        let url = format!(
            "{}/open-apis/auth/v3/app_access_token/internal",
            self.base_url
        );
        let body = serde_json::json!({
            "app_id": self.app_id,
            "app_secret": self.app_secret,
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

        let expires_in: u64 = token_response.expire;
        let expires_at = SystemTime::now() + Duration::from_secs(expires_in);
        let token_info = TokenInfo {
            access_token: token_response.app_access_token.clone(),
            expires_at,
        };

        let mut token_guard = self.token.write().await;
        *token_guard = Some(token_info);

        info!("Successfully obtained Lark app access token");
        Ok(token_response.app_access_token)
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    app_access_token: String,
    expire: u64,
}

/// Event envelope from Lark WebSocket
#[derive(Debug, Deserialize)]
struct EventEnvelope {
    header: EventHeader,
    #[serde(default)]
    event: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct EventHeader {
    #[serde(rename = "event_type")]
    event_type: String,
}

/// Message receive event body
#[derive(Debug, Deserialize)]
struct MessageEvent {
    sender: Sender,
    message: Message,
    #[serde(default)]
    chat: Option<Chat>,
}

#[derive(Debug, Deserialize)]
struct Sender {
    #[serde(rename = "sender_id")]
    sender_id: SenderId,
}

#[derive(Debug, Deserialize)]
struct SenderId {
    #[serde(rename = "open_id")]
    open_id: String,
}

#[derive(Debug, Deserialize)]
struct MentionId {
    open_id: String,
}

#[derive(Debug, Deserialize)]
struct MentionEntry {
    id: MentionId,
}

#[derive(Debug, Deserialize)]
struct Message {
    #[serde(rename = "message_id")]
    message_id: String,
    #[serde(rename = "message_type")]
    message_type: String,
    content: String,
    #[serde(rename = "chat_type")]
    chat_type: String,
    #[serde(rename = "chat_id")]
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    mentions: Vec<MentionEntry>,
}

#[derive(Debug, Deserialize)]
struct Chat {
    #[serde(rename = "chat_id")]
    chat_id: String,
}

#[derive(Debug, Deserialize)]
struct TextContent {
    text: String,
}

async fn get_bot_open_id(
    token_manager: &TokenManager,
    bot_id_cache: &tokio::sync::OnceCell<Option<String>>,
) -> Option<String> {
    bot_id_cache
        .get_or_init(|| async {
            let token = match token_manager.get_token().await {
                Ok(token) => token,
                Err(_) => return None,
            };

            let url = format!("{}/open-apis/bot/v3/info", token_manager.base_url);

            #[derive(Deserialize)]
            struct BotResp {
                open_id: String,
            }

            #[derive(Deserialize)]
            struct Resp {
                bot: BotResp,
            }

            let response = match token_manager
                .http_client
                .get(&url)
                .header("Authorization", format!("Bearer {}", token))
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => response,
                _ => return None,
            };

            response.json::<Resp>().await.ok().map(|resp| resp.bot.open_id)
        })
        .await
        .clone()
}

fn message_mentions_bot(text: &str, mentions: &[MentionEntry], bot_open_id: &str) -> bool {
    // Current Lark format: @_user_N placeholder in text + mentions array with real open_ids
    if mentions.iter().any(|m| m.id.open_id == bot_open_id) {
        return true;
    }
    // Legacy XML format: <at user_id="ou_xxx">@Name</at> embedded in text
    let mention = format!("<at user_id=\"{}\">", bot_open_id);
    text.contains(&mention)
}

async fn run_websocket_connection(
    token_manager: &Arc<TokenManager>,
    bot_id_cache: &Arc<tokio::sync::OnceCell<Option<String>>>,
    incoming_tx: mpsc::Sender<RawIncoming>,
) -> Result<()> {
    use openlark_client::Config as LarkConfig;
    use openlark_client::ws_client::{EventDispatcherHandler, LarkWsClient};
    use std::sync::Arc;

    // Build openlark config
    let ws_config = LarkConfig::builder()
        .app_id(token_manager.app_id.clone())
        .app_secret(token_manager.app_secret.clone())
        .base_url(&token_manager.base_url)
        .build()
        .map_err(|e| anyhow!("Failed to build Lark config: {:?}", e))?;

    // Create event channel
    let (payload_tx, mut payload_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // Create event handler and start WebSocket in background
    let event_handler = EventDispatcherHandler::builder()
        .payload_sender(payload_tx)
        .build();

    let ws_config_arc = Arc::new(ws_config);

    // Spawn WebSocket client in background
    let mut ws_client_handle: tokio::task::JoinHandle<
        Result<(), openlark_client::ws_client::WsClientError>,
    > = tokio::spawn(async move { LarkWsClient::open(ws_config_arc, event_handler).await });

    info!("Lark WebSocket client started");

    // Get bot open_id for mention checking
    let bot_open_id = get_bot_open_id(token_manager, bot_id_cache).await;

    // Process events
    loop {
        tokio::select! {
            Some(payload) = payload_rx.recv() => {
                debug!(
                    payload = %String::from_utf8_lossy(&payload),
                    bytes = payload.len(),
                    "WebSocket message received"
                );
                if let Err(e) = handle_event(
                    &payload,
                    token_manager,
                    &incoming_tx,
                    bot_open_id.as_deref(),
                ).await {
                    error!(error = %e, "Failed to handle event");
                }
            }
            res = &mut ws_client_handle => {
                // WebSocket task exited - return error to trigger reconnection
                match res {
                    Ok(Ok(())) => {
                        return Err(anyhow!("Lark WebSocket client exited unexpectedly (clean exit)"));
                    }
                    Ok(Err(e)) => {
                        return Err(anyhow!("Lark WebSocket client error: {:?}", e));
                    }
                    Err(e) => {
                        return Err(anyhow!("Lark WebSocket client task panicked: {:?}", e));
                    }
                }
            }
        }
    }
}

async fn handle_event(
    payload: &[u8],
    token_manager: &Arc<TokenManager>,
    incoming_tx: &mpsc::Sender<RawIncoming>,
    bot_open_id: Option<&str>,
) -> Result<()> {
    let envelope: EventEnvelope =
        serde_json::from_slice(payload).context("Failed to parse event envelope")?;

    let event_type = envelope.header.event_type;
    debug!(event_type = %event_type, "Received Lark event");

    // Only handle text message receive events
    if event_type != "im.message.receive_v1" {
        debug!(event_type = %event_type, "Ignoring non-message event");
        return Ok(());
    }

    let event_data = envelope
        .event
        .ok_or_else(|| anyhow!("Missing event data"))?;
    let message_event: MessageEvent =
        serde_json::from_value(event_data).context("Failed to parse message event")?;

    // Only handle text messages
    if message_event.message.message_type != "text" {
        debug!(msg_type = %message_event.message.message_type, "Ignoring non-text message");
        return Ok(());
    }

    // Parse content JSON to get actual text
    let text_content: TextContent = serde_json::from_str(&message_event.message.content)
        .context("Failed to parse message content")?;

    // Determine chat_id based on chat_type
    let chat_id = match message_event.message.chat_type.as_str() {
        "p2p" => {
            // Single chat: use sender's open_id
            ChatId(format!("p2p:{}", message_event.sender.sender_id.open_id))
        }
        "group" => {
            // Group chat: use chat_id from message or chat
            if let Some(chat_id) = message_event.chat.as_ref().map(|c| c.chat_id.clone()) {
                ChatId(format!("group:{}", chat_id))
            } else if let Some(chat_id) = message_event.message.chat_id.clone() {
                ChatId(format!("group:{}", chat_id))
            } else {
                return Err(anyhow!("Missing chat_id for group message"));
            }
        }
        _ => {
            warn!(chat_type = %message_event.message.chat_type, "Unknown chat type");
            return Ok(());
        }
    };

    let is_group = message_event.message.chat_type == "group";
    let is_mentioned = bot_open_id.is_some_and(|bot_id| {
        message_mentions_bot(
            &text_content.text,
            &message_event.message.mentions,
            bot_id,
        )
    });

    // Immediately acknowledge the message with a reaction to satisfy Lark's 3-second
    // long-connection processing requirement and signal to the user that the bot is working.
    let tm = Arc::clone(token_manager);
    let message_id = message_event.message.message_id.clone();
    tokio::spawn(async move {
        if let Err(e) = send_reaction(&tm.http_client, &tm, &message_id).await {
            warn!(error = %e, message_id = %message_id, "Failed to send reaction");
        }
    });

    let raw = RawIncoming {
        chat_id,
        text: text_content.text,
        is_group,
        is_mentioned,
    };

    let _ = incoming_tx.send(raw).await;

    Ok(())
}

/// Send an emoji reaction to a Lark message as an immediate acknowledgement.
/// Best-effort: logs a warning on failure but never propagates the error.
async fn send_reaction(
    http_client: &HttpClient,
    token_manager: &TokenManager,
    message_id: &str,
) -> Result<()> {
    let access_token = token_manager.get_token().await?;

    let url = format!(
        "{}/open-apis/im/v1/messages/{}/reactions",
        token_manager.base_url, message_id
    );

    let body = serde_json::json!({
        "reaction_type": {
            "emoji_type": "THUMBSUP"
        }
    });

    let response = http_client
        .post(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .context("Failed to send reaction request")?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(anyhow!("Send reaction failed: {} - {}", status, text));
    }

    debug!(message_id = %message_id, "Reaction sent successfully");
    Ok(())
}

/// Send a message, automatically splitting if it exceeds the max length
async fn send_long_message(
    http_client: &HttpClient,
    token_manager: &TokenManager,
    chat_id: &ChatId,
    content: &str,
) -> Result<()> {
    let char_count = content.chars().count();

    if char_count <= MAX_MESSAGE_LENGTH {
        // Short message - send directly
        send_single_message(http_client, token_manager, chat_id, content).await
    } else {
        // Long message - split into chunks
        let mut start = 0;
        let mut chunk_index = 0;
        let total_chars = char_count;

        while start < total_chars {
            let end = std::cmp::min(start + MAX_MESSAGE_LENGTH, total_chars);

            // Get chunk using char indices
            let chunk: String = content.chars().skip(start).take(end - start).collect();

            // Add part indicator if split into multiple chunks
            let message = if total_chars > MAX_MESSAGE_LENGTH {
                let part_num = chunk_index + 1;
                let total_parts = total_chars.div_ceil(MAX_MESSAGE_LENGTH);
                format!("[Part {}/{}]\n{}", part_num, total_parts, chunk)
            } else {
                chunk
            };

            send_single_message(http_client, token_manager, chat_id, &message).await?;

            start = end;
            chunk_index += 1;

            // Small delay between chunks to avoid rate limiting
            if start < total_chars {
                sleep(Duration::from_millis(200)).await;
            }
        }

        Ok(())
    }
}

/// Send a single message (internal helper)
async fn send_single_message(
    http_client: &HttpClient,
    token_manager: &TokenManager,
    chat_id: &ChatId,
    content: &str,
) -> Result<()> {
    let access_token = token_manager.get_token().await?;

    // Parse chat_id format: "p2p:open_id" or "group:chat_id"
    let (receive_id_type, receive_id) = if let Some(id) = chat_id.0.strip_prefix("p2p:") {
        ("open_id", id)
    } else if let Some(id) = chat_id.0.strip_prefix("group:") {
        ("chat_id", id)
    } else {
        return Err(anyhow!("Invalid chat_id format: {}", chat_id.0));
    };

    // Build request body
    let body = serde_json::json!({
        "receive_id": receive_id,
        "msg_type": "text",
        "content": serde_json::json!({ "text": content }).to_string()
    });

    let url = format!("{}/open-apis/im/v1/messages", token_manager.base_url);

    let response = http_client
        .post(url)
        .query(&[("receive_id_type", receive_id_type)])
        .header("Authorization", format!("Bearer {}", access_token))
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

    debug!("Message sent successfully to {}", chat_id.0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_id_parsing() {
        let p2p_id = ChatId("p2p:test_open_id".to_string());
        assert!(p2p_id.0.starts_with("p2p:"));

        let group_id = ChatId("group:test_chat_id".to_string());
        assert!(group_id.0.starts_with("group:"));
    }

    #[test]
    fn test_lark_adapter_new() {
        let adapter = LarkAdapter::new(
            "app_id".to_string(),
            "secret".to_string(),
            "https://open.feishu.cn".to_string(),
            "test-channel".to_string(),
        );
        assert_eq!(adapter.platform(), "lark");
        assert_eq!(adapter.max_message_length(), 4000);
    }
}
