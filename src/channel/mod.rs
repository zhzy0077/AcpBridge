//! Channel abstraction layer
//!
//! This module defines the `ChannelAdapter` trait that abstracts over different
//! messaging platforms (Telegram, QQ, Lark/Feishu, etc.). Each platform implements
//! this trait to handle platform-specific I/O.
//!
//! The `ChannelOrchestrator` handles shared logic: incoming dispatch, outgoing loop,
//! stream buffering, and timeout management.

mod discord;
mod lark;
mod qq;
mod telegram;
mod wechat;

pub use discord::DiscordAdapter;
pub use lark::LarkAdapter;
pub use qq::QqAdapter;
pub use telegram::TelegramAdapter;
pub use wechat::{WeChatAdapter, WeChatConfig};

use crate::message_bus::BotInstanceKey;
use crate::orchestrator::Orchestrator;
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{error, warn};

/// Unique identifier for a chat/dialog (platform-specific)
///
/// Warning: The internal format is platform-specific and private to each channel
/// implementation. External code should not depend on or parse the inner String.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChatId(pub String);

/// Opaque handle to a sent message, used for edit-in-place streaming.
/// Each platform stores its own ID type inside.
#[derive(Debug, Clone)]
pub struct MessageHandle(pub String);

/// Raw incoming message from the platform, before orchestration.
/// The adapter extracts platform-specific fields; the orchestrator
/// handles parse_incoming, should_dispatch, /bot, get_or_create, dispatch.
#[derive(Debug, Clone)]
pub struct RawIncoming {
    pub chat_id: ChatId,
    pub text: String,
    pub is_group: bool,
    pub is_mentioned: bool,
}

/// ChannelAdapter trait for messaging platform implementations
///
/// Implementors must provide pure platform I/O without orchestration logic.
/// The `ChannelOrchestrator` handles the rest.
#[async_trait]
pub trait ChannelAdapter: Send + Sync + 'static {
    /// Platform name for logging
    fn platform(&self) -> &'static str;

    /// Max chars per message on this platform
    fn max_message_length(&self) -> usize;

    /// Whether this platform supports editing a sent message for streaming.
    /// If true, the orchestrator uses edit-in-place streaming.
    /// If false, it buffers and sends once on StreamEnd.
    fn supports_edit(&self) -> bool {
        false
    }

    /// Start receiving messages. Send each incoming user message into `incoming_tx`.
    /// This is the platform's main loop (polling, WebSocket, long-poll, etc.).
    /// The adapter owns its own connection lifecycle and reconnection.
    async fn run_incoming(
        &self,
        incoming_tx: mpsc::Sender<RawIncoming>,
    ) -> anyhow::Result<()>;

    /// Send a single text message
    async fn send_text(&self, chat_id: &ChatId, text: &str) -> Result<()>;

    /// Send a message and return a handle for future edits.
    /// Only called when supports_edit() returns true.
    async fn send_and_track(&self, chat_id: &ChatId, text: &str) -> Result<MessageHandle> {
        self.send_text(chat_id, text).await?;
        Err(anyhow::anyhow!("tracking not supported"))
    }

    /// Edit a previously sent message.
    /// Only called when supports_edit() returns true.
    async fn edit_message(
        &self,
        chat_id: &ChatId,
        handle: &MessageHandle,
        text: &str,
    ) -> Result<()> {
        let _ = (chat_id, handle, text);
        Err(anyhow::anyhow!("edit not supported"))
    }

    /// Format a mention in this platform's native syntax.
    /// Default: plain text fallback.
    fn format_mention(
        &self,
        target_bot: &str,
        _target_channel: Option<&str>,
        message: &str,
    ) -> String {
        format!("@{} {}", target_bot, message)
    }
}

/// ChannelOrchestrator manages the shared logic for all channels:
/// - Incoming dispatch pipeline
/// - Outgoing message loop with stream buffering
/// - Timeout management
pub struct ChannelOrchestrator {
    channel_name: String,
    mention_only: bool,
    orchestrator: Arc<Orchestrator>,
    message_bus: Arc<crate::message_bus::MessageBus>,
}

impl ChannelOrchestrator {
    /// Create a new ChannelOrchestrator
    pub fn new(
        channel_name: String,
        mention_only: bool,
        orchestrator: Arc<Orchestrator>,
        message_bus: Arc<crate::message_bus::MessageBus>,
    ) -> Self {
        Self {
            channel_name,
            mention_only,
            orchestrator,
            message_bus,
        }
    }

    /// Main entry point — replaces the old Channel::start().
    pub async fn run(self, adapter: Arc<dyn ChannelAdapter>) -> Result<()> {
        // 1. Create outgoing channel, register with MessageBus
        let (outgoing_tx, outgoing_rx) = mpsc::channel(100);
        self.message_bus
            .register_channel(self.channel_name.clone(), outgoing_tx)
            .await;

        // 2. Create incoming channel
        let (incoming_tx, incoming_rx) = mpsc::channel(100);

        // 3. Spawn adapter's incoming loop
        let incoming_handle = tokio::spawn({
            let adapter = adapter.clone();
            let tx = incoming_tx;
            async move { adapter.run_incoming(tx).await }
        });

        // Clone fields needed for dispatcher
        let channel_name = self.channel_name.clone();
        let mention_only = self.mention_only;
        let orchestrator = self.orchestrator.clone();
        let message_bus_in = self.message_bus.clone();
        let message_bus_out = self.message_bus.clone();

        // 4. Spawn shared incoming dispatcher
        let dispatch_handle =
            tokio::spawn(async move { run_incoming_dispatcher(
                channel_name, mention_only, orchestrator, message_bus_in, incoming_rx).await });

        // 5. Spawn shared outgoing loop
        let outgoing_handle =
            tokio::spawn(async move { run_outgoing_loop(
                message_bus_out, adapter, outgoing_rx).await });

        // 6. Wait for any task to finish
        tokio::select! {
            _ = incoming_handle => {},
            _ = dispatch_handle => {},
            _ = outgoing_handle => {},
        }
        Ok(())
    }
}

/// All the logic that used to be duplicated in every channel's incoming handler.
async fn run_incoming_dispatcher(
    channel_name: String,
    mention_only: bool,
    orchestrator: Arc<Orchestrator>,
    message_bus: Arc<crate::message_bus::MessageBus>,
    mut incoming_rx: mpsc::Receiver<RawIncoming>,
) -> Result<()> {
    while let Some(raw) = incoming_rx.recv().await {
        if !should_dispatch_message(mention_only, raw.is_group, raw.is_mentioned) {
            continue;
        }

        let incoming = parse_incoming(&raw.text);

        // /bot command → orchestrator
        if let IncomingContent::Command { name, args } = &incoming.content
            && name == "bot"
        {
            let _ = orchestrator
                .handle_bot_command(&channel_name, &raw.chat_id, args.clone())
                .await;
            continue;
        }

        // Normal message → get_or_create → dispatch
        if let Some(bot_name) = orchestrator
            .get_or_create(&channel_name, &raw.chat_id)
            .await
        {
            let key = BotInstanceKey {
                channel_name: channel_name.clone(),
                chat_id: raw.chat_id,
                bot_name,
            };
            if let Err(e) = message_bus.dispatch(&key, incoming).await {
                error!(error = %e, "Failed to dispatch message");
            }
        }
    }
    Ok(())
}

/// Outgoing loop with stream strategy (shared)
async fn run_outgoing_loop(
    _message_bus: Arc<crate::message_bus::MessageBus>,
    adapter: Arc<dyn ChannelAdapter>,
    mut outgoing_rx: mpsc::Receiver<(ChatId, OutgoingMessage)>,
) -> Result<()> {
    let mut streamer: Box<dyn StreamHandler> = if adapter.supports_edit() {
        Box::new(EditInPlaceStream::new(adapter.clone()))
    } else {
        Box::new(BufferAndSendStream::new(adapter.clone()))
    };

    let mut timeout_checker = interval(Duration::from_secs(5));
    timeout_checker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            Some((chat_id, msg)) = outgoing_rx.recv() => {
                match msg.content {
                    OutgoingContent::Text(text) => {
                        let _ = send_with_splitting(&*adapter, &chat_id, &text).await;
                    }
                    OutgoingContent::StreamChunk(text) => {
                        streamer.on_chunk(&chat_id, &text).await;
                    }
                    OutgoingContent::StreamEnd => {
                        streamer.on_end(&chat_id).await;
                    }
                    OutgoingContent::Error(err) => {
                        let _ = adapter.send_text(&chat_id, &format!("Error: {err}")).await;
                    }
                    OutgoingContent::Mention { target_bot, target_channel_name, message } => {
                        let text = adapter.format_mention(
                            &target_bot, target_channel_name.as_deref(), &message,
                        );
                        let _ = adapter.send_text(&chat_id, &text).await;
                    }
                }
            }
            _ = timeout_checker.tick() => {
                streamer.flush_expired().await;
            }
            else => break,
        }
    }
    Ok(())
}

/// Trait for stream handling strategies
#[async_trait]
trait StreamHandler: Send {
    async fn on_chunk(&mut self, chat_id: &ChatId, text: &str);
    async fn on_end(&mut self, chat_id: &ChatId);
    async fn flush_expired(&mut self);
}

/// Edit-in-place streaming for platforms that support message editing (Telegram, Discord)
struct EditInPlaceStream {
    adapter: Arc<dyn ChannelAdapter>,
    /// Map chat_id to stream state
    states: HashMap<String, EditInPlaceState>,
}

struct EditInPlaceState {
    handle: MessageHandle,
    buffer: String,
    last_update: Instant,
}

/// Min edit interval to avoid rate limiting
const MIN_EDIT_INTERVAL: Duration = Duration::from_millis(500);
/// Stream timeout (30 seconds without new chunk)
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);

impl EditInPlaceStream {
    fn new(adapter: Arc<dyn ChannelAdapter>) -> Self {
        Self {
            adapter,
            states: HashMap::new(),
        }
    }
}

#[async_trait]
impl StreamHandler for EditInPlaceStream {
    async fn on_chunk(&mut self, chat_id: &ChatId, text: &str) {
        let chat_id_str = chat_id.0.clone();
        let max_len = self.adapter.max_message_length();

        if let Some(state) = self.states.get_mut(&chat_id_str) {
            // Check if adding this chunk would exceed the limit
            let new_len = state.buffer.len() + text.len();
            if new_len > max_len {
                // Final edit of current message
                let _ = self
                    .adapter
                    .edit_message(chat_id, &state.handle, &state.buffer)
                    .await;
                // Start new message with the chunk
                let new_buffer = text.to_string();
                if let Ok(handle) = self.adapter.send_and_track(chat_id, &new_buffer).await {
                    state.handle = handle;
                    state.buffer = new_buffer;
                    state.last_update = Instant::now();
                }
            } else {
                // Normal append
                state.buffer.push_str(text);
                if state.last_update.elapsed() >= MIN_EDIT_INTERVAL {
                    let _ = self
                        .adapter
                        .edit_message(chat_id, &state.handle, &state.buffer)
                        .await;
                    state.last_update = Instant::now();
                }
            }
        } else {
            // First chunk: send new message
            let buffer = text.to_string();
            if let Ok(handle) = self.adapter.send_and_track(chat_id, &buffer).await {
                self.states.insert(
                    chat_id_str,
                    EditInPlaceState {
                        handle,
                        buffer,
                        last_update: Instant::now(),
                    },
                );
            }
        }
    }

    async fn on_end(&mut self, chat_id: &ChatId) {
        let chat_id_str = chat_id.0.clone();
        if let Some(state) = self.states.remove(&chat_id_str) {
            let _ = self
                .adapter
                .edit_message(chat_id, &state.handle, &state.buffer)
                .await;
        }
    }

    async fn flush_expired(&mut self) {
        let expired: Vec<String> = self
            .states
            .iter()
            .filter(|(_, s)| s.last_update.elapsed() > STREAM_TIMEOUT)
            .map(|(id, _)| id.clone())
            .collect();

        for chat_id_str in expired {
            warn!(chat_id = %chat_id_str, "Stream timeout, auto-flushing");
            let chat_id = ChatId(chat_id_str);
            self.on_end(&chat_id).await;
        }
    }
}

/// Buffer-and-send streaming for platforms without edit support (QQ, Lark, WeChat)
struct BufferAndSendStream {
    adapter: Arc<dyn ChannelAdapter>,
    /// Map chat_id to buffer with timestamp
    buffers: HashMap<String, BufferState>,
}

struct BufferState {
    buffer: String,
    last_update: Instant,
}

impl BufferAndSendStream {
    fn new(adapter: Arc<dyn ChannelAdapter>) -> Self {
        Self {
            adapter,
            buffers: HashMap::new(),
        }
    }
}

#[async_trait]
impl StreamHandler for BufferAndSendStream {
    async fn on_chunk(&mut self, chat_id: &ChatId, text: &str) {
        let chat_id_str = chat_id.0.clone();
        let max_len = self.adapter.max_message_length();

        if let Some(state) = self.buffers.get_mut(&chat_id_str) {
            // Check if adding this chunk would exceed the limit
            if state.buffer.len() + text.len() > max_len {
                // Send current buffer first
                let _ = send_with_splitting(&*self.adapter, chat_id, &state.buffer).await;
                state.buffer = text.to_string();
            } else {
                state.buffer.push_str(text);
            }
            state.last_update = Instant::now();
        } else {
            self.buffers.insert(
                chat_id_str,
                BufferState {
                    buffer: text.to_string(),
                    last_update: Instant::now(),
                },
            );
        }
    }

    async fn on_end(&mut self, chat_id: &ChatId) {
        let chat_id_str = chat_id.0.clone();
        if let Some(state) = self.buffers.remove(&chat_id_str) {
            let _ = send_with_splitting(&*self.adapter, chat_id, &state.buffer).await;
        }
    }

    async fn flush_expired(&mut self) {
        let expired: Vec<String> = self
            .buffers
            .iter()
            .filter(|(_, s)| s.last_update.elapsed() > STREAM_TIMEOUT)
            .map(|(id, _)| id.clone())
            .collect();

        for chat_id_str in expired {
            warn!(chat_id = %chat_id_str, "Stream timeout, auto-flushing");
            let chat_id = ChatId(chat_id_str);
            self.on_end(&chat_id).await;
        }
    }
}

/// Parse incoming text into an IncomingMessage
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

/// Check if a message should be dispatched based on mention_only settings
fn should_dispatch_message(
    mention_only: bool,
    is_group_chat: bool,
    is_mentioned: bool,
) -> bool {
    !mention_only || !is_group_chat || is_mentioned
}

/// Split and send a long message respecting platform limits.
async fn send_with_splitting(
    adapter: &dyn ChannelAdapter,
    chat_id: &ChatId,
    text: &str,
) -> Result<()> {
    let max = adapter.max_message_length();
    if text.chars().count() <= max {
        return adapter.send_text(chat_id, text).await;
    }
    for chunk in split_by_chars(text, max) {
        adapter.send_text(chat_id, &chunk).await?;
    }
    Ok(())
}

/// Split text into chunks of max_chars (respecting character boundaries)
fn split_by_chars(text: &str, max_chars: usize) -> impl Iterator<Item = String> + '_ {
    text.chars()
        .collect::<Vec<_>>()
        .chunks(max_chars)
        .map(|chunk| chunk.iter().collect())
        .collect::<Vec<_>>()
        .into_iter()
}

impl IncomingMessage {
    /// Create a text message
    pub fn text(text: String) -> Self {
        IncomingMessage {
            content: IncomingContent::Text(text),
        }
    }

    /// Create a command
    pub fn command(name: String, args: Option<String>) -> Self {
        IncomingMessage {
            content: IncomingContent::Command { name, args },
        }
    }
}

/// Messages coming from the channel (user input)
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    /// The actual message content
    pub content: IncomingContent,
}

/// Content of an incoming message
#[derive(Debug, Clone)]
pub enum IncomingContent {
    /// A plain text message from the user
    Text(String),

    /// A slash command (e.g., /new, /model, /cd)
    Command { name: String, args: Option<String> },
}

/// Messages going to the channel (output to user)
#[derive(Debug, Clone)]
pub struct OutgoingMessage {
    /// The message content
    pub content: OutgoingContent,
}

/// Content of an outgoing message
#[derive(Debug, Clone)]
pub enum OutgoingContent {
    /// A text message to send to the user (非流式，如 tool call 通知、命令回复)
    Text(String),
    /// 流式消息的一个片段，Channel 自行决定如何处理
    StreamChunk(String),
    /// 流式消息结束信号
    StreamEnd,
    /// An error message to send to the user
    Error(String),
    /// Mention another bot by name — each Channel formats this into its native @mention syntax.
    /// `target_channel_name` is the config channel name of the target bot; channels with a
    /// static bot registry use it to look up the target's platform ID.
    Mention {
        target_bot: String,
        target_channel_name: Option<String>,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_id_clone() {
        let chat_id = ChatId("12345".to_string());
        let cloned = chat_id.clone();
        assert_eq!(chat_id, cloned);
    }

    #[test]
    fn test_chat_id_debug() {
        let chat_id = ChatId("999".to_string());
        let debug_str = format!("{:?}", chat_id);
        assert!(debug_str.contains("999"));
    }

    #[test]
    fn test_incoming_message_text() {
        let msg = IncomingMessage::text("Hello".to_string());
        match msg.content {
            IncomingContent::Text(t) => assert_eq!(t, "Hello"),
            _ => panic!("Expected Text content"),
        }
    }

    #[test]
    fn test_incoming_message_command() {
        let msg = IncomingMessage::command("model".to_string(), Some("gpt-4".to_string()));
        match msg.content {
            IncomingContent::Command { name, args } => {
                assert_eq!(name, "model");
                assert_eq!(args, Some("gpt-4".to_string()));
            }
            _ => panic!("Expected Command content"),
        }
    }

    #[test]
    fn test_incoming_message_command_no_args() {
        let msg = IncomingMessage::command("help".to_string(), None);
        match msg.content {
            IncomingContent::Command { name, args } => {
                assert_eq!(name, "help");
                assert!(args.is_none());
            }
            _ => panic!("Expected Command content"),
        }
    }

    #[test]
    fn test_should_dispatch_private_message_when_mention_only_enabled() {
        assert!(should_dispatch_message(true, false, false));
    }

    #[test]
    fn test_should_dispatch_mentioned_group_message_when_mention_only_enabled() {
        assert!(should_dispatch_message(true, true, true));
    }

    #[test]
    fn test_should_block_unmentioned_group_message_when_mention_only_enabled() {
        assert!(!should_dispatch_message(true, true, false));
    }

    #[test]
    fn test_should_block_unmentioned_group_command_when_mention_only_enabled() {
        assert!(!should_dispatch_message(true, true, false));
    }

    #[test]
    fn test_outgoing_message_debug() {
        let msg = OutgoingMessage {
            content: OutgoingContent::Text("test".to_string()),
        };
        let debug_str = format!("{:?}", msg);
        assert!(debug_str.contains("test"));
    }

    #[test]
    fn test_incoming_content_clone() {
        let content = IncomingContent::Text("clone me".to_string());
        let cloned = content.clone();
        match (content, cloned) {
            (IncomingContent::Text(a), IncomingContent::Text(b)) => {
                assert_eq!(a, b);
            }
            _ => panic!("Clone should preserve content type"),
        }
    }

    #[test]
    fn test_outgoing_content_clone() {
        // Test Text variant
        let content = OutgoingContent::Text("hello".to_string());
        let cloned = content.clone();
        match (content, cloned) {
            (OutgoingContent::Text(a), OutgoingContent::Text(b)) => {
                assert_eq!(a, b);
            }
            _ => panic!("Clone should preserve content type for Text"),
        }

        // Test StreamChunk variant
        let content = OutgoingContent::StreamChunk("chunk".to_string());
        let cloned = content.clone();
        match (content, cloned) {
            (OutgoingContent::StreamChunk(a), OutgoingContent::StreamChunk(b)) => {
                assert_eq!(a, b);
            }
            _ => panic!("Clone should preserve content type for StreamChunk"),
        }

        // Test StreamEnd variant
        let content = OutgoingContent::StreamEnd;
        let cloned = content.clone();
        match (content, cloned) {
            (OutgoingContent::StreamEnd, OutgoingContent::StreamEnd) => {}
            _ => panic!("Clone should preserve content type for StreamEnd"),
        }

        // Test Error variant
        let content = OutgoingContent::Error("error".to_string());
        let cloned = content.clone();
        match (content, cloned) {
            (OutgoingContent::Error(a), OutgoingContent::Error(b)) => {
                assert_eq!(a, b);
            }
            _ => panic!("Clone should preserve content type for Error"),
        }
    }

    #[test]
    fn test_parse_incoming_text() {
        let incoming = parse_incoming("hello world");
        match incoming.content {
            IncomingContent::Text(text) => assert_eq!(text, "hello world"),
            _ => panic!("Expected text"),
        }
    }

    #[test]
    fn test_parse_incoming_command() {
        let incoming = parse_incoming("/model fast");
        match incoming.content {
            IncomingContent::Command { name, args } => {
                assert_eq!(name, "model");
                assert_eq!(args, Some("fast".to_string()));
            }
            _ => panic!("Expected command"),
        }
    }

    #[test]
    fn test_parse_incoming_command_no_args() {
        let incoming = parse_incoming("/help");
        match incoming.content {
            IncomingContent::Command { name, args } => {
                assert_eq!(name, "help");
                assert!(args.is_none());
            }
            _ => panic!("Expected command"),
        }
    }

    #[test]
    fn test_split_by_chars() {
        let text = "abcdefghij";
        let chunks: Vec<_> = split_by_chars(text, 3).collect();
        assert_eq!(chunks, vec!["abc", "def", "ghi", "j"]);
    }

    #[test]
    fn test_split_by_chars_short() {
        let text = "ab";
        let chunks: Vec<_> = split_by_chars(text, 5).collect();
        assert_eq!(chunks, vec!["ab"]);
    }
}
