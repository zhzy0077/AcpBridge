//! Channel abstraction layer
//!
//! This module defines the `Channel` trait that abstracts over different
//! messaging platforms (Telegram, QQ, Lark/Feishu, etc.). Each platform implements
//! this trait to handle message sending and receiving.

mod lark;
mod qq;
mod telegram;
mod wechat;

pub use lark::LarkChannel;
pub use qq::QqChannel;
pub use telegram::TelegramChannel;
pub use wechat::{WeChatChannel, WeChatConfig};

/// Unique identifier for a chat/dialog (platform-specific)
///
/// Warning: The internal format is platform-specific and private to each channel
/// implementation. External code should not depend on or parse the inner String.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChatId(pub String);

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

pub(crate) fn should_dispatch_message(
    mention_only: bool,
    is_group_chat: bool,
    is_mentioned: bool,
) -> bool {
    !mention_only || !is_group_chat || is_mentioned
}

/// Channel trait for messaging platform implementations
///
/// Implementors must:
/// - Start an async task that receives messages and sends them via `incoming_tx`
/// - Listen on `outgoing_rx` and forward messages to users
/// - Clean up gracefully when the future is cancelled
#[async_trait::async_trait]
pub trait Channel: Send + Sync {
    /// Start the channel and begin processing messages
    ///
    /// # Arguments
    /// * `channel_name` - The unique name of this channel (from config)
    /// * `orchestrator` - The orchestrator for getting/creating bots
    /// * `message_bus` - The message bus for routing messages
    async fn start(
        &self,
        channel_name: String,
        orchestrator: std::sync::Arc<crate::orchestrator::Orchestrator>,
        message_bus: std::sync::Arc<crate::message_bus::MessageBus>,
    ) -> anyhow::Result<()>;

    /// Search for other bots present in the given chat.
    ///
    /// Queries the platform API to find which bots are members of the chat,
    /// then maps their platform IDs back to channel names using the platform's
    /// static bot registry. Returns the channel names of other bots in the chat.
    ///
    /// Default implementation returns empty (platforms that don't support discovery).
    async fn search_bots_in_chat(&self, _chat_id: &ChatId) -> Vec<String> {
        vec![]
    }
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
}
