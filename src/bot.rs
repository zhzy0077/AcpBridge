//! Bot - a per-chat AI agent instance
//!
//! Each Bot is bound to a specific (channel, chat_id, bot_config).
//! It owns an AcpClient and processes incoming messages from its chat.

use crate::acp_client;
use crate::channel::{ChatId, IncomingContent, IncomingMessage, OutgoingContent, OutgoingMessage};
use crate::config::BotConfig;
use crate::message_bus::{BotEvent, BotReceiver, MessageBus};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

/// A Bot instance, bound to one (channel, chat_id).
///
/// It processes incoming messages and forwards them to an AcpClient.
/// It sends outgoing messages back through the MessageBus.
pub struct Bot {
    /// The bot configuration (from top-level config)
    config: BotConfig,
    /// The channel this bot lives on
    channel_name: String,
    /// The chat this bot is bound to
    chat_id: ChatId,
    /// MessageBus for sending outgoing messages
    message_bus: Arc<MessageBus>,
    /// ACP client state (connection to agent subprocess)
    acp_state: Arc<acp_client::AcpClientState>,
    /// Whether the ACP client is connected
    connected: Arc<tokio::sync::Mutex<bool>>,
    /// Receiver for outgoing messages produced by AcpClientState
    outgoing_rx: Option<mpsc::Receiver<OutgoingMessage>>,
}

enum StartupWaitResult {
    Connected,
    Failed,
    TimedOut,
}

impl Bot {
    /// Create a new Bot instance.
    ///
    /// Does NOT start the AcpClient yet - call `run()` for that.
    pub fn new(
        config: BotConfig,
        channel_name: String,
        chat_id: ChatId,
        message_bus: Arc<MessageBus>,
        mcp_server_port: Option<u16>,
    ) -> Self {
        let working_dir = config
            .working_dir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let (outgoing_tx, outgoing_rx) = mpsc::channel::<OutgoingMessage>(100);

        let mcp_servers = mcp_server_port.map(|port| {
            let url = format!(
                "http://localhost:{}/mcp/{}/{}/{}",
                port, channel_name, config.name, chat_id.0
            );
            vec![agent_client_protocol::McpServer::Http(
                agent_client_protocol::McpServerHttp::new("mcp", url),
            )]
        });

        let acp_state = Arc::new(acp_client::AcpClientState::new(
            working_dir,
            config.show_thinking,
            config.show_auto_approved,
            outgoing_tx,
            mcp_servers,
        ));

        Self {
            config,
            channel_name,
            chat_id,
            message_bus,
            acp_state,
            connected: Arc::new(tokio::sync::Mutex::new(false)),
            outgoing_rx: Some(outgoing_rx),
        }
    }

    /// Run the bot: start the AcpClient subprocess and process incoming messages.
    ///
    /// This method runs until the incoming channel is closed, shutdown signal received,
    /// or an unrecoverable error occurs.
    pub async fn run(mut self, receiver: BotReceiver) {
        let chat_id = self.chat_id.clone();
        let bot_name = self.config.name.clone();
        info!(bot = %bot_name, chat_id = %chat_id.0, "Starting bot");

        // Track whether instructions have been sent with the first prompt
        let mut has_sent_instructions = false;

        let BotReceiver { mut event_rx } = receiver;

        // Forward outgoing messages from AcpClientState back to the user via MessageBus
        let mut outgoing_rx = self.outgoing_rx.take().expect("outgoing_rx already taken");
        let fwd_channel = self.channel_name.clone();
        let fwd_chat_id = self.chat_id.clone();
        let fwd_bus = self.message_bus.clone();
        tokio::spawn(async move {
            while let Some(msg) = outgoing_rx.recv().await {
                if let Err(e) = fwd_bus.send(&fwd_channel, fwd_chat_id.clone(), msg).await {
                    error!(error = %e, "Failed to forward outgoing message");
                }
            }
        });

        // Send "starting" message to user
        self.send_text("Agent is starting...").await;

        // Start AcpClient subprocess
        let connected_clone = self.connected.clone();
        let startup_failure = Arc::new(tokio::sync::Mutex::new(None));
        let startup_failure_clone = startup_failure.clone();
        let acp_state_clone = self.acp_state.clone();
        let agent_command = self.config.agent.command.clone();
        let agent_args = self.config.agent.args.clone();

        let acp_handle = tokio::task::spawn_local(async move {
            if let Err(e) = acp_client::run_client(
                agent_command,
                agent_args,
                acp_state_clone.clone(),
                connected_clone.clone(),
            )
            .await
            {
                let error_message = e.to_string();
                error!(error = %error_message, "ACP client error");

                if !*connected_clone.lock().await {
                    *startup_failure_clone.lock().await = Some(error_message.clone());
                    acp_state_clone.send_error(error_message).await;
                }
            }
        });

        // Wait for connection
        match self
            .wait_for_connection(&acp_handle, &startup_failure)
            .await
        {
            StartupWaitResult::Connected => {}
            StartupWaitResult::Failed => {
                return;
            }
            StartupWaitResult::TimedOut => {
                self.send_error("Agent failed to connect within timeout")
                    .await;
                acp_handle.abort();
                return;
            }
        }

        self.apply_configured_session_settings().await;

        // Main message loop
        loop {
            match event_rx.recv().await {
                Some(BotEvent::Message(msg)) => {
                    if !*self.connected.lock().await {
                        self.send_error("Agent disconnected").await;
                        break;
                    }
                    self.handle_message(msg, &mut has_sent_instructions).await;
                }
                Some(BotEvent::Shutdown) => {
                    info!(bot = %bot_name, chat_id = %self.chat_id.0, "Received shutdown signal");
                    self.send_text("Bot is shutting down...").await;
                    break;
                }
                None => {
                    info!(bot = %bot_name, chat_id = %self.chat_id.0, "Event channel closed");
                    break;
                }
            }
        }

        // Graceful shutdown
        info!(bot = %bot_name, chat_id = %self.chat_id.0, "Bot stopping gracefully");

        // Abort the ACP client handle
        acp_handle.abort();

        // Wait a bit for ACP client to clean up
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        info!(bot = %bot_name, chat_id = %self.chat_id.0, "Bot stopped");
    }

    /// Wait for the ACP client to connect, with timeout.
    async fn wait_for_connection(
        &self,
        acp_handle: &JoinHandle<()>,
        startup_failure: &tokio::sync::Mutex<Option<String>>,
    ) -> StartupWaitResult {
        for _ in 0..100 {
            if *self.connected.lock().await {
                return StartupWaitResult::Connected;
            }
            if startup_failure.lock().await.is_some() {
                return StartupWaitResult::Failed;
            }
            if acp_handle.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        if startup_failure.lock().await.is_some() {
            return StartupWaitResult::Failed;
        }

        warn!(chat_id = %self.chat_id.0, "Agent connection timeout");
        StartupWaitResult::TimedOut
    }

    async fn apply_configured_session_settings(&self) {
        if let Some(ref model) = self.config.model {
            info!(model = %model, "Setting configured model");
            if let Err(e) = acp_client::send_set_model_command(&self.acp_state, model.clone()).await
            {
                warn!(error = %e, model = %model, "Failed to set configured model");
                self.send_error(&format!(
                    "Failed to apply configured model '{}': {}",
                    model, e
                ))
                .await;
            }
        }

        if let Some(ref mode) = self.config.mode {
            info!(mode = %mode, "Setting configured mode");
            if let Err(e) = acp_client::send_set_mode_command(&self.acp_state, mode.clone()).await {
                warn!(error = %e, mode = %mode, "Failed to set configured mode");
                self.send_error(&format!(
                    "Failed to apply configured mode '{}': {}",
                    mode, e
                ))
                .await;
            }
        }

        for (config_id, value) in &self.config.config_options {
            info!(config_id = %config_id, value = %value, "Setting configured ACP config option");
            if let Err(e) = acp_client::send_set_config_option_command(
                &self.acp_state,
                config_id.clone(),
                value.clone(),
            )
            .await
            {
                warn!(error = %e, config_id = %config_id, value = %value, "Failed to set configured ACP config option");
                self.send_error(&format!(
                    "Failed to apply configured option '{}={}': {}",
                    config_id, value, e
                ))
                .await;
            }
        }
    }

    fn format_config_options(options: &[acp_client::SessionConfigOptionState]) -> String {
        if options.is_empty() {
            return "No config options available.".to_string();
        }

        let mut lines = vec!["Available config options:".to_string()];
        for option in options {
            let mut header = format!("- {}", option.id);
            if let Some(category) = &option.category {
                header.push_str(&format!(" [{}]", category));
            }
            if let Some(current_value) = &option.current_value {
                header.push_str(&format!(" current={}", current_value));
            }
            header.push_str(&format!(" ({})", option.name));
            lines.push(header);

            if !option.choices.is_empty() {
                let choices = option
                    .choices
                    .iter()
                    .map(|choice| {
                        if let Some(group) = &choice.group {
                            format!("{}/{} ({})", group, choice.value, choice.name)
                        } else {
                            format!("{} ({})", choice.value, choice.name)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(format!("  choices: {}", choices));
            }
        }

        lines.join("\n")
    }

    /// Handle a single incoming message.
    async fn handle_message(&mut self, msg: IncomingMessage, has_sent_instructions: &mut bool) {
        match msg.content {
            IncomingContent::Text(text) => {
                info!(text = %text, chat_id = %self.chat_id.0, "User text message");
                self.handle_text(text, has_sent_instructions).await;
            }
            IncomingContent::Command { name, args } => {
                info!(command = %name, args = ?args, chat_id = %self.chat_id.0, "User command");
                self.handle_command(&name, args).await;
            }
        }
    }

    /// Handle a text message: prepend instructions if first message, then send to agent.
    async fn handle_text(&self, text: String, has_sent_instructions: &mut bool) {
        let text_to_send = if !*has_sent_instructions {
            if let Some(ref instructions) = self.config.instructions {
                *has_sent_instructions = true;
                format!("{}\n\n{}", instructions, text)
            } else {
                *has_sent_instructions = true;
                text
            }
        } else {
            text
        };

        if let Err(e) = acp_client::send_prompt(&self.acp_state, text_to_send).await {
            error!(error = %e, "Failed to send prompt");
        }
    }

    /// Handle a slash command.
    async fn handle_command(&self, name: &str, args: Option<String>) {
        match name {
            "new" => {
                self.send_text("Starting new session...").await;
                match acp_client::send_new_session_command(&self.acp_state).await {
                    Ok(session_id) => {
                        self.apply_configured_session_settings().await;
                        self.send_text(&format!("New session started: {}", session_id))
                            .await;
                    }
                    Err(e) => {
                        self.send_error(&format!("Failed to start new session: {}", e))
                            .await;
                    }
                }
            }
            "model" => {
                if let Some(model_name) = args {
                    self.send_text(&format!("Switching to model: {}...", model_name))
                        .await;
                    match acp_client::send_set_model_command(&self.acp_state, model_name.clone())
                        .await
                    {
                        Ok(_) => {
                            self.send_text(&format!("Model changed to: {}", model_name))
                                .await;
                        }
                        Err(e) => {
                            self.send_error(&format!("Failed to change model: {}", e))
                                .await;
                        }
                    }
                } else {
                    match acp_client::send_get_models_command(&self.acp_state).await {
                        Ok((models, current_model)) if !models.is_empty() => {
                            let mut list = String::new();
                            for m in models {
                                if Some(&m) == current_model.as_ref() {
                                    list.push_str(&format!("- {} (current)\n", m));
                                } else {
                                    list.push_str(&format!("- {}\n", m));
                                }
                            }
                            self.send_text(&format!("Available models:\n{}", list.trim_end()))
                                .await;
                        }
                        Ok(_) => {
                            self.send_text("No models available.").await;
                        }
                        Err(e) => {
                            self.send_error(&format!("Failed to get models: {}", e))
                                .await;
                        }
                    }
                }
            }
            "mode" => {
                if let Some(mode_name) = args {
                    self.send_text(&format!("Switching to mode: {}...", mode_name))
                        .await;
                    match acp_client::send_set_mode_command(&self.acp_state, mode_name.clone())
                        .await
                    {
                        Ok(_) => {
                            self.send_text(&format!("Mode changed to: {}", mode_name))
                                .await;
                        }
                        Err(e) => {
                            self.send_error(&format!("Failed to change mode: {}", e))
                                .await;
                        }
                    }
                } else {
                    match acp_client::send_get_modes_command(&self.acp_state).await {
                        Ok((modes, current_mode)) if !modes.is_empty() => {
                            let mut list = String::new();
                            for m in modes {
                                if Some(&m) == current_mode.as_ref() {
                                    list.push_str(&format!("- {} (current)\n", m));
                                } else {
                                    list.push_str(&format!("- {}\n", m));
                                }
                            }
                            self.send_text(&format!("Available modes:\n{}", list.trim_end()))
                                .await;
                        }
                        Ok(_) => {
                            self.send_text("No modes available.").await;
                        }
                        Err(e) => {
                            self.send_error(&format!("Failed to get modes: {}", e))
                                .await;
                        }
                    }
                }
            }
            "config" => {
                if let Some(raw_args) = args.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    let mut parts = raw_args.split_whitespace();
                    let Some(config_id) = parts.next() else {
                        self.send_error("Usage: /config [id value]").await;
                        return;
                    };
                    let value = parts.collect::<Vec<_>>().join(" ");
                    if value.is_empty() {
                        self.send_error("Usage: /config [id value]").await;
                        return;
                    }

                    self.send_text(&format!("Setting config option {}={}...", config_id, value))
                        .await;
                    match acp_client::send_set_config_option_command(
                        &self.acp_state,
                        config_id.to_string(),
                        value.clone(),
                    )
                    .await
                    {
                        Ok(_) => {
                            self.send_text(&format!(
                                "Config option changed: {}={}",
                                config_id, value
                            ))
                            .await;
                        }
                        Err(e) => {
                            self.send_error(&format!(
                                "Failed to change config option '{}': {}",
                                config_id, e
                            ))
                            .await;
                        }
                    }
                } else {
                    match acp_client::send_get_config_options_command(&self.acp_state).await {
                        Ok(options) => {
                            self.send_text(&Self::format_config_options(&options)).await;
                        }
                        Err(e) => {
                            self.send_error(&format!("Failed to get config options: {}", e))
                                .await;
                        }
                    }
                }
            }
            "cd" => {
                if let Some(path) = args {
                    let new_dir = PathBuf::from(&path);
                    if new_dir.exists() && new_dir.is_dir() {
                        self.send_text(&format!("Changing directory to: {}...", new_dir.display()))
                            .await;
                        match acp_client::send_change_directory_command(
                            &self.acp_state,
                            new_dir.clone(),
                        )
                        .await
                        {
                            Ok(session_id) => {
                                self.apply_configured_session_settings().await;
                                self.send_text(&format!(
                                    "Changed directory to: {}\nNew session: {}",
                                    new_dir.display(),
                                    session_id
                                ))
                                .await;
                            }
                            Err(e) => {
                                self.send_error(&format!("Failed to change directory: {}", e))
                                    .await;
                            }
                        }
                    } else {
                        self.send_error(&format!("Directory not found: {}", path))
                            .await;
                    }
                } else {
                    self.send_error("Usage: /cd <path>").await;
                }
            }
            "help" => {
                let help_text = "\
Available commands:
/new - Start a new session
/model [name] - Switch model or list available models
/mode [name] - Switch mode or list available modes
/config [id value] - List config options or set one
/cd <path> - Change working directory
/bot [name] - Switch bot or list available bots
/help - Show this help message";
                self.send_text(help_text).await;
            }
            // /bot is handled by Orchestrator before dispatching to Bot,
            // but if it reaches here, show help
            "bot" => {
                self.send_text("Use /bot to list bots or /bot <name> to switch.")
                    .await;
            }
            _ => {
                self.send_error(&format!("Unknown command: /{}", name))
                    .await;
            }
        }
    }

    /// Send a text message back to the user via MessageBus.
    async fn send_text(&self, text: &str) {
        let msg = OutgoingMessage {
            content: OutgoingContent::Text(text.to_string()),
        };
        if let Err(e) = self
            .message_bus
            .send(&self.channel_name, self.chat_id.clone(), msg)
            .await
        {
            error!(error = %e, "Failed to send text to user");
        }
    }

    /// Send an error message back to the user via MessageBus.
    async fn send_error(&self, text: &str) {
        let msg = OutgoingMessage {
            content: OutgoingContent::Error(text.to_string()),
        };
        if let Err(e) = self
            .message_bus
            .send(&self.channel_name, self.chat_id.clone(), msg)
            .await
        {
            error!(error = %e, "Failed to send error to user");
        }
    }
}
