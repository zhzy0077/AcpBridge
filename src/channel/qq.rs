//! QQ Bot channel implementation
//!
//! Implements WebSocket-based connection to QQ Bot Open Platform.
//! Supports C2C (single chat) and Group (@bot) message scenarios.

use crate::channel::{
    Channel, ChatId, IncomingContent, IncomingMessage, OutgoingContent, OutgoingMessage,
};
use crate::message_bus::BotInstanceKey;
use anyhow::{Context, Result, anyhow};
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{RwLock, mpsc};
use tokio::time::{MissedTickBehavior, interval, sleep};
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tracing::{debug, error, info, warn};

/// Static registry: QQ bot_id → channel_name.
/// Populated at startup when each QqChannel connects and fetches its own bot_id.
static QQ_BOT_REGISTRY: LazyLock<DashMap<String, String>> = LazyLock::new(DashMap::new);

/// QQ Bot channel
#[derive(Debug)]
pub struct QqChannel {
    /// Shared across start() and search_bots_in_chat()
    http_client: HttpClient,
    token_manager: Arc<TokenManager>,
    bot_id_cache: Arc<tokio::sync::OnceCell<Option<String>>>,
    /// Cache: group_openid (without "group:" prefix) → Vec<bot_ids of bot members>
    chat_members_cache: DashMap<String, Vec<String>>,
}

impl QqChannel {
    pub fn new(app_id: String, client_secret: String, _mention_only: bool) -> Self {
        let http_client = HttpClient::new();
        let token_manager = Arc::new(TokenManager::new(
            app_id,
            client_secret,
            http_client.clone(),
        ));
        Self {
            http_client,
            token_manager,
            bot_id_cache: Arc::new(tokio::sync::OnceCell::new()),
            chat_members_cache: DashMap::new(),
        }
    }
}

#[async_trait::async_trait]
impl Channel for QqChannel {
    async fn start(
        &self,
        channel_name: String,
        orchestrator: Arc<crate::orchestrator::Orchestrator>,
        message_bus: Arc<crate::message_bus::MessageBus>,
    ) -> Result<()> {
        // Eagerly register own bot_id so peers can discover us via QQ_BOT_REGISTRY
        let bot_id = self
            .bot_id_cache
            .get_or_init(|| async {
                let token = match self.token_manager.get_token().await {
                    Ok(t) => t,
                    Err(_) => return None,
                };
                #[derive(Deserialize)]
                struct Me {
                    id: String,
                }
                let resp = match self
                    .http_client
                    .get("https://api.sgroup.qq.com/users/@me")
                    .header("Authorization", format!("QQBot {}", token))
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => r,
                    _ => return None,
                };
                resp.json::<Me>().await.ok().map(|m| m.id)
            })
            .await
            .clone();

        if let Some(ref id) = bot_id {
            QQ_BOT_REGISTRY.insert(id.clone(), channel_name.clone());
            info!(channel = %channel_name, "Registered QQ bot in registry");
        } else {
            warn!(channel = %channel_name, "Could not fetch QQ bot id; search_bots will be unavailable");
        }

        let manager = Arc::new(QqBotManager::new(
            channel_name,
            orchestrator,
            message_bus,
            self.http_client.clone(),
            self.token_manager.clone(),
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
            None => return vec![],
        };

        let own_bot_id = self
            .bot_id_cache
            .get()
            .and_then(|o| o.as_deref())
            .unwrap_or("")
            .to_string();

        let member_ids = if let Some(cached) = self.chat_members_cache.get(raw_chat_id) {
            cached.clone()
        } else {
            match fetch_group_member_bot_ids(&self.http_client, &self.token_manager, raw_chat_id)
                .await
            {
                Ok(ids) => {
                    self.chat_members_cache
                        .insert(raw_chat_id.to_string(), ids.clone());
                    ids
                }
                Err(e) => {
                    warn!(error = %e, chat_id = %chat_id.0, "Failed to fetch QQ group members");
                    return vec![];
                }
            }
        };

        member_ids
            .iter()
            .filter(|id| id.as_str() != own_bot_id)
            .filter_map(|id| QQ_BOT_REGISTRY.get(id).map(|e| e.value().clone()))
            .collect()
    }
}

/// QQ Bot connection manager
struct QqBotManager {
    http_client: HttpClient,
    token_manager: Arc<TokenManager>,
    channel_name: String,
    orchestrator: Arc<crate::orchestrator::Orchestrator>,
    message_bus: Arc<crate::message_bus::MessageBus>,
    /// Recent message IDs for passive reply (chat_id -> msg_id)
    recent_msg_ids: Arc<RwLock<HashMap<String, String>>>,
}

impl QqBotManager {
    fn new(
        channel_name: String,
        orchestrator: Arc<crate::orchestrator::Orchestrator>,
        message_bus: Arc<crate::message_bus::MessageBus>,
        http_client: HttpClient,
        token_manager: Arc<TokenManager>,
    ) -> Self {
        Self {
            http_client,
            token_manager,
            channel_name,
            orchestrator,
            message_bus,
            recent_msg_ids: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn run(&self, mut outgoing_rx: mpsc::Receiver<(ChatId, OutgoingMessage)>) -> Result<()> {
        let token_manager = self.token_manager.clone();
        let channel_name = self.channel_name.clone();
        let orchestrator = self.orchestrator.clone();
        let message_bus = self.message_bus.clone();
        let recent_msg_ids = self.recent_msg_ids.clone();
        let session_state = Arc::new(SessionState::new());

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
                    &recent_msg_ids,
                    &session_state,
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
        });

        // Handle outgoing messages
        let recent_msg_ids = self.recent_msg_ids.clone();
        let token_manager = self.token_manager.clone();
        let http_client = self.http_client.clone();

        let outgoing_handle = tokio::spawn(async move {
            // 本地状态管理，无需 Arc<RwLock>
            let mut stream_buffers: HashMap<String, (String, std::time::Instant)> = HashMap::new();

            // 定时器用于检查超时（30秒）
            let mut timeout_checker = interval(Duration::from_secs(5));
            timeout_checker.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    // 处理新的 outgoing 消息
                    Some((chat_id, msg)) = outgoing_rx.recv() => {
                        let chat_id_str = chat_id.0.clone();

                        match msg.content {
                            OutgoingContent::Text(text) => {
                                // 非流式消息直接发送
                                if let Err(e) = send_message(
                                    &http_client,
                                    &token_manager,
                                    &chat_id,
                                    &text,
                                    &recent_msg_ids,
                                ).await {
                                    error!(error = %e, chat_id = %chat_id_str, "Failed to send message");
                                }
                            }
                            OutgoingContent::StreamChunk(text) => {
                                // 缓冲模式：累积到缓冲区，更新时间戳
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
                                // 流式结束：发送缓冲的内容，并从 HashMap 中移除（避免内存泄漏）
                                if let Some((content, _)) = stream_buffers.remove(&chat_id_str)
                                    && !content.is_empty()
                                    && let Err(e) = send_message(
                                        &http_client,
                                        &token_manager,
                                        &chat_id,
                                        &content,
                                        &recent_msg_ids,
                                    )
                                    .await
                                {
                                    error!(error = %e, chat_id = %chat_id_str, "Failed to send stream message");
                                }
                            }
                            OutgoingContent::Error(err) => {
                                let content = format!("Error: {}", err);
                                if let Err(e) = send_message(
                                    &http_client,
                                    &token_manager,
                                    &chat_id,
                                    &content,
                                    &recent_msg_ids,
                                ).await {
                                    error!(error = %e, chat_id = %chat_id_str, "Failed to send error");
                                }
                            }
                            OutgoingContent::Mention { target_bot, target_channel_name, message } => {
                                // Look up target's bot_id via the static registry
                                let bot_id = target_channel_name.as_deref().and_then(|cn| {
                                    QQ_BOT_REGISTRY
                                        .iter()
                                        .find(|e| e.value() == cn)
                                        .map(|e| e.key().clone())
                                });
                                let full_message = match bot_id {
                                    Some(id) => format!("<@!{}> {}", id, message),
                                    None => {
                                        warn!(target_bot = %target_bot, "Target bot id not in QQ registry, falling back to plain mention");
                                        format!("@{} {}", target_bot, message)
                                    }
                                };
                                if let Err(e) = send_message(
                                    &http_client,
                                    &token_manager,
                                    &chat_id,
                                    &full_message,
                                    &recent_msg_ids,
                                ).await {
                                    error!(error = %e, chat_id = %chat_id_str, "Failed to send mention");
                                }
                            }
                        }
                    }
                    // 定期检查超时（30秒）
                    _ = timeout_checker.tick() => {
                        check_and_flush_timeouts(
                            &http_client,
                            &token_manager,
                            &mut stream_buffers,
                            &recent_msg_ids,
                        ).await;
                    }
                    // 通道关闭时退出
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
    client_secret: String,
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
    channel_name: &str,
    orchestrator: &Arc<crate::orchestrator::Orchestrator>,
    message_bus: &Arc<crate::message_bus::MessageBus>,
    recent_msg_ids: &Arc<RwLock<HashMap<String, String>>>,
    session_state: &Arc<SessionState>,
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
                    "$browser": "acpconnector",
                    "$device": "acpconnector"
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
            // Resume failed or other error, clear session state
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
    let mut heartbeat_timer = interval(heartbeat_interval);
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
                                        channel_name,
                                        orchestrator,
                                        message_bus,
                                        recent_msg_ids,
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

/// Fetch the bot_ids of all bot members in a QQ group.
async fn fetch_group_member_bot_ids(
    http_client: &HttpClient,
    token_manager: &TokenManager,
    group_openid: &str,
) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct GroupMember {
        member_openid: String,
        is_bot: Option<bool>,
    }
    #[derive(Deserialize)]
    struct Pagination {
        has_more: bool,
        next_token: Option<String>,
    }
    #[derive(Deserialize)]
    struct GroupMembersResp {
        members: Vec<GroupMember>,
        pagination: Pagination,
    }

    let url = format!(
        "https://api.sgroup.qq.com/v2/groups/{}/members",
        group_openid
    );
    let mut all_bot_ids = Vec::new();
    let mut next_token: Option<String> = None;

    loop {
        let token = token_manager.get_token().await?;
        let mut request = http_client
            .get(&url)
            .header("Authorization", format!("QQBot {}", token));
        if let Some(ref nt) = next_token {
            request = request.query(&[("next_token", nt.as_str())]);
        }

        let response = request
            .send()
            .await
            .context("Failed to send group members request")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("Group members request failed: {} - {}", status, text));
        }

        let resp: GroupMembersResp = response
            .json()
            .await
            .context("Failed to parse group members response")?;

        for member in resp.members {
            if member.is_bot.unwrap_or(false) {
                all_bot_ids.push(member.member_openid);
            }
        }

        if !resp.pagination.has_more {
            break;
        }
        next_token = resp.pagination.next_token;
    }

    Ok(all_bot_ids)
}

/// Parse message content and extract command or text
/// Handles @mention prefix removal for group messages
fn parse_message_content(content: &str) -> IncomingContent {
    // Remove @mention patterns like <@!123456> or leading @bot
    let content = content.trim();

    // Find the first occurrence of '/' that indicates a command
    // This handles cases like "@bot /help" or "<@!123> /help"
    if let Some(cmd_pos) = content.find('/') {
        let before_cmd = &content[..cmd_pos];
        let after_cmd = &content[cmd_pos + 1..];

        // Check if before_cmd only contains whitespace and @mentions
        let is_only_mentions = before_cmd.chars().all(|c| {
            c.is_whitespace() || c == '@' || c == '<' || c == '>' || c == '!' || c.is_ascii_digit()
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

async fn handle_event(
    payload: &GatewayPayload,
    channel_name: &str,
    orchestrator: &Arc<crate::orchestrator::Orchestrator>,
    message_bus: &Arc<crate::message_bus::MessageBus>,
    recent_msg_ids: &Arc<RwLock<HashMap<String, String>>>,
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

            let content = parse_message_content(&msg.content);

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
                        .await?;
                }
                None => {
                    error!("Failed to get or create bot for chat");
                }
            }
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

            let content = parse_message_content(&msg.content);

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
                        .await?;
                }
                None => {
                    error!("Failed to get or create bot for chat");
                }
            }
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
    // Use char count to avoid panic on multi-byte characters
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

use std::sync::atomic::{AtomicU64, Ordering};

static MSG_SEQ: AtomicU64 = AtomicU64::new(1);

fn generate_msg_seq() -> u64 {
    MSG_SEQ.fetch_add(1, Ordering::SeqCst)
}

/// 检查并 flush 超时的流式缓冲区（30秒超时）
async fn check_and_flush_timeouts(
    http_client: &reqwest::Client,
    token_manager: &crate::channel::qq::TokenManager,
    stream_buffers: &mut HashMap<String, (String, std::time::Instant)>,
    recent_msg_ids: &std::sync::Arc<tokio::sync::RwLock<HashMap<String, String>>>,
) {
    use tracing::warn;

    const STREAM_TIMEOUT: Duration = Duration::from_secs(30);
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
            if let Err(e) = send_message(
                http_client,
                token_manager,
                &chat_id,
                &content,
                recent_msg_ids,
            )
            .await
            {
                error!(error = %e, chat_id = %chat_id_str, "Failed to send timeout stream message");
            }
        }
    }
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
}
