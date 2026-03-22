//! WeChat iLink channel implementation
//!
//! Implements HTTP long-poll connection to WeChat iLink Bot API.
//! Supports personal WeChat messaging via the iLink protocol.

use crate::channel::{
    should_dispatch_message, Channel, ChatId, IncomingContent, IncomingMessage, OutgoingContent,
    OutgoingMessage,
};
use crate::message_bus::BotInstanceKey;
use dashmap::DashMap;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, error, info, warn};

/// Generate X-WECHAT-UIN header value.
/// 
/// This header is required by the WeChat iLink API. According to the protocol,
/// it should be a random uint32 value encoded as base64.
fn generate_uin_header() -> String {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use rand::Rng;
    let n: u32 = rand::thread_rng().r#gen();
    STANDARD.encode(n.to_le_bytes())
}

/// WeChat iLink API base URL
const DEFAULT_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
/// Long-poll timeout duration
const POLL_TIMEOUT: Duration = Duration::from_secs(60);
/// Stream buffer timeout (30 seconds)
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum message length for WeChat (roughly 2000 chars to be safe)
const MAX_MESSAGE_LENGTH: usize = 2000;
/// Typing ticket TTL (24 hours)
const TYPING_TICKET_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Channel version for base_info
const CHANNEL_VERSION: &str = "acpconnector-0.1.0";
/// Message type constants
const MSG_TYPE_USER: i64 = 1;
const MSG_TYPE_BOT: i64 = 2;
const MSG_STATE_FINISH: i64 = 2;
const MSG_ITEM_TEXT: i64 = 1;
const MSG_ITEM_VOICE: i64 = 3;



/// WeChat channel configuration
#[derive(Debug, Clone)]
pub struct WeChatConfig {
    pub bot_token: String,
    pub base_url: String,
}

impl WeChatConfig {
    pub fn new(bot_token: String, base_url: Option<String>) -> Self {
        Self {
            bot_token,
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
        }
    }
}

/// WeChat channel implementation
#[derive(Debug, Clone)]
pub struct WeChatChannel {
    config: WeChatConfig,
    mention_only: bool,
}

impl WeChatChannel {
    pub fn new(config: WeChatConfig, mention_only: bool) -> Self {
        Self {
            config,
            mention_only,
        }
    }
}

/// Typing ticket cache entry
struct TypingTicketEntry {
    ticket: String,
    expiry: Instant,
}

#[async_trait::async_trait]
impl Channel for WeChatChannel {
    async fn start(
        &self,
        channel_name: String,
        orchestrator: Arc<crate::orchestrator::Orchestrator>,
        message_bus: Arc<crate::message_bus::MessageBus>,
    ) -> anyhow::Result<()> {
        // Create shared HTTP client to leverage connection pooling
        let http_client = Arc::new(HttpClient::new());
        let context_tokens: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        let typing_tickets: Arc<DashMap<String, TypingTicketEntry>> = Arc::new(DashMap::new());

        // Create outgoing channel and register with MessageBus
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<(ChatId, OutgoingMessage)>(100);
        message_bus
            .register_channel(channel_name.clone(), outgoing_tx)
            .await;

        // Spawn long-poll loop for incoming messages
        let poll_client = http_client.clone();
        let poll_config = self.config.clone();
        let poll_context_tokens = context_tokens.clone();
        let poll_typing_tickets = typing_tickets.clone();
        let poll_orchestrator = orchestrator.clone();
        let poll_message_bus = message_bus.clone();
        let poll_channel_name = channel_name.clone();
        let poll_mention_only = self.mention_only;

        let poll_handle = tokio::spawn(async move {
            let mut retry_delay = Duration::from_secs(1);
            const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

            loop {
                match run_poll_loop(
                    &poll_client,
                    &poll_config,
                    &poll_context_tokens,
                    &poll_typing_tickets,
                    &poll_orchestrator,
                    &poll_message_bus,
                    &poll_channel_name,
                    poll_mention_only,
                )
                .await
                {
                    Ok(()) => {
                        info!("Poll loop ended gracefully, restarting in {:?}", retry_delay);
                        tokio::time::sleep(retry_delay).await;
                        retry_delay = std::cmp::min(retry_delay * 2, MAX_RETRY_DELAY);
                    }
                    Err(e) => {
                        error!(error = %e, "Poll loop error, restarting in {:?}", retry_delay);
                        tokio::time::sleep(retry_delay).await;
                        retry_delay = std::cmp::min(retry_delay * 2, MAX_RETRY_DELAY);
                    }
                }
            }
        });

        // Handle outgoing messages
        let outgoing_client = http_client.clone();
        let outgoing_config = self.config.clone();
        let outgoing_context_tokens = context_tokens.clone();
        let outgoing_handle = tokio::spawn(async move {
            handle_outgoing_messages(
                outgoing_rx,
                &outgoing_client,
                &outgoing_config,
                &outgoing_context_tokens,
            )
            .await
        });

        tokio::select! {
            _ = poll_handle => {},
            _ = outgoing_handle => {},
        }

        Ok(())
    }
}

/// Run the long-poll loop for receiving messages
async fn run_poll_loop(
    http_client: &HttpClient,
    config: &WeChatConfig,
    context_tokens: &Arc<DashMap<String, String>>,
    typing_tickets: &Arc<DashMap<String, TypingTicketEntry>>,
    orchestrator: &Arc<crate::orchestrator::Orchestrator>,
    message_bus: &Arc<crate::message_bus::MessageBus>,
    channel_name: &str,
    mention_only: bool,
) -> anyhow::Result<()> {
    let mut sync_buf: Option<String> = None;

    info!("Starting WeChat iLink poll loop");

    loop {
        let url = format!("{}/ilink/bot/getupdates", config.base_url);
        let request_body = GetUpdatesRequest {
            get_updates_buf: sync_buf.clone(),
        };

        debug!(sync_buf = ?sync_buf, "Polling for updates");

        let response = http_client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("AuthorizationType", "ilink_bot_token")
            .header("Authorization", format!("Bearer {}", config.bot_token))
            .header("X-WECHAT-UIN", generate_uin_header())
            .json(&request_body)
            .timeout(POLL_TIMEOUT)
            .send()
            .await;

        match response {
            Ok(resp) => {
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    error!(status = %status, body = %text, "GetUpdates request failed");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }

                // Try to parse as JSON, but capture body text on failure for diagnostics
                let body_text = match resp.text().await {
                    Ok(text) => text,
                    Err(e) => {
                        error!(error = %e, "Failed to read GetUpdates response body");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                };

                let body: GetUpdatesResponse = match serde_json::from_str(&body_text) {
                    Ok(b) => b,
                    Err(e) => {
                        // Log first 200 chars of body to help diagnose protocol changes
                        let preview = if body_text.len() > 200 {
                            format!("{}...", &body_text[..200])
                        } else {
                            body_text.clone()
                        };
                        error!(error = %e, body_preview = %preview, "Failed to parse GetUpdates response");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                };

                // Check for API errors
                let is_api_error = matches!(body.ret, Some(r) if r != 0)
                    || matches!(body.errcode, Some(e) if e != 0);
                
                if is_api_error {
                    error!(
                        ret = ?body.ret,
                        errcode = ?body.errcode,
                        errmsg = ?body.errmsg,
                        "WeChat API error in getUpdates"
                    );
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }

                // Update sync buffer for next poll
                if let Some(buf) = body.get_updates_buf {
                    sync_buf = Some(buf);
                }

                // Process messages
                for msg in body.msgs {
                    if let Err(e) = process_incoming_message(
                        &msg,
                        http_client,
                        config,
                        context_tokens,
                        typing_tickets,
                        orchestrator,
                        message_bus,
                        channel_name,
                        mention_only,
                    )
                    .await
                    {
                        error!(error = %e, "Failed to process incoming message");
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "Poll request failed");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

/// Process a single incoming WeChat message
async fn process_incoming_message(
    msg: &WeixinMessage,
    http_client: &HttpClient,
    config: &WeChatConfig,
    context_tokens: &Arc<DashMap<String, String>>,
    typing_tickets: &Arc<DashMap<String, TypingTicketEntry>>,
    orchestrator: &Arc<crate::orchestrator::Orchestrator>,
    message_bus: &Arc<crate::message_bus::MessageBus>,
    channel_name: &str,
    mention_only: bool,
) -> anyhow::Result<()> {
    // Only process user messages
    if msg.message_type != MSG_TYPE_USER {
        return Ok(());
    }

    // Extract user ID and store context token
    let user_id = msg.from_user_id.clone();
    if !msg.context_token.is_empty() {
        context_tokens.insert(user_id.clone(), msg.context_token.clone());
    }

    let chat_id = ChatId(user_id.clone());

    // WeChat personal chat is always 1-on-1, so is_group_chat is always false
    let is_group_chat = false;
    let is_mentioned = false;

    if !should_dispatch_message(mention_only, is_group_chat, is_mentioned) {
        return Ok(());
    }

    // Extract text content from message items
    let content = extract_message_content(msg);
    let incoming = parse_incoming(&content);

    // Check for /bot command
    if let IncomingContent::Command {
        name: cmd_name,
        args,
    } = &incoming.content
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
            // Send typing indicator to show bot is processing
            let chat_id_str = chat_id.0.clone();
            let context_token = context_tokens.get(&chat_id_str).map(|t| t.clone());
            
            if let Err(e) = send_typing_with_ticket(
                http_client,
                config,
                typing_tickets,
                &chat_id_str,
                context_token.as_deref(),
            ).await {
                debug!(error = %e, chat_id = %chat_id_str, "Failed to send typing indicator");
            }
            
            let key = BotInstanceKey {
                channel_name: channel_name.to_string(),
                chat_id: chat_id.clone(),
                bot_name,
            };
            message_bus.dispatch(&key, incoming).await?;
        }
        None => {
            error!("Failed to get or create bot for chat");
        }
    }

    Ok(())
}

/// Extract text content from WeChat message
fn extract_message_content(msg: &WeixinMessage) -> String {
    let mut parts: Vec<String> = Vec::new();

    for item in &msg.item_list {
        if item.item_type == MSG_ITEM_TEXT {
            if let Some(ref ti) = item.text_item {
                let t = ti.text.trim();
                if !t.is_empty() {
                    parts.push(t.to_string());
                }
            }
        } else if item.item_type == MSG_ITEM_VOICE {
            if let Some(ref vi) = item.voice_item {
                let t = vi.text.trim();
                if !t.is_empty() {
                    parts.push(t.to_string());
                }
            }
        }
        // Include referenced message title if present
        if let Some(ref rm) = item.ref_msg {
            let t = rm.title.trim();
            if !t.is_empty() {
                parts.push(format!("> {}", t));
            }
        }
    }

    parts.join("\n")
}

/// Parse incoming text into message or command
fn parse_incoming(text: &str) -> IncomingMessage {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix('/') {
        let parts: Vec<&str> = rest.splitn(2, ' ').collect();
        let name = parts[0].to_lowercase();
        let args = parts.get(1).map(|s| s.to_string());
        IncomingMessage::command(name, args)
    } else {
        IncomingMessage::text(text.to_string())
    }
}

/// Handle outgoing messages
async fn handle_outgoing_messages(
    mut outgoing_rx: mpsc::Receiver<(ChatId, OutgoingMessage)>,
    http_client: &HttpClient,
    config: &WeChatConfig,
    context_tokens: &Arc<DashMap<String, String>>,
) {
    let mut stream_buffers: HashMap<String, (String, Instant)> = HashMap::new();

    // Timer for checking stream timeouts
    let mut timeout_checker = interval(Duration::from_secs(5));
    timeout_checker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            Some((chat_id, msg)) = outgoing_rx.recv() => {
                let chat_id_str = chat_id.0.clone();

                match msg.content {
                    OutgoingContent::Text(text) => {
                        if let Err(e) = send_text_message(
                            http_client,
                            config,
                            context_tokens,
                            &chat_id,
                            &text,
                        ).await {
                            error!(error = %e, chat_id = %chat_id_str, "Failed to send message");
                        }
                    }
                    OutgoingContent::StreamChunk(text) => {
                        // Buffer mode: accumulate chunks
                        let now = Instant::now();
                        stream_buffers
                            .entry(chat_id_str)
                            .and_modify(|(buf, ts)| {
                                buf.push_str(&text);
                                *ts = now;
                            })
                            .or_insert((text, now));
                    }
                    OutgoingContent::StreamEnd => {
                        // Stream end: send buffered content
                        if let Some((content, _)) = stream_buffers.remove(&chat_id_str)
                            && !content.is_empty()
                        {
                            if let Err(e) = send_text_message(
                                http_client,
                                config,
                                context_tokens,
                                &chat_id,
                                &content,
                            ).await {
                                error!(error = %e, chat_id = %chat_id_str, "Failed to send stream message");
                            }
                        }
                    }
                    OutgoingContent::Error(err) => {
                        let content = format!("Error: {}", err);
                        if let Err(e) = send_text_message(
                            http_client,
                            config,
                            context_tokens,
                            &chat_id,
                            &content,
                        ).await {
                            error!(error = %e, chat_id = %chat_id_str, "Failed to send error");
                        }
                    }
                    OutgoingContent::Mention { target_bot, message, .. } => {
                        // WeChat doesn't have native bot mention format
                        let full_message = format!("@{} {}", target_bot, message);
                        if let Err(e) = send_text_message(
                            http_client,
                            config,
                            context_tokens,
                            &chat_id,
                            &full_message,
                        ).await {
                            error!(error = %e, chat_id = %chat_id_str, "Failed to send mention");
                        }
                    }
                }
            }
            _ = timeout_checker.tick() => {
                check_and_flush_timeouts(
                    http_client,
                    config,
                    context_tokens,
                    &mut stream_buffers,
                ).await;
            }
            else => break,
        }
    }
}

/// Generate a unique client_id for message deduplication
fn generate_client_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let counter = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("acpconnector-{}-{}", timestamp, counter)
}

/// Send a text message to WeChat
async fn send_text_message(
    http_client: &HttpClient,
    config: &WeChatConfig,
    context_tokens: &Arc<DashMap<String, String>>,
    chat_id: &ChatId,
    content: &str,
) -> anyhow::Result<()> {
    let url = format!("{}/ilink/bot/sendmessage", config.base_url);

    // Get context token for this user (required)
    let context_token = match context_tokens.get(&chat_id.0) {
        Some(t) if !t.is_empty() => t.clone(),
        _ => {
            return Err(anyhow::anyhow!(
                "context_token is required for sendMessage but missing for user {}",
                chat_id.0
            ));
        }
    };

    // Truncate if too long
    let content = if content.chars().count() > MAX_MESSAGE_LENGTH {
        let truncated: String = content.chars().take(MAX_MESSAGE_LENGTH - 3).collect();
        format!("{}...", truncated)
    } else {
        content.to_string()
    };

    // Build message items
    let item = MessageItem {
        item_type: MSG_ITEM_TEXT,
        text_item: Some(TextItem { text: content }),
        voice_item: None,
        ref_msg: None,
    };

    let request = SendMessageRequest {
        msg: MessagePayload {
            from_user_id: String::new(),
            to_user_id: chat_id.0.clone(),
            client_id: generate_client_id(),
            message_type: MSG_TYPE_BOT,
            message_state: MSG_STATE_FINISH,
            context_token,
            item_list: vec![item],
        },
        base_info: BaseInfo {
            channel_version: CHANNEL_VERSION.to_string(),
        },
    };

    let response = http_client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("AuthorizationType", "ilink_bot_token")
        .header("Authorization", format!("Bearer {}", config.bot_token))
        .header("X-WECHAT-UIN", generate_uin_header())
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Send message failed: {} - {}", status, text));
    }

    debug!(chat_id = %chat_id.0, "Message sent successfully");
    Ok(())
}

/// Check and flush timed-out stream buffers
async fn check_and_flush_timeouts(
    http_client: &HttpClient,
    config: &WeChatConfig,
    context_tokens: &Arc<DashMap<String, String>>,
    stream_buffers: &mut HashMap<String, (String, Instant)>,
) {
    let now = Instant::now();

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
            if let Err(e) = send_text_message(
                http_client,
                config,
                context_tokens,
                &chat_id,
                &content,
            ).await {
                error!(error = %e, chat_id = %chat_id_str, "Failed to send timeout stream message");
            }
        }
    }
}



/// Fetch typing ticket from getConfig API
async fn fetch_typing_ticket(
    http_client: &HttpClient,
    config: &WeChatConfig,
    user_id: &str,
    context_token: &str,
) -> anyhow::Result<String> {
    let url = format!("{}/ilink/bot/getconfig", config.base_url);
    
    let request = serde_json::json!({
        "ilink_user_id": user_id,
        "context_token": context_token,
        "base_info": { "channel_version": CHANNEL_VERSION }
    });

    let response = http_client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("AuthorizationType", "ilink_bot_token")
        .header("Authorization", format!("Bearer {}", config.bot_token))
        .header("X-WECHAT-UIN", generate_uin_header())
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Get config failed: {} - {}", status, text));
    }

    let body: GetConfigResponse = response.json().await?;
    
    if let Some(ticket) = body.typing_ticket {
        if !ticket.is_empty() {
            return Ok(ticket);
        }
    }
    
    Err(anyhow::anyhow!("No typing_ticket in getConfig response"))
}

/// Send typing indicator to WeChat with ticket caching
async fn send_typing_with_ticket(
    http_client: &HttpClient,
    config: &WeChatConfig,
    typing_tickets: &Arc<DashMap<String, TypingTicketEntry>>,
    to_user_id: &str,
    context_token: Option<&str>,
) -> anyhow::Result<()> {
    // Check cache first
    if let Some(entry) = typing_tickets.get(to_user_id) {
        if entry.expiry > Instant::now() {
            // Use cached ticket
            return send_typing(http_client, config, to_user_id, &entry.ticket).await;
        }
    }

    // Fetch new ticket
    let context_token = match context_token {
        Some(t) if !t.is_empty() => t,
        _ => return Err(anyhow::anyhow!("context_token required for typing indicator")),
    };

    match fetch_typing_ticket(http_client, config, to_user_id, context_token).await {
        Ok(ticket) => {
            // Cache the ticket
            typing_tickets.insert(
                to_user_id.to_string(),
                TypingTicketEntry {
                    ticket: ticket.clone(),
                    expiry: Instant::now() + TYPING_TICKET_TTL,
                },
            );
            // Send typing indicator
            send_typing(http_client, config, to_user_id, &ticket).await
        }
        Err(e) => Err(e),
    }
}

/// Send typing indicator to WeChat
async fn send_typing(
    http_client: &HttpClient,
    config: &WeChatConfig,
    to_user_id: &str,
    typing_ticket: &str,
) -> anyhow::Result<()> {
    let url = format!("{}/ilink/bot/sendtyping", config.base_url);

    let request = serde_json::json!({
        "ilink_user_id": to_user_id,
        "typing_ticket": typing_ticket,
        "status": 1,
        "base_info": { "channel_version": CHANNEL_VERSION }
    });

    let response = http_client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("AuthorizationType", "ilink_bot_token")
        .header("Authorization", format!("Bearer {}", config.bot_token))
        .header("X-WECHAT-UIN", generate_uin_header())
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Send typing failed: {} - {}", status, text));
    }

    debug!(user_id = %to_user_id, "Typing indicator sent");
    Ok(())
}

// ============================================================================
// API Types
// ============================================================================

/// Request body for getUpdates
#[derive(Debug, Serialize)]
struct GetUpdatesRequest {
    #[serde(rename = "get_updates_buf", skip_serializing_if = "Option::is_none")]
    get_updates_buf: Option<String>,
}

/// Response body for getConfig
#[derive(Debug, Deserialize)]
struct GetConfigResponse {
    #[allow(dead_code)]
    ret: Option<i64>,
    #[allow(dead_code)]
    errmsg: Option<String>,
    #[serde(default)]
    typing_ticket: Option<String>,
}

/// Response body for getUpdates
#[derive(Debug, Deserialize)]
struct GetUpdatesResponse {
    ret: Option<i64>,
    errcode: Option<i64>,
    #[allow(dead_code)]
    errmsg: Option<String>,
    #[serde(default)]
    msgs: Vec<WeixinMessage>,
    #[serde(rename = "get_updates_buf")]
    get_updates_buf: Option<String>,
    #[allow(dead_code)]
    #[serde(rename = "longpolling_timeout_ms")]
    longpolling_timeout_ms: Option<u64>,
}

/// WeChat message structure
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct WeixinMessage {
    #[serde(default)]
    from_user_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    to_user_id: String,
    #[serde(default)]
    message_type: i64,
    #[allow(dead_code)]
    #[serde(default)]
    message_state: i64,
    #[serde(default)]
    context_token: String,
    #[serde(default)]
    item_list: Vec<MessageItem>,
}

/// Message item within a WeChat message
#[derive(Debug, Deserialize, Serialize)]
struct MessageItem {
    #[serde(default, rename = "type")]
    item_type: i64,
    #[serde(default)]
    text_item: Option<TextItem>,
    #[serde(default)]
    voice_item: Option<VoiceItem>,
    #[serde(default)]
    ref_msg: Option<RefMsg>,
}

#[derive(Debug, Deserialize, Serialize)]
struct TextItem {
    #[serde(default)]
    text: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct VoiceItem {
    #[serde(default)]
    text: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct RefMsg {
    #[serde(default)]
    title: String,
}

/// Request body for sendMessage (wrapped in "msg" field)
#[derive(Debug, Serialize)]
struct SendMessageRequest {
    msg: MessagePayload,
    #[serde(rename = "base_info")]
    base_info: BaseInfo,
}

#[derive(Debug, Serialize)]
struct MessagePayload {
    #[serde(rename = "from_user_id")]
    from_user_id: String,
    #[serde(rename = "to_user_id")]
    to_user_id: String,
    #[serde(rename = "client_id")]
    client_id: String,
    #[serde(rename = "message_type")]
    message_type: i64,
    #[serde(rename = "message_state")]
    message_state: i64,
    #[serde(rename = "context_token")]
    context_token: String,
    #[serde(rename = "item_list")]
    item_list: Vec<MessageItem>,
}

#[derive(Debug, Serialize)]
struct BaseInfo {
    #[serde(rename = "channel_version")]
    channel_version: String,
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_incoming_text() {
        let incoming = parse_incoming("hello");
        match incoming.content {
            IncomingContent::Text(text) => assert_eq!(text, "hello"),
            _ => panic!("Expected text"),
        }
    }

    #[test]
    fn test_parse_incoming_command() {
        let incoming = parse_incoming("/model gpt-4");
        match incoming.content {
            IncomingContent::Command { name, args } => {
                assert_eq!(name, "model");
                assert_eq!(args, Some("gpt-4".to_string()));
            }
            _ => panic!("Expected command"),
        }
    }

    #[test]
    fn test_extract_message_content_text() {
        let msg = WeixinMessage {
            from_user_id: "user123".to_string(),
            to_user_id: "bot456".to_string(),
            message_type: MSG_TYPE_USER,
            message_state: 0,
            context_token: "token".to_string(),
            item_list: vec![MessageItem {
                item_type: MSG_ITEM_TEXT,
                text_item: Some(TextItem { text: "Hello world".to_string() }),
                voice_item: None,
                ref_msg: None,
            }],
        };
        assert_eq!(extract_message_content(&msg), "Hello world");
    }

    #[test]
    fn test_extract_message_content_with_ref() {
        let msg = WeixinMessage {
            from_user_id: "user123".to_string(),
            to_user_id: "bot456".to_string(),
            message_type: MSG_TYPE_USER,
            message_state: 0,
            context_token: "token".to_string(),
            item_list: vec![MessageItem {
                item_type: MSG_ITEM_TEXT,
                text_item: Some(TextItem { text: "my reply".to_string() }),
                voice_item: None,
                ref_msg: Some(RefMsg { title: "original message".to_string() }),
            }],
        };
        let text = extract_message_content(&msg);
        assert!(text.contains("my reply"));
        assert!(text.contains("> original message"));
    }

    #[test]
    fn test_extract_message_content_voice() {
        let msg = WeixinMessage {
            from_user_id: "user123".to_string(),
            to_user_id: "bot456".to_string(),
            message_type: MSG_TYPE_USER,
            message_state: 0,
            context_token: "token".to_string(),
            item_list: vec![MessageItem {
                item_type: MSG_ITEM_VOICE,
                text_item: None,
                voice_item: Some(VoiceItem { text: "voice transcription".to_string() }),
                ref_msg: None,
            }],
        };
        assert_eq!(extract_message_content(&msg), "voice transcription");
    }

    #[test]
    fn test_base64_encode() {
        use base64::{Engine, engine::general_purpose::STANDARD};
        assert_eq!(STANDARD.encode(b"hello"), "aGVsbG8=");
        assert_eq!(STANDARD.encode(b"hello world"), "aGVsbG8gd29ybGQ=");
        // 3 zero bytes -> 4 base64 chars
        assert_eq!(STANDARD.encode(&[0u8, 0u8, 0u8]), "AAAA");
    }

    #[test]
    fn test_wechat_config_default_url() {
        let config = WeChatConfig::new("token123".to_string(), None);
        assert_eq!(config.bot_token, "token123");
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
    }

    #[test]
    fn test_wechat_config_custom_url() {
        let config = WeChatConfig::new(
            "token123".to_string(),
            Some("https://custom.example.com".to_string()),
        );
        assert_eq!(config.base_url, "https://custom.example.com");
    }
}
