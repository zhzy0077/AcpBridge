//! MessageBus - pure message routing between Channels and Bots
//!
//! MessageBus decouples Channels from Bots. Neither side knows about the other.
//! - Channels call `dispatch()` to send incoming user messages to a Bot
//! - Bots call `send()` to send outgoing messages back through the Channel
//! - Channels call `subscribe()` once at startup to get their outgoing_rx

use crate::channel::{ChatId, IncomingMessage, OutgoingMessage};
use std::collections::HashMap;
use tokio::sync::{Mutex, mpsc};

/// Key that uniquely identifies a Bot instance at runtime.
/// A Bot instance is bound to a specific channel + chat + bot config.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BotInstanceKey {
    pub channel_name: String,
    pub chat_id: ChatId,
    pub bot_name: String,
}

/// Events delivered to a bot instance.
pub enum BotEvent {
    /// Incoming message from user
    Message(IncomingMessage),
    /// Shutdown signal - bot should clean up and exit
    Shutdown,
}

/// Bot registration info - sender side of the bot's event channel
struct BotRegistration {
    event_tx: mpsc::Sender<BotEvent>,
}

/// MessageBus routes messages between Channels and Bot instances.
///
/// Thread-safe: all fields are behind Mutex, can be shared via Arc.
pub struct MessageBus {
    /// Per-channel outgoing sender.
    /// When a Bot calls `send()`, the message goes to the Channel's outgoing_tx.
    /// Key: channel_name
    outgoing_txs: Mutex<HashMap<String, mpsc::Sender<(ChatId, OutgoingMessage)>>>,

    /// Per-bot-instance registration.
    /// Key: (channel_name, chat_id, bot_name)
    bots: Mutex<HashMap<BotInstanceKey, BotRegistration>>,
}

/// Receiver for a bot instance
pub struct BotReceiver {
    pub event_rx: mpsc::Receiver<BotEvent>,
}

impl Default for MessageBus {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageBus {
    /// Create a new empty MessageBus.
    pub fn new() -> Self {
        Self {
            outgoing_txs: Mutex::new(HashMap::new()),
            bots: Mutex::new(HashMap::new()),
        }
    }

    /// Register a channel's outgoing sender.
    ///
    /// Called once per channel at startup. The channel keeps the rx side.
    /// All bots on this channel will share this tx to send messages back.
    ///
    /// # Arguments
    /// * `channel_name` - Unique name of the channel (from ChannelConfig.name)
    /// * `outgoing_tx` - The sender side; the channel holds the receiver
    pub async fn register_channel(
        &self,
        channel_name: String,
        outgoing_tx: mpsc::Sender<(ChatId, OutgoingMessage)>,
    ) {
        self.outgoing_txs
            .lock()
            .await
            .insert(channel_name, outgoing_tx);
    }

    /// Register a bot instance and return the receivers.
    ///
    /// Called by Orchestrator when creating a new Bot instance.
    /// The Bot will listen on the returned rx for incoming messages and shutdown signals.
    ///
    /// # Arguments
    /// * `key` - Unique key identifying this bot instance
    ///
    /// # Returns
    /// The receivers for this bot instance (incoming messages and shutdown signal).
    pub async fn register_bot(&self, key: BotInstanceKey) -> BotReceiver {
        let (event_tx, event_rx) = mpsc::channel::<BotEvent>(100);

        self.bots
            .lock()
            .await
            .insert(key, BotRegistration { event_tx });

        BotReceiver { event_rx }
    }

    /// Unregister a bot instance and signal it to shut down gracefully.
    ///
    /// Called when Bot needs to be stopped (e.g., during /bot switch or cleanup).
    /// Sends a shutdown signal to the bot, allowing it to clean up before exiting.
    pub async fn unregister_bot(&self, key: &BotInstanceKey) {
        if let Some(reg) = self.bots.lock().await.remove(key) {
            let _ = reg.event_tx.send(BotEvent::Shutdown).await;
        }
    }

    /// Dispatch an incoming message from a Channel to a Bot instance.
    ///
    /// Called by Channel when it receives a user message.
    /// The Channel must have already asked Orchestrator to ensure the Bot exists.
    ///
    /// # Arguments
    /// * `key` - The target bot instance
    /// * `msg` - The incoming message to deliver
    ///
    /// # Returns
    /// Ok(()) if sent, Err if the bot's channel is closed/not found.
    pub async fn dispatch(&self, key: &BotInstanceKey, msg: IncomingMessage) -> anyhow::Result<()> {
        let bots = self.bots.lock().await;
        if let Some(reg) = bots.get(key) {
            reg.event_tx
                .send(BotEvent::Message(msg))
                .await
                .map_err(|_| anyhow::anyhow!("Bot event channel closed"))?;
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Bot instance not found: channel={}, chat={}, bot={}",
                key.channel_name,
                key.chat_id.0,
                key.bot_name
            ))
        }
    }

    /// Send an outgoing message from a Bot back to its Channel.
    ///
    /// Called by Bot when it wants to send a message to the user.
    ///
    /// # Arguments
    /// * `channel_name` - Which channel to send through
    /// * `chat_id` - Which chat to send to
    /// * `msg` - The outgoing message
    pub async fn send(
        &self,
        channel_name: &str,
        chat_id: ChatId,
        msg: OutgoingMessage,
    ) -> anyhow::Result<()> {
        let txs = self.outgoing_txs.lock().await;
        if let Some(tx) = txs.get(channel_name) {
            tx.send((chat_id, msg))
                .await
                .map_err(|_| anyhow::anyhow!("Channel outgoing channel closed"))?;
            Ok(())
        } else {
            Err(anyhow::anyhow!("Channel not found: {}", channel_name))
        }
    }
}
