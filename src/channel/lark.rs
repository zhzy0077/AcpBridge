//! Lark/Feishu Bot channel implementation
//!
//! Implements WebSocket-based connection to Lark/Feishu Open Platform.
//! Supports p2p (single chat) and Group (@bot) message scenarios.

use crate::channel::{
    Channel, ChatId, IncomingContent, IncomingMessage, OutgoingContent, OutgoingMessage,
    should_dispatch_message,
};
use crate::message_bus::BotInstanceKey;
use anyhow::{Context, Result, anyhow};
use dashmap::DashMap;
use reqwest::Client as HttpClient;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime};
use tokio::sync::{RwLock, mpsc};
use tokio::time::{MissedTickBehavior, interval, sleep};
use tracing::{debug, error, info, warn};

/// Static registry: Lark open_id → channel_name.
/// Populated at startup when each LarkChannel connects and fetches its own open_id.
static LARK_BOT_REGISTRY: LazyLock<DashMap<String, String>> = LazyLock::new(DashMap::new);

/// Lark Bot channel
#[derive(Debug)]
pub struct LarkChannel {
    mention_only: bool,
    /// Shared across start() and search_bots_in_chat()
    http_client: HttpClient,
    token_manager: Arc<TokenManager>,
    bot_id_cache: Arc<tokio::sync::OnceCell<Option<String>>>,
    /// Cache: raw_chat_id (without "group:" prefix) → Vec<open_ids of bot members>
    chat_members_cache: DashMap<String, Vec<String>>,
}

impl LarkChannel {
    pub fn new(app_id: String, app_secret: String, base_url: String, mention_only: bool) -> Self {
        let http_client = HttpClient::new();
        let token_manager = Arc::new(TokenManager::new(
            app_id,
            app_secret,
            base_url,
            http_client.clone(),
        ));
        Self {
            mention_only,
            http_client,
            token_manager,
            bot_id_cache: Arc::new(tokio::sync::OnceCell::new()),
            chat_members_cache: DashMap::new(),
        }
    }
}

#[async_trait::async_trait]
impl Channel for LarkChannel {
    async fn start(
        &self,
        channel_name: String,
        orchestrator: Arc<crate::orchestrator::Orchestrator>,
        message_bus: Arc<crate::message_bus::MessageBus>,
    ) -> Result<()> {
        // Eagerly register own open_id so peers can discover us via LARK_BOT_REGISTRY
        if let Some(open_id) = get_bot_open_id(&self.token_manager, &self.bot_id_cache).await {
            LARK_BOT_REGISTRY.insert(open_id, channel_name.clone());
            info!(channel = %channel_name, "Registered Lark bot in registry");
        } else {
            warn!(channel = %channel_name, "Could not fetch Lark bot open_id; search_bots will be unavailable");
        }

        let manager = Arc::new(LarkBotManager::new(
            self.http_client.clone(),
            self.token_manager.clone(),
            self.mention_only,
            channel_name,
            orchestrator,
            message_bus,
            self.bot_id_cache.clone(),
        ));

        // Create outgoing channel and register with MessageBus
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<(ChatId, OutgoingMessage)>(100);
        manager
            .message_bus
            .register_channel(manager.channel_name.clone(), outgoing_tx)
            .await;

        manager.run(outgoing_rx).await
    }

    async fn search_bots_in_chat(&self, chat_id: &ChatId) -> Vec<String> {
        let raw_chat_id = match chat_id.0.strip_prefix("group:") {
            Some(id) => id,
            None => return vec![], // p2p chats never have multiple bots
        };

        let own_open_id = self
            .bot_id_cache
            .get()
            .and_then(|o| o.as_deref())
            .unwrap_or("")
            .to_string();

        // Check cache first
        let member_ids = if let Some(cached) = self.chat_members_cache.get(raw_chat_id) {
            cached.clone()
        } else {
            match fetch_chat_member_bot_ids(&self.http_client, &self.token_manager, raw_chat_id)
                .await
            {
                Ok(ids) => {
                    self.chat_members_cache
                        .insert(raw_chat_id.to_string(), ids.clone());
                    ids
                }
                Err(e) => {
                    warn!(error = %e, chat_id = %chat_id.0, "Failed to fetch Lark chat members");
                    return vec![];
                }
            }
        };

        member_ids
            .iter()
            .filter(|id| id.as_str() != own_open_id)
            .filter_map(|id| LARK_BOT_REGISTRY.get(id).map(|e| e.value().clone()))
            .collect()
    }
}

/// Lark Bot connection manager
struct LarkBotManager {
    http_client: HttpClient,
    token_manager: Arc<TokenManager>,
    channel_name: String,
    orchestrator: Arc<crate::orchestrator::Orchestrator>,
    message_bus: Arc<crate::message_bus::MessageBus>,
    mention_only: bool,
    bot_id_cache: Arc<tokio::sync::OnceCell<Option<String>>>,
}

impl LarkBotManager {
    fn new(
        http_client: HttpClient,
        token_manager: Arc<TokenManager>,
        mention_only: bool,
        channel_name: String,
        orchestrator: Arc<crate::orchestrator::Orchestrator>,
        message_bus: Arc<crate::message_bus::MessageBus>,
        bot_id_cache: Arc<tokio::sync::OnceCell<Option<String>>>,
    ) -> Self {
        Self {
            http_client,
            token_manager,
            channel_name,
            orchestrator,
            message_bus,
            mention_only,
            bot_id_cache,
        }
    }

    async fn run(&self, mut outgoing_rx: mpsc::Receiver<(ChatId, OutgoingMessage)>) -> Result<()> {
        let token_manager = self.token_manager.clone();
        let channel_name = self.channel_name.clone();
        let orchestrator = self.orchestrator.clone();
        let message_bus = self.message_bus.clone();
        let base_url = self.token_manager.base_url.clone();
        let mention_only = self.mention_only;
        let bot_id_cache = self.bot_id_cache.clone();

        // Spawn WebSocket connection task
        let ws_handle = tokio::spawn(async move {
            let mut reconnect_delay = Duration::from_secs(1);
            const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);

            loop {
                match run_websocket_connection(
                    &token_manager,
                    &channel_name,
                    &orchestrator,
                    &message_bus,
                    &base_url,
                    mention_only,
                    &bot_id_cache,
                )
                .await
                {
                    Ok(()) => {
                        // This should not happen - run_websocket_connection only returns Ok on programming errors
                        // Treat as error and reconnect
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
        });

        // Handle outgoing messages
        let token_manager = self.token_manager.clone();
        let http_client = self.http_client.clone();

        let outgoing_handle = tokio::spawn(async move {
            // Local state management for stream buffering
            let mut stream_buffers: HashMap<String, (String, std::time::Instant)> = HashMap::new();

            // Timer for checking timeouts (5 seconds for more responsive streaming)
            let mut timeout_checker = interval(Duration::from_secs(5));
            timeout_checker.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    // Process new outgoing message
                    Some((chat_id, msg)) = outgoing_rx.recv() => {
                        let chat_id_str = chat_id.0.clone();

                        match msg.content {
                            OutgoingContent::Text(text) => {
                                // Non-streaming message, send directly (with splitting if needed)
                                if let Err(e) = send_long_message(
                                    &http_client,
                                    &token_manager,
                                    &chat_id,
                                    &text,
                                ).await {
                                    error!(error = %e, chat_id = %chat_id_str, "Failed to send message");
                                }
                            }
                            OutgoingContent::StreamChunk(text) => {
                                // Buffer mode: accumulate to buffer, update timestamp
                                let now = std::time::Instant::now();
                                let should_flush = {
                                    let entry = stream_buffers
                                        .entry(chat_id_str.clone())
                                        .and_modify(|(buf, ts)| {
                                            buf.push_str(&text);
                                            *ts = now;
                                        })
                                        .or_insert((text.clone(), now));

                                    // Pseudo-streaming: flush if buffer exceeds 500 chars
                                    entry.0.chars().count() >= 500
                                };

                                // Flush immediately if buffer is large enough
                                if should_flush
                                    && let Some((content, _)) = stream_buffers.remove(&chat_id_str)
                                    && !content.is_empty()
                                    && let Err(e) = send_long_message(
                                        &http_client,
                                        &token_manager,
                                        &chat_id,
                                        &content,
                                    )
                                    .await
                                {
                                    error!(error = %e, chat_id = %chat_id_str, "Failed to send stream chunk");
                                }
                            }
                            OutgoingContent::StreamEnd => {
                                // Stream end: send buffered content and remove from HashMap (avoid memory leak)
                                if let Some((content, _)) = stream_buffers.remove(&chat_id_str)
                                    && !content.is_empty()
                                    && let Err(e) = send_long_message(
                                        &http_client,
                                        &token_manager,
                                        &chat_id,
                                        &content,
                                    )
                                    .await
                                {
                                    error!(error = %e, chat_id = %chat_id_str, "Failed to send stream message");
                                }
                            }
                            OutgoingContent::Error(err) => {
                                let content = format!("Error: {}", err);
                                if let Err(e) = send_long_message(
                                    &http_client,
                                    &token_manager,
                                    &chat_id,
                                    &content,
                                ).await {
                                    error!(error = %e, chat_id = %chat_id_str, "Failed to send error");
                                }
                            }
                            OutgoingContent::Mention { target_bot, target_channel_name, message } => {
                                // Look up target's open_id via the static registry
                                let open_id = target_channel_name.as_deref().and_then(|cn| {
                                    LARK_BOT_REGISTRY
                                        .iter()
                                        .find(|e| e.value() == cn)
                                        .map(|e| e.key().clone())
                                });
                                let full_message = match open_id {
                                    Some(id) => format!(
                                        "<at user_id=\"{}\">@{}</at> {}",
                                        id, target_bot, message
                                    ),
                                    None => {
                                        warn!(target_bot = %target_bot, "Target bot open_id not in registry, falling back to plain mention");
                                        format!("@{} {}", target_bot, message)
                                    }
                                };
                                if let Err(e) = send_long_message(
                                    &http_client,
                                    &token_manager,
                                    &chat_id,
                                    &full_message,
                                ).await {
                                    error!(error = %e, chat_id = %chat_id_str, "Failed to send mention");
                                }
                            }
                        }
                    }
                    // Periodic timeout check (10 seconds for better responsiveness)
                    _ = timeout_checker.tick() => {
                        check_and_flush_timeouts(
                            &http_client,
                            &token_manager,
                            &mut stream_buffers,
                        ).await;
                    }
                    // Exit when channel closed
                    else => break,
                }
            }
        });

        tokio::select! {
            _ = ws_handle => {},
            _ = outgoing_handle => {},
        }

        Ok(())
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
    /// Mutex to ensure only one request fetches token at a time
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
        // Fast path: Check if we have a valid cached token (read lock only)
        {
            let token_guard = self.token.read().await;
            if let Some(token) = token_guard.as_ref()
                && SystemTime::now() + Duration::from_secs(300) < token.expires_at
            {
                return Ok(token.access_token.clone());
            }
        }

        // Slow path: Need to fetch new token, acquire exclusive lock
        let _guard = self.refresh_lock.lock().await;

        // Double-check after acquiring lock (another request might have refreshed)
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

/// Fetch the open_ids of all bot members in a Lark group chat.
async fn fetch_chat_member_bot_ids(
    http_client: &HttpClient,
    token_manager: &TokenManager,
    raw_chat_id: &str,
) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct MemberItem {
        member_id: String,
        member_type: String,
    }
    #[derive(Deserialize)]
    struct MembersData {
        items: Vec<MemberItem>,
        page_token: Option<String>,
        has_more: bool,
    }
    #[derive(Deserialize)]
    struct MembersResp {
        data: MembersData,
    }

    let url = format!(
        "{}/open-apis/im/v1/chats/{}/members",
        token_manager.base_url, raw_chat_id
    );
    let mut all_bot_ids = Vec::new();
    let mut page_token: Option<String> = None;

    loop {
        let token = token_manager.get_token().await?;
        let mut request = http_client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .query(&[("member_id_type", "open_id")]);
        if let Some(ref pt) = page_token {
            request = request.query(&[("page_token", pt.as_str())]);
        }

        let response = request
            .send()
            .await
            .context("Failed to send chat members request")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("Chat members request failed: {} - {}", status, text));
        }

        let body = response
            .text()
            .await
            .context("Failed to read chat members response body")?;
        tracing::debug!(body = %body, "Chat members raw response");
        let resp: MembersResp = serde_json::from_str(&body)
            .with_context(|| format!("Failed to parse chat members response: {}", body))?;

        for member in resp.data.items {
            if member.member_type == "bot" {
                all_bot_ids.push(member.member_id);
            }
        }

        if !resp.data.has_more {
            break;
        }
        page_token = resp.data.page_token;
    }

    Ok(all_bot_ids)
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
    channel_name: &str,
    orchestrator: &Arc<crate::orchestrator::Orchestrator>,
    message_bus: &Arc<crate::message_bus::MessageBus>,
    base_url: &str,
    mention_only: bool,
    bot_id_cache: &Arc<tokio::sync::OnceCell<Option<String>>>,
) -> Result<()> {
    use openlark_client::Config as LarkConfig;
    use openlark_client::ws_client::{EventDispatcherHandler, LarkWsClient};
    use std::sync::Arc;

    // Build openlark config
    let ws_config = LarkConfig::builder()
        .app_id(token_manager.app_id.clone())
        .app_secret(token_manager.app_secret.clone())
        .base_url(base_url)
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
                    channel_name,
                    orchestrator,
                    message_bus,
                    token_manager,
                    mention_only,
                    bot_id_cache,
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

/// Parse message content and extract command or text
///
/// Lark's @mention format: <at user_id="ou_xxx">@Name</at>
/// We need to be permissive about characters before the command
fn parse_message_content(content: &str) -> IncomingContent {
    let content = content.trim();

    // Find command position - look for "/" followed by a letter at word boundary
    // The command should be:
    // 1. At the start of the message, OR
    // 2. After whitespace (not inside a tag like </at>)
    let mut cmd_pos = None;
    let chars: Vec<char> = content.chars().collect();

    for (i, c) in chars.iter().enumerate() {
        if *c == '/' {
            // Check if this "/" is at a command position:
            // - Must be at start OR preceded by whitespace
            // - Next char must exist and be a letter
            let is_at_start = i == 0;
            let is_after_whitespace = i > 0 && chars[i - 1].is_whitespace();
            let is_command_position = is_at_start || is_after_whitespace;

            if is_command_position && i + 1 < chars.len() && chars[i + 1].is_ascii_alphabetic() {
                cmd_pos = Some(i);
                break;
            }
        }
    }

    if let Some(pos) = cmd_pos {
        let before_cmd = &content[..pos];
        let after_cmd = &content[pos + 1..];

        // Check if before_cmd only contains whitespace and @mentions.
        // Lark uses two mention formats:
        //   - New: @_user_N placeholder (e.g. "@_user_1 ") with a separate mentions array
        //   - Legacy XML: <at user_id="ou_xxx">@Name</at>
        // Both are handled by the permissive character allowlist below.
        let is_only_mentions = before_cmd.chars().all(|c| {
            c.is_whitespace()
                || c.is_ascii_alphanumeric()
                || matches!(c, '<' | '>' | '/' | '=' | '"' | '_' | '@' | '!')
        });

        if is_only_mentions {
            let rest = after_cmd.trim_start();
            let parts: Vec<&str> = rest.splitn(2, ' ').collect();
            let name = parts[0].to_lowercase();
            let args = parts.get(1).map(|s| s.to_string());
            return IncomingContent::Command { name, args };
        }
    }

    // Not a command
    IncomingContent::Text(content.to_string())
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

async fn handle_event(
    payload: &[u8],
    channel_name: &str,
    orchestrator: &Arc<crate::orchestrator::Orchestrator>,
    message_bus: &Arc<crate::message_bus::MessageBus>,
    token_manager: &Arc<TokenManager>,
    mention_only: bool,
    bot_id_cache: &Arc<tokio::sync::OnceCell<Option<String>>>,
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

    let content = parse_message_content(&text_content.text);
    let is_group_chat = message_event.message.chat_type == "group";
    let is_mentioned = if mention_only && is_group_chat {
        get_bot_open_id(token_manager, bot_id_cache)
            .await
            .is_some_and(|bot_open_id| {
                message_mentions_bot(
                    &text_content.text,
                    &message_event.message.mentions,
                    &bot_open_id,
                )
            })
    } else {
        false
    };
    if !should_dispatch_message(mention_only, is_group_chat, is_mentioned) {
        return Ok(());
    }

    // Immediately acknowledge the message with a reaction to satisfy Lark's 3-second
    // long-connection processing requirement and signal to the user that the bot is working.
    let tm = Arc::clone(token_manager);
    let message_id = message_event.message.message_id.clone();
    tokio::spawn(async move {
        if let Err(e) = send_reaction(&tm.http_client, &tm, &message_id).await {
            warn!(error = %e, message_id = %message_id, "Failed to send reaction");
        }
    });

    // Check for /bot command
    if let IncomingContent::Command {
        name: cmd_name,
        args,
    } = &content
        && cmd_name == "bot"
    {
        orchestrator
            .handle_bot_command(channel_name, &chat_id, args.clone())
            .await?;
        return Ok(());
    }

    // Get or create bot and dispatch
    match orchestrator.get_or_create(channel_name, &chat_id).await {
        Some(bot_name) => {
            let key = BotInstanceKey {
                channel_name: channel_name.to_string(),
                chat_id: chat_id.clone(),
                bot_name,
            };
            message_bus
                .dispatch(&key, IncomingMessage { content })
                .await
                .context("Failed to send incoming message")?;
        }
        None => {
            error!("Failed to get or create bot for chat");
        }
    }

    Ok(())
}

/// Send a message, automatically splitting if it exceeds the max length
async fn send_long_message(
    http_client: &HttpClient,
    token_manager: &TokenManager,
    chat_id: &ChatId,
    content: &str,
) -> Result<()> {
    const MAX_MESSAGE_LENGTH: usize = 4000;

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

/// Check and flush timed-out stream buffers (10 second timeout for better responsiveness)
async fn check_and_flush_timeouts(
    http_client: &HttpClient,
    token_manager: &TokenManager,
    stream_buffers: &mut HashMap<String, (String, std::time::Instant)>,
) {
    const STREAM_TIMEOUT: Duration = Duration::from_secs(10);
    const MAX_BUFFER_COUNT: usize = 1000; // Memory safety limit
    let now = std::time::Instant::now();

    // Check for timeout
    let expired_chat_ids: Vec<String> = stream_buffers
        .iter()
        .filter(|(_, (_, ts))| now.duration_since(*ts) > STREAM_TIMEOUT)
        .map(|(chat_id, _)| chat_id.clone())
        .collect();

    for chat_id_str in expired_chat_ids {
        warn!(chat_id = %chat_id_str, "Stream timeout, auto-flushing");

        if let Some((content, _)) = stream_buffers.remove(&chat_id_str)
            && !content.is_empty()
        {
            let chat_id = ChatId(chat_id_str.clone());
            if let Err(e) = send_long_message(http_client, token_manager, &chat_id, &content).await
            {
                error!(error = %e, chat_id = %chat_id_str, "Failed to send timeout stream message");
            }
        }
    }

    // Memory safety: if too many buffers, flush the oldest ones
    if stream_buffers.len() > MAX_BUFFER_COUNT {
        let mut entries: Vec<(String, std::time::Instant)> = stream_buffers
            .iter()
            .map(|(k, (_, ts))| (k.clone(), *ts))
            .collect();
        entries.sort_by(|a, b| a.1.cmp(&b.1));

        let to_remove = stream_buffers.len() - MAX_BUFFER_COUNT;
        for (chat_id_str, _) in entries.into_iter().take(to_remove) {
            warn!(chat_id = %chat_id_str, "Too many stream buffers, flushing oldest");

            if let Some((content, _)) = stream_buffers.remove(&chat_id_str)
                && !content.is_empty()
            {
                let chat_id = ChatId(chat_id_str.clone());
                if let Err(e) =
                    send_long_message(http_client, token_manager, &chat_id, &content).await
                {
                    error!(error = %e, chat_id = %chat_id_str, "Failed to send buffer overflow message");
                }
            }
        }
    }
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
    fn test_parse_message_content_text() {
        let content = parse_message_content("Hello world");
        match content {
            IncomingContent::Text(t) => assert_eq!(t, "Hello world"),
            _ => panic!("Expected Text content"),
        }
    }

    #[test]
    fn test_parse_message_content_command() {
        let content = parse_message_content("/help");
        match content {
            IncomingContent::Command { name, args } => {
                assert_eq!(name, "help");
                assert!(args.is_none());
            }
            _ => panic!("Expected Command content"),
        }
    }

    #[test]
    fn test_parse_message_content_command_with_args() {
        let content = parse_message_content("/model gpt-4");
        match content {
            IncomingContent::Command { name, args } => {
                assert_eq!(name, "model");
                assert_eq!(args, Some("gpt-4".to_string()));
            }
            _ => panic!("Expected Command content"),
        }
    }

    #[test]
    fn test_parse_message_content_command_with_at() {
        let content = parse_message_content("<@!123456> /help");
        match content {
            IncomingContent::Command { name, args } => {
                assert_eq!(name, "help");
                assert!(args.is_none());
            }
            _ => panic!("Expected Command content"),
        }
    }

    #[test]
    fn test_parse_message_content_lark_at_format() {
        // Test Lark's actual @ format
        let content = parse_message_content("<at user_id=\"ou_123abc\">@Bot</at> /help");
        match content {
            IncomingContent::Command { name, args } => {
                assert_eq!(name, "help");
                assert!(args.is_none());
            }
            _ => panic!("Expected Command content for Lark @ format"),
        }
    }

    #[test]
    fn test_parse_message_content_lark_at_with_args() {
        let content = parse_message_content("<at user_id=\"ou_xxx\">@Bot</at> /model gpt-4");
        match content {
            IncomingContent::Command { name, args } => {
                assert_eq!(name, "model");
                assert_eq!(args, Some("gpt-4".to_string()));
            }
            _ => panic!("Expected Command content"),
        }
    }

    #[test]
    fn test_message_mentions_bot() {
        // New format: @_user_N placeholder in text + mentions array
        let mentions = vec![MentionEntry {
            id: MentionId {
                open_id: "ou_bot".to_string(),
            },
        }];
        assert!(message_mentions_bot("@_user_1 hello", &mentions, "ou_bot"));
        assert!(!message_mentions_bot("@_user_1 hello", &mentions, "ou_other"));

        // Legacy XML format: <at user_id="ou_xxx"> in text, no mentions array
        assert!(message_mentions_bot(
            "<at user_id=\"ou_bot\">@Bot</at> hello",
            &[],
            "ou_bot"
        ));
        assert!(!message_mentions_bot(
            "<at user_id=\"ou_other\">@Other</at> hello",
            &[],
            "ou_bot"
        ));
    }
}
