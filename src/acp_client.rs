//! ACP (Agent Client Protocol) Client implementation
//!
//! This module implements the ACP Client using the `agent-client-protocol` SDK.
//! It manages the Agent CLI subprocess lifecycle and protocol communication.

use crate::channel::{OutgoingContent, OutgoingMessage};
use agent_client_protocol::{self as acp, Agent};
use anyhow::{Context, Result, anyhow};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command as TokioCommand;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::sleep;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{debug, error, info, warn};

/// Commands that can be sent to the ACP connection from outside the LocalSet
#[derive(Debug)]
pub enum AcpCommand {
    /// Create a new session
    NewSession {
        response_tx: oneshot::Sender<Result<String>>,
    },
    /// Set the model for the current session
    SetModel {
        model: String,
        response_tx: oneshot::Sender<Result<()>>,
    },
    /// Change the working directory (creates new session)
    ChangeDirectory {
        path: PathBuf,
        response_tx: oneshot::Sender<Result<String>>,
    },
    /// Get available models
    GetModels {
        response_tx: oneshot::Sender<(Vec<String>, Option<String>)>,
    },
    /// Set the mode for the current session
    SetMode {
        mode: String,
        response_tx: oneshot::Sender<Result<()>>,
    },
    /// Get available modes
    GetModes {
        response_tx: oneshot::Sender<(Vec<String>, Option<String>)>,
    },
    /// Set a generic ACP session config option
    SetConfigOption {
        config_id: String,
        value: String,
        response_tx: oneshot::Sender<Result<()>>,
    },
    /// Get available ACP session config options
    GetConfigOptions {
        response_tx: oneshot::Sender<Vec<SessionConfigOptionState>>,
    },
}

#[derive(Debug, Clone)]
pub struct SessionConfigOptionChoice {
    pub value: String,
    pub name: String,
    pub group: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionConfigOptionState {
    pub id: String,
    pub name: String,
    pub category: Option<String>,
    pub current_value: Option<String>,
    pub choices: Vec<SessionConfigOptionChoice>,
}

/// ACP Client state shared between connection tasks
pub struct AcpClientState {
    /// Sender for outgoing messages back to the user (owned by Bot)
    outgoing_tx: mpsc::Sender<OutgoingMessage>,
    /// MCP servers to inject into new sessions (pre-computed by Bot)
    mcp_servers: Option<Vec<acp::McpServer>>,
    /// Currently active session ID (if any)
    session_id: Arc<Mutex<Option<acp::SessionId>>>,
    /// Current working directory for new sessions
    working_dir: Arc<Mutex<PathBuf>>,
    /// Available models for the current session
    available_models: Arc<Mutex<Vec<String>>>,
    /// Currently active model for the session
    active_model: Arc<Mutex<Option<String>>>,
    /// Available modes for the current session
    available_modes: Arc<Mutex<Vec<String>>>,
    /// Currently active mode for the session
    active_mode: Arc<Mutex<Option<String>>>,
    /// Available ACP config options for the current session
    available_config_options: Arc<Mutex<Vec<SessionConfigOptionState>>>,
    /// Connection handle for sending prompts (used within LocalSet)
    connection: Arc<Mutex<Option<AcpConnectionHandle>>>,
    /// Channel for sending commands to the ACP connection
    command_tx: mpsc::Sender<AcpCommand>,
    /// Channel for receiving commands in the ACP connection
    command_rx: Arc<Mutex<Option<mpsc::Receiver<AcpCommand>>>>,
    /// Whether to show agent thinking chunks to users
    show_thinking: bool,
    /// Whether to show "[Auto-approved tool]" messages to users
    show_auto_approved: bool,
}

impl AcpClientState {
    pub fn new(
        working_dir: PathBuf,
        show_thinking: bool,
        show_auto_approved: bool,
        outgoing_tx: mpsc::Sender<OutgoingMessage>,
        mcp_servers: Option<Vec<acp::McpServer>>,
    ) -> Self {
        let (command_tx, command_rx) = mpsc::channel::<AcpCommand>(10);
        Self {
            outgoing_tx,
            mcp_servers,
            session_id: Arc::new(Mutex::new(None)),
            working_dir: Arc::new(Mutex::new(working_dir)),
            available_models: Arc::new(Mutex::new(Vec::new())),
            active_model: Arc::new(Mutex::new(None)),
            available_modes: Arc::new(Mutex::new(Vec::new())),
            active_mode: Arc::new(Mutex::new(None)),
            available_config_options: Arc::new(Mutex::new(Vec::new())),
            connection: Arc::new(Mutex::new(None)),
            command_tx,
            command_rx: Arc::new(Mutex::new(Some(command_rx))),
            show_thinking,
            show_auto_approved,
        }
    }

    /// Get available models
    pub async fn get_available_models(&self) -> Vec<String> {
        self.available_models.lock().await.clone()
    }

    /// Set available models
    pub async fn set_available_models(&self, models: Vec<String>) {
        *self.available_models.lock().await = models;
    }

    /// Get active model
    pub async fn get_active_model(&self) -> Option<String> {
        self.active_model.lock().await.clone()
    }

    /// Set active model
    pub async fn set_active_model(&self, model: Option<String>) {
        *self.active_model.lock().await = model;
    }

    /// Get available modes
    pub async fn get_available_modes(&self) -> Vec<String> {
        self.available_modes.lock().await.clone()
    }

    /// Set available modes
    pub async fn set_available_modes(&self, modes: Vec<String>) {
        *self.available_modes.lock().await = modes;
    }

    /// Get active mode
    pub async fn get_active_mode(&self) -> Option<String> {
        self.active_mode.lock().await.clone()
    }

    /// Set active mode
    pub async fn set_active_mode(&self, mode: Option<String>) {
        *self.active_mode.lock().await = mode;
    }

    /// Get available ACP config options
    pub async fn get_available_config_options(&self) -> Vec<SessionConfigOptionState> {
        self.available_config_options.lock().await.clone()
    }

    /// Set available ACP config options
    pub async fn set_available_config_options(&self, options: Vec<SessionConfigOptionState>) {
        *self.available_config_options.lock().await = options;
    }

    /// Get the working directory
    pub async fn get_working_dir(&self) -> PathBuf {
        self.working_dir.lock().await.clone()
    }

    /// Set the working directory
    pub async fn set_working_dir(&self, path: PathBuf) {
        *self.working_dir.lock().await = path;
    }

    /// Get the current session ID
    pub async fn get_session_id(&self) -> Option<acp::SessionId> {
        self.session_id.lock().await.clone()
    }

    /// Set the current session ID
    pub async fn set_session_id(&self, session_id: Option<acp::SessionId>) {
        *self.session_id.lock().await = session_id;
    }

    /// Send a message to the user through the channel
    pub async fn send_to_user(&self, text: String) {
        let msg = OutgoingMessage {
            content: OutgoingContent::Text(text),
        };
        if self.outgoing_tx.send(msg).await.is_err() {
            error!("Failed to send message to user: receiver dropped");
        }
    }

    /// Send an error message to the user
    pub async fn send_error(&self, error: String) {
        let msg = OutgoingMessage {
            content: OutgoingContent::Error(error),
        };
        if self.outgoing_tx.send(msg).await.is_err() {
            error!("Failed to send error to user: receiver dropped");
        }
    }

    /// Store the connection handle for later use (called from LocalSet)
    pub async fn set_connection(&self, conn: AcpConnectionHandle) {
        *self.connection.lock().await = Some(conn);
    }

    /// Get the stored connection handle
    pub async fn get_connection(&self) -> Option<AcpConnectionHandle> {
        self.connection.lock().await.clone()
    }

    /// Get the command channel sender (for sending commands from outside)
    pub fn get_command_tx(&self) -> mpsc::Sender<AcpCommand> {
        self.command_tx.clone()
    }

    /// Take the command receiver (should only be called once by the ACP protocol task)
    pub async fn take_command_rx(&self) -> Option<mpsc::Receiver<AcpCommand>> {
        self.command_rx.lock().await.take()
    }

    /// Get whether to show agent thinking chunks
    pub fn show_thinking(&self) -> bool {
        self.show_thinking
    }

    /// Get whether to show "[Auto-approved tool]" messages
    pub fn show_auto_approved(&self) -> bool {
        self.show_auto_approved
    }

    fn mcp_servers(&self) -> Option<Vec<acp::McpServer>> {
        self.mcp_servers.clone()
    }
}

fn config_option_category_name(category: &acp::SessionConfigOptionCategory) -> String {
    match category {
        acp::SessionConfigOptionCategory::Mode => "mode".to_string(),
        acp::SessionConfigOptionCategory::Model => "model".to_string(),
        acp::SessionConfigOptionCategory::ThoughtLevel => "thought_level".to_string(),
        acp::SessionConfigOptionCategory::Other(value) => value.clone(),
        _ => "unknown".to_string(),
    }
}

fn flatten_config_option_choices(
    options: &acp::SessionConfigSelectOptions,
) -> Vec<SessionConfigOptionChoice> {
    match options {
        acp::SessionConfigSelectOptions::Ungrouped(entries) => entries
            .iter()
            .map(|entry| SessionConfigOptionChoice {
                value: entry.value.to_string(),
                name: entry.name.clone(),
                group: None,
            })
            .collect(),
        acp::SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| {
                group.options.iter().map(|entry| SessionConfigOptionChoice {
                    value: entry.value.to_string(),
                    name: entry.name.clone(),
                    group: Some(group.name.clone()),
                })
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn map_config_option(option: &acp::SessionConfigOption) -> SessionConfigOptionState {
    let (current_value, choices) = match &option.kind {
        acp::SessionConfigKind::Select(select) => (
            Some(select.current_value.to_string()),
            flatten_config_option_choices(&select.options),
        ),
        _ => (None, Vec::new()),
    };

    SessionConfigOptionState {
        id: option.id.to_string(),
        name: option.name.clone(),
        category: option.category.as_ref().map(config_option_category_name),
        current_value,
        choices,
    }
}

fn map_config_options(options: &[acp::SessionConfigOption]) -> Vec<SessionConfigOptionState> {
    options.iter().map(map_config_option).collect()
}

/// Apply session response: update session_id, models, modes
async fn apply_session_response(state: &AcpClientState, response: &acp::NewSessionResponse) {
    state
        .set_session_id(Some(response.session_id.clone()))
        .await;
    state.set_active_model(None).await;
    state.set_active_mode(None).await;
    if let Some(ref models) = response.models {
        let names: Vec<String> = models
            .available_models
            .iter()
            .map(|m| m.model_id.to_string())
            .collect();
        state.set_available_models(names).await;
    }
    if let Some(ref modes) = response.modes {
        let names: Vec<String> = modes
            .available_modes
            .iter()
            .map(|m| m.id.to_string())
            .collect();
        state.set_available_modes(names).await;
    }
    if let Some(ref options) = response.config_options {
        state
            .set_available_config_options(map_config_options(options))
            .await;
    }
}

/// Connection handle that can be used to send prompts
#[derive(Clone)]
pub struct AcpConnectionHandle {
    /// Channel to send prompts to the ACP client
    pub prompt_tx: mpsc::Sender<String>,
}

const STARTUP_OUTPUT_LIMIT: usize = 4096;

#[derive(Debug, Default)]
struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

impl CapturedOutput {
    fn push(&mut self, chunk: &[u8]) {
        if chunk.is_empty() || self.bytes.len() >= STARTUP_OUTPUT_LIMIT {
            self.truncated |= !chunk.is_empty();
            return;
        }

        let remaining = STARTUP_OUTPUT_LIMIT - self.bytes.len();
        let take_len = chunk.len().min(remaining);
        self.bytes.extend_from_slice(&chunk[..take_len]);
        if take_len < chunk.len() {
            self.truncated = true;
        }
    }

    fn render(&self) -> Option<String> {
        let text = String::from_utf8_lossy(&self.bytes);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }

        let mut rendered = trimmed.to_string();
        if self.truncated {
            rendered.push_str("\n...[truncated]");
        }
        Some(rendered)
    }
}

#[derive(Clone, Debug)]
struct StartupDiagnostics {
    command: String,
    args: Vec<String>,
    stdout: Arc<StdMutex<CapturedOutput>>,
    stderr: Arc<StdMutex<CapturedOutput>>,
}

impl StartupDiagnostics {
    fn new(command: String, args: Vec<String>) -> Self {
        Self {
            command,
            args,
            stdout: Arc::new(StdMutex::new(CapturedOutput::default())),
            stderr: Arc::new(StdMutex::new(CapturedOutput::default())),
        }
    }

    fn format_failure(&self, stage: &str, error: impl std::fmt::Display) -> String {
        let mut message = format!("Agent startup failed while {}: {}", stage, error);
        let _ = write!(message, "\nCommand: {}", self.command_line());

        if let Some(stdout) = self.render_output(&self.stdout) {
            let _ = write!(message, "\nstdout:\n{}", stdout);
        }

        if let Some(stderr) = self.render_output(&self.stderr) {
            let _ = write!(message, "\nstderr:\n{}", stderr);
        }

        message
    }

    fn command_line(&self) -> String {
        if self.args.is_empty() {
            return self.command.clone();
        }

        format!("{} {}", self.command, self.args.join(" "))
    }

    fn render_output(&self, output: &StdMutex<CapturedOutput>) -> Option<String> {
        output.lock().ok().and_then(|captured| captured.render())
    }
}

/// Run an ACP client session with automatic process management
pub async fn run_client(
    command: String,
    args: Vec<String>,
    client_state: Arc<AcpClientState>,
    connected_flag: Arc<Mutex<bool>>,
) -> Result<()> {
    let mut restart_delay = 1u64;

    loop {
        *connected_flag.lock().await = false;
        info!(command = %command, "Starting ACP agent process");
        let diagnostics = StartupDiagnostics::new(command.clone(), args.clone());

        // Create a channel for sending prompts to this ACP session
        let (prompt_tx, prompt_rx) = mpsc::channel::<String>(32);

        // Store the connection handle
        client_state
            .set_connection(AcpConnectionHandle { prompt_tx })
            .await;

        // Take the command receiver (only once)
        let command_rx = client_state.take_command_rx().await;

        // Spawn the agent subprocess
        let mut child = TokioCommand::new(&command)
            .args(&args)
            .current_dir(client_state.get_working_dir().await)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow!(diagnostics.format_failure("spawning the agent process", e)))?;

        let stdin = child.stdin.take().expect("Failed to take stdin");
        let stdout = child.stdout.take().expect("Failed to take stdout");
        let stderr = child.stderr.take().expect("Failed to take stderr");

        let (protocol_stdout, protocol_stdout_writer) = tokio::io::duplex(8192);
        let stdout_handle = tokio::spawn(forward_stdout(
            stdout,
            protocol_stdout_writer,
            diagnostics.stdout.clone(),
        ));

        // Spawn stderr handler for logging
        let stderr_handle = tokio::spawn(handle_stderr(stderr, diagnostics.stderr.clone()));

        // Run the ACP protocol in a LocalSet
        let result = run_acp_protocol(
            stdin,
            protocol_stdout,
            client_state.clone(),
            prompt_rx,
            command_rx,
            connected_flag.clone(),
        )
        .await;

        // Clean up handles
        stdout_handle.abort();
        stderr_handle.abort();

        match result {
            Ok(()) => {
                info!("ACP session completed normally");
                break;
            }
            Err(e) => {
                if !*connected_flag.lock().await {
                    return Err(anyhow!(
                        diagnostics.format_failure("initializing the ACP session", e)
                    ));
                }
                warn!(error = %e, "ACP session error, will restart");
            }
        }

        info!(delay = restart_delay, "Restarting ACP agent after delay");
        sleep(Duration::from_secs(restart_delay)).await;
        restart_delay = (restart_delay * 2).min(60);
    }

    Ok(())
}

async fn forward_stdout<R, W>(
    mut stdout: R,
    mut writer: W,
    captured_output: Arc<StdMutex<CapturedOutput>>,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = [0u8; 4096];
    loop {
        let bytes_read = match stdout.read(&mut buffer).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                debug!(error = %e, "Failed to read agent stdout");
                break;
            }
        };

        if let Ok(mut captured) = captured_output.lock() {
            captured.push(&buffer[..bytes_read]);
        }

        if let Err(e) = writer.write_all(&buffer[..bytes_read]).await {
            debug!(error = %e, "Failed to forward agent stdout");
            break;
        }
    }

    if let Err(e) = writer.shutdown().await {
        debug!(error = %e, "Failed to close forwarded agent stdout");
    }
}

/// Handle stderr output from the agent process
async fn handle_stderr(
    stderr: tokio::process::ChildStderr,
    captured_output: Arc<StdMutex<CapturedOutput>>,
) {
    let mut stderr = tokio::io::BufReader::new(stderr);
    let mut buf = String::new();
    while let Ok(n) = stderr.read_line(&mut buf).await {
        if n == 0 {
            break;
        }
        if !buf.trim().is_empty() {
            if let Ok(mut captured) = captured_output.lock() {
                captured.push(buf.as_bytes());
            }
            debug!(stderr = %buf.trim(), "Agent stderr");
        }
        buf.clear();
    }
}

/// Run the ACP protocol over the subprocess stdio
async fn run_acp_protocol<R>(
    stdin: tokio::process::ChildStdin,
    stdout: R,
    client_state: Arc<AcpClientState>,
    mut prompt_rx: mpsc::Receiver<String>,
    mut command_rx: Option<mpsc::Receiver<AcpCommand>>,
    connected_flag: Arc<Mutex<bool>>,
) -> Result<()>
where
    R: AsyncRead + Unpin + 'static,
{
    // Convert to compat types for the SDK
    let outgoing = stdin.compat_write();
    let incoming = stdout.compat();

    // Create the ACP client implementation
    let client = AcpClient::new(client_state.clone());

    // Create the ClientSideConnection
    let (conn, handle_io) =
        acp::ClientSideConnection::new(client.clone(), outgoing, incoming, |fut| {
            tokio::task::spawn_local(fut);
        });

    // Run everything in a LocalSet (required by SDK)
    let local_set = tokio::task::LocalSet::new();

    local_set
        .run_until(async move {
            // Handle I/O in the background
            tokio::task::spawn_local(handle_io);

            // Initialize and create session with MCP context
            if let Err(e) = initialize_and_create_session(&conn, client_state.clone()).await {
                error!(error = %e, "Failed to initialize");
                return Err(e);
            }

            // Mark as connected
            *connected_flag.lock().await = true;

            // Main loop: handle prompts, commands, and keep the connection alive
            let mut timeout_check_interval = tokio::time::interval(Duration::from_secs(10));
            let client_for_loop = client.clone();

            loop {
                tokio::select! {
                    _ = timeout_check_interval.tick() => {
                        // 定期检查流式超时
                        client_for_loop.check_stream_timeout().await;
                    }
                    _ = sleep(Duration::from_secs(30)) => {
                        // Keep-alive
                        debug!("ACP connection alive");
                    }
                    Some(text) = prompt_rx.recv() => {
                        // Handle incoming prompt
                        debug!(text_len = text.len(), "Received prompt to send");
                        let session_id = client_state.get_session_id().await;
                        if let Some(session_id) = session_id {
                            let result = conn
                                .prompt(acp::PromptRequest::new(
                                    session_id,
                                    vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
                                ))
                                .await;

                            // 确保 prompt 结束后结束流式输出
                            client_for_loop.end_streaming_if_active().await;

                            if let Err(e) = result {
                                error!(error = %e, "Failed to send prompt");
                                client_state.send_error(format!("Prompt failed: {}", e)).await;
                            }
                        } else {
                            client_state.send_error("No active session".to_string()).await;
                        }
                    }
                    Some(cmd) = async {
                        if let Some(ref mut rx) = command_rx {
                            rx.recv().await
                        } else {
                            std::future::pending().await
                        }
                    } => {
                        // Handle command from the bridge
                        if let Err(e) = handle_command(
                            &conn,
                            &client_state,
                            cmd,
                        ).await {
                            error!(error = %e, "Failed to handle command");
                        }
                    }
                }
            }
        })
        .await
}

/// Handle a command
async fn handle_command(
    conn: &acp::ClientSideConnection,
    client_state: &Arc<AcpClientState>,
    cmd: AcpCommand,
) -> Result<()> {
    use acp::Agent as _;

    match cmd {
        AcpCommand::NewSession { response_tx } => {
            info!("Creating new session");
            let working_dir = client_state.get_working_dir().await;

            // Build NewSessionRequest with optional MCP servers
            let mut req = acp::NewSessionRequest::new(working_dir);

            // Inject MCP server URL if configured
            if let Some(servers) = client_state.mcp_servers() {
                req = req.mcp_servers(servers);
            }

            match conn.new_session(req).await {
                Ok(response) => {
                    apply_session_response(client_state, &response).await;
                    info!(session_id = %response.session_id, "New session created");
                    let _ = response_tx.send(Ok(response.session_id.to_string()));
                }
                Err(e) => {
                    error!(error = %e, "Failed to create new session");
                    let _ = response_tx.send(Err(anyhow!("Failed to create session: {}", e)));
                }
            }
        }
        AcpCommand::SetModel { model, response_tx } => {
            info!(model = %model, "Setting session model");
            if let Some(session_id) = client_state.get_session_id().await {
                // Use set_session_model (requires unstable_session_model feature)
                match conn
                    .set_session_model(acp::SetSessionModelRequest::new(
                        session_id,
                        acp::ModelId::new(model.clone()),
                    ))
                    .await
                {
                    Ok(_) => {
                        info!("Model changed successfully");
                        client_state.set_active_model(Some(model.clone())).await;
                        let _ = response_tx.send(Ok(()));
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to set model");
                        let _ = response_tx.send(Err(anyhow!("Failed to set model: {}", e)));
                    }
                }
            } else {
                let _ = response_tx.send(Err(anyhow!("No active session")));
            }
        }
        AcpCommand::ChangeDirectory { path, response_tx } => {
            info!(path = %path.display(), "Changing working directory");

            // Update the working directory
            client_state.set_working_dir(path.clone()).await;

            // Create a new session with the new working directory and optional MCP
            let mut req = acp::NewSessionRequest::new(path);

            // Inject MCP server URL if configured
            if let Some(servers) = client_state.mcp_servers() {
                req = req.mcp_servers(servers);
            }

            match conn.new_session(req).await {
                Ok(response) => {
                    apply_session_response(client_state, &response).await;
                    info!(session_id = %response.session_id, "New session created in new directory");
                    let _ = response_tx.send(Ok(response.session_id.to_string()));
                }
                Err(e) => {
                    error!(error = %e, "Failed to create session in new directory");
                    let _ = response_tx.send(Err(anyhow!("Failed to create session: {}", e)));
                }
            }
        }
        AcpCommand::GetModels { response_tx } => {
            let models = client_state.get_available_models().await;
            let active = client_state.get_active_model().await;
            let _ = response_tx.send((models, active));
        }
        AcpCommand::SetMode { mode, response_tx } => {
            info!(mode = %mode, "Setting session mode");
            if let Some(session_id) = client_state.get_session_id().await {
                match conn
                    .set_session_mode(acp::SetSessionModeRequest::new(
                        session_id,
                        acp::SessionModeId::new(mode.clone()),
                    ))
                    .await
                {
                    Ok(_) => {
                        info!("Mode changed successfully");
                        client_state.set_active_mode(Some(mode.clone())).await;
                        let _ = response_tx.send(Ok(()));
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to set mode");
                        let _ = response_tx.send(Err(anyhow!("Failed to set mode: {}", e)));
                    }
                }
            } else {
                let _ = response_tx.send(Err(anyhow!("No active session")));
            }
        }
        AcpCommand::GetModes { response_tx } => {
            let modes = client_state.get_available_modes().await;
            let active = client_state.get_active_mode().await;
            let _ = response_tx.send((modes, active));
        }
        AcpCommand::SetConfigOption {
            config_id,
            value,
            response_tx,
        } => {
            info!(config_id = %config_id, value = %value, "Setting session config option");
            if let Some(session_id) = client_state.get_session_id().await {
                match conn
                    .set_session_config_option(acp::SetSessionConfigOptionRequest::new(
                        session_id,
                        acp::SessionConfigId::new(config_id.clone()),
                        acp::SessionConfigValueId::new(value),
                    ))
                    .await
                {
                    Ok(response) => {
                        client_state
                            .set_available_config_options(map_config_options(
                                &response.config_options,
                            ))
                            .await;
                        info!(config_id = %config_id, "Config option changed successfully");
                        let _ = response_tx.send(Ok(()));
                    }
                    Err(e) => {
                        error!(error = %e, config_id = %config_id, "Failed to set config option");
                        let _ = response_tx.send(Err(anyhow!(
                            "Failed to set config option '{}': {}",
                            config_id,
                            e
                        )));
                    }
                }
            } else {
                let _ = response_tx.send(Err(anyhow!("No active session")));
            }
        }
        AcpCommand::GetConfigOptions { response_tx } => {
            let options = client_state.get_available_config_options().await;
            let _ = response_tx.send(options);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_output_marks_truncation() {
        let mut output = CapturedOutput::default();
        output.push(&vec![b'a'; STARTUP_OUTPUT_LIMIT + 10]);

        assert_eq!(output.bytes.len(), STARTUP_OUTPUT_LIMIT);
        assert!(output.truncated);
    }

    #[test]
    fn startup_failure_message_includes_command_and_outputs() {
        let diagnostics = StartupDiagnostics::new(
            "kimi".to_string(),
            vec![
                "--stdio".to_string(),
                "--model".to_string(),
                "sonnet".to_string(),
            ],
        );

        diagnostics
            .stdout
            .lock()
            .expect("stdout lock poisoned")
            .push(b"usage: kimi --stdio\n");
        diagnostics
            .stderr
            .lock()
            .expect("stderr lock poisoned")
            .push(b"binary not configured\n");

        let message = diagnostics.format_failure("initializing the ACP session", "broken pipe");

        assert!(message.contains("Agent startup failed while initializing the ACP session"));
        assert!(message.contains("Command: kimi --stdio --model sonnet"));
        assert!(message.contains("stdout:\nusage: kimi --stdio"));
        assert!(message.contains("stderr:\nbinary not configured"));
    }
}

/// Initialize the connection and create a session
async fn initialize_and_create_session(
    conn: &acp::ClientSideConnection,
    client_state: Arc<AcpClientState>,
) -> Result<()> {
    use acp::Agent as _;

    info!("Sending initialization request");

    // Initialize the connection
    conn.initialize(
        acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_info(
            acp::Implementation::new("acpconnector", env!("CARGO_PKG_VERSION"))
                .title("ACP Connector"),
        ),
    )
    .await
    .context("Failed to initialize")?;

    info!("Initialization successful, creating session");

    // Create a new session with optional MCP servers
    let working_dir = client_state.get_working_dir().await;
    let mut req = acp::NewSessionRequest::new(working_dir);

    // Inject MCP server URL if configured
    if let Some(servers) = client_state.mcp_servers() {
        req = req.mcp_servers(servers);
    }

    let response = conn
        .new_session(req)
        .await
        .context("Failed to create session")?;

    // Update state
    apply_session_response(&client_state, &response).await;

    info!(session_id = %response.session_id, "Session created successfully");

    // Send ready message to user
    client_state
        .send_to_user("Agent connected and ready!".to_string())
        .await;

    Ok(())
}

/// ACP Client implementation that handles incoming agent requests
#[derive(Clone)]
struct AcpClient {
    state: Arc<AcpClientState>,
    /// Whether we're currently streaming a message (waiting to send StreamEnd)
    streaming: Arc<AtomicBool>,
    /// 最后收到 chunk 的时间（用于超时检测）
    last_chunk_time: Arc<Mutex<Instant>>,
    /// 流式消息超时时间（60秒）
    stream_timeout: Duration,
}

impl AcpClient {
    fn new(state: Arc<AcpClientState>) -> Self {
        Self {
            state,
            streaming: Arc::new(AtomicBool::new(false)),
            last_chunk_time: Arc::new(Mutex::new(Instant::now())),
            stream_timeout: Duration::from_secs(60),
        }
    }

    /// Send a stream chunk to the user
    async fn send_stream_chunk(&self, text: String) {
        let msg = OutgoingMessage {
            content: OutgoingContent::StreamChunk(text),
        };
        if self.state.outgoing_tx.send(msg).await.is_err() {
            error!("Failed to send stream chunk: receiver dropped");
        }
    }

    /// Send stream end signal to the user
    async fn send_stream_end(&self) {
        let msg = OutgoingMessage {
            content: OutgoingContent::StreamEnd,
        };
        if self.state.outgoing_tx.send(msg).await.is_err() {
            error!("Failed to send stream end: receiver dropped");
        }
    }

    /// End streaming if currently active
    async fn end_streaming_if_active(&self) {
        if self.streaming.swap(false, Ordering::SeqCst) {
            self.send_stream_end().await;
        }
    }

    /// 检查流式是否超时，如果超时则发送 StreamEnd
    async fn check_stream_timeout(&self) {
        if self.streaming.load(Ordering::SeqCst) {
            let last_time = *self.last_chunk_time.lock().await;
            if last_time.elapsed() > self.stream_timeout {
                warn!("Stream timeout detected, forcing StreamEnd");
                self.end_streaming_if_active().await;
            }
        }
    }

    /// 更新最后 chunk 时间
    async fn update_chunk_time(&self) {
        *self.last_chunk_time.lock().await = Instant::now();
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Client for AcpClient {
    /// Handle permission requests from the agent - auto-approve
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        debug!(tool_call = ?args.tool_call, "Permission requested");
        // Only show the message if show_auto_approved is enabled
        if self.state.show_auto_approved() {
            self.state
                .send_to_user(format!(
                    "[Auto-approved tool: {}]",
                    args.tool_call.fields.title.as_deref().unwrap_or("unknown")
                ))
                .await;
        }
        // Auto-approve by selecting the first option (typically 'allow')
        let option_id = args
            .options
            .first()
            .map(|o| o.option_id.clone())
            .unwrap_or_else(|| acp::PermissionOptionId::new("allow"));
        Ok(acp::RequestPermissionResponse::new(
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(option_id)),
        ))
    }

    /// Handle session notifications (agent output)
    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        match args.update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                let text = extract_content_text(&chunk.content);
                if let Some(text) = text {
                    self.streaming.store(true, Ordering::SeqCst);
                    self.update_chunk_time().await;
                    self.send_stream_chunk(text).await;
                }
            }
            acp::SessionUpdate::AgentThoughtChunk(chunk) => {
                // Only show thinking chunks if enabled for this bot
                if self.state.show_thinking() {
                    let text = extract_content_text(&chunk.content);
                    if let Some(text) = text {
                        self.streaming.store(true, Ordering::SeqCst);
                        self.update_chunk_time().await;
                        self.send_stream_chunk(text).await;
                    }
                }
            }
            acp::SessionUpdate::ToolCallUpdate(update) => {
                // End streaming when tool call starts/updates
                self.end_streaming_if_active().await;
                if let Some(status) = &update.fields.status {
                    match status {
                        acp::ToolCallStatus::InProgress => {
                            debug!(tool_call_id = %update.tool_call_id, "Tool call started");
                        }
                        acp::ToolCallStatus::Completed => {
                            debug!("Tool call completed");
                        }
                        acp::ToolCallStatus::Failed => {
                            if let Some(raw_output) = &update.fields.raw_output {
                                self.state
                                    .send_error(format!("Tool failed: {}", raw_output))
                                    .await;
                            }
                        }
                        _ => {}
                    }
                }
            }
            acp::SessionUpdate::Plan(plan) => {
                self.end_streaming_if_active().await;
                debug!(has_entries = !plan.entries.is_empty(), "Agent plan update");
            }
            acp::SessionUpdate::UsageUpdate(_) => {
                // Usage tracking - informational only
                // Note: This often comes after message is complete, don't end streaming here
            }
            acp::SessionUpdate::CurrentModeUpdate(mode) => {
                debug!(mode = ?mode, "Mode update");
                self.state
                    .set_active_mode(Some(mode.current_mode_id.to_string()))
                    .await;
            }
            acp::SessionUpdate::AvailableCommandsUpdate(cmds) => {
                debug!(count = cmds.available_commands.len(), "Commands update");
            }
            acp::SessionUpdate::SessionInfoUpdate(_) => {
                // Session info update
            }
            acp::SessionUpdate::ConfigOptionUpdate(update) => {
                self.state
                    .set_available_config_options(map_config_options(&update.config_options))
                    .await;
                debug!(count = update.config_options.len(), "Config option update");
            }
            update => {
                self.end_streaming_if_active().await;
                debug!(?update, "Unhandled session update");
            }
        }
        Ok(())
    }

    /// File system operations - not supported
    async fn write_text_file(
        &self,
        _args: acp::WriteTextFileRequest,
    ) -> acp::Result<acp::WriteTextFileResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn read_text_file(
        &self,
        _args: acp::ReadTextFileRequest,
    ) -> acp::Result<acp::ReadTextFileResponse> {
        Err(acp::Error::method_not_found())
    }

    /// Terminal operations - not supported
    async fn create_terminal(
        &self,
        _args: acp::CreateTerminalRequest,
    ) -> acp::Result<acp::CreateTerminalResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn terminal_output(
        &self,
        _args: acp::TerminalOutputRequest,
    ) -> acp::Result<acp::TerminalOutputResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn release_terminal(
        &self,
        _args: acp::ReleaseTerminalRequest,
    ) -> acp::Result<acp::ReleaseTerminalResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn wait_for_terminal_exit(
        &self,
        _args: acp::WaitForTerminalExitRequest,
    ) -> acp::Result<acp::WaitForTerminalExitResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn kill_terminal(
        &self,
        _args: acp::KillTerminalRequest,
    ) -> acp::Result<acp::KillTerminalResponse> {
        Err(acp::Error::method_not_found())
    }

    /// Extension methods - not supported
    async fn ext_method(&self, _args: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_notification(&self, _args: acp::ExtNotification) -> acp::Result<()> {
        // Extension notifications are often informational - just log and ignore
        Ok(())
    }
}

/// Extract text content from a content block
fn extract_content_text(content: &acp::ContentBlock) -> Option<String> {
    match content {
        acp::ContentBlock::Text(text) => Some(text.text.clone()),
        acp::ContentBlock::Image(_) => Some("<image>".to_string()),
        acp::ContentBlock::Audio(_) => Some("<audio>".to_string()),
        acp::ContentBlock::ResourceLink(link) => Some(format!("[Resource: {}]", link.uri)),
        acp::ContentBlock::Resource(res) => match &res.resource {
            acp::EmbeddedResourceResource::TextResourceContents(contents) => {
                Some(format!("[Resource: {}]", contents.uri))
            }
            acp::EmbeddedResourceResource::BlobResourceContents(_) => {
                Some("[Resource: binary data]".to_string())
            }
            _ => Some("[Resource]".to_string()),
        },
        _ => None,
    }
}

/// Send a prompt to the ACP agent
///
/// # Arguments
/// * `state` - The ACP client state
/// * `text` - The text to send
pub async fn send_prompt(state: &Arc<AcpClientState>, text: String) -> Result<()> {
    let handle = match state.get_connection().await {
        Some(h) => h,
        None => {
            state.send_error("Agent not connected".to_string()).await;
            return Err(anyhow!("ACP connection not available"));
        }
    };

    // Send the prompt to the ACP client task
    if handle.prompt_tx.send(text).await.is_err() {
        state.send_error("Failed to send prompt".to_string()).await;
        return Err(anyhow!("Failed to send prompt to channel"));
    }

    Ok(())
}

/// Send a command to create a new session
pub async fn send_new_session_command(state: &Arc<AcpClientState>) -> Result<String> {
    let (response_tx, response_rx) = oneshot::channel();
    let cmd = AcpCommand::NewSession { response_tx };

    if state.get_command_tx().send(cmd).await.is_err() {
        return Err(anyhow!("Failed to send command: channel closed"));
    }

    match response_rx.await {
        Ok(result) => result,
        Err(_) => Err(anyhow!("Command response channel closed")),
    }
}

/// Send a command to set the session model
pub async fn send_set_model_command(state: &Arc<AcpClientState>, model: String) -> Result<()> {
    let (response_tx, response_rx) = oneshot::channel();
    let cmd = AcpCommand::SetModel { model, response_tx };

    if state.get_command_tx().send(cmd).await.is_err() {
        return Err(anyhow!("Failed to send command: channel closed"));
    }

    match response_rx.await {
        Ok(result) => result,
        Err(_) => Err(anyhow!("Command response channel closed")),
    }
}

/// Send a command to get available models
pub async fn send_get_models_command(
    state: &Arc<AcpClientState>,
) -> Result<(Vec<String>, Option<String>)> {
    let (response_tx, response_rx) = oneshot::channel();
    let cmd = AcpCommand::GetModels { response_tx };

    if state.get_command_tx().send(cmd).await.is_err() {
        return Err(anyhow!("Failed to send command: channel closed"));
    }

    match response_rx.await {
        Ok(result) => Ok(result),
        Err(_) => Err(anyhow!("Command response channel closed")),
    }
}

/// Send a command to set the session mode
pub async fn send_set_mode_command(state: &Arc<AcpClientState>, mode: String) -> Result<()> {
    let (response_tx, response_rx) = oneshot::channel();
    let cmd = AcpCommand::SetMode { mode, response_tx };

    if state.get_command_tx().send(cmd).await.is_err() {
        return Err(anyhow!("Failed to send command: channel closed"));
    }

    match response_rx.await {
        Ok(result) => result,
        Err(_) => Err(anyhow!("Command response channel closed")),
    }
}

/// Send a command to get available modes
pub async fn send_get_modes_command(
    state: &Arc<AcpClientState>,
) -> Result<(Vec<String>, Option<String>)> {
    let (response_tx, response_rx) = oneshot::channel();
    let cmd = AcpCommand::GetModes { response_tx };

    if state.get_command_tx().send(cmd).await.is_err() {
        return Err(anyhow!("Failed to send command: channel closed"));
    }

    match response_rx.await {
        Ok(result) => Ok(result),
        Err(_) => Err(anyhow!("Command response channel closed")),
    }
}

/// Send a command to set an ACP config option
pub async fn send_set_config_option_command(
    state: &Arc<AcpClientState>,
    config_id: String,
    value: String,
) -> Result<()> {
    let (response_tx, response_rx) = oneshot::channel();
    let cmd = AcpCommand::SetConfigOption {
        config_id,
        value,
        response_tx,
    };

    if state.get_command_tx().send(cmd).await.is_err() {
        return Err(anyhow!("Failed to send command: channel closed"));
    }

    match response_rx.await {
        Ok(result) => result,
        Err(_) => Err(anyhow!("Command response channel closed")),
    }
}

/// Send a command to get ACP config options
pub async fn send_get_config_options_command(
    state: &Arc<AcpClientState>,
) -> Result<Vec<SessionConfigOptionState>> {
    let (response_tx, response_rx) = oneshot::channel();
    let cmd = AcpCommand::GetConfigOptions { response_tx };

    if state.get_command_tx().send(cmd).await.is_err() {
        return Err(anyhow!("Failed to send command: channel closed"));
    }

    match response_rx.await {
        Ok(result) => Ok(result),
        Err(_) => Err(anyhow!("Command response channel closed")),
    }
}

/// Send a command to change the working directory
pub async fn send_change_directory_command(
    state: &Arc<AcpClientState>,
    path: PathBuf,
) -> Result<String> {
    let (response_tx, response_rx) = oneshot::channel();
    let cmd = AcpCommand::ChangeDirectory { path, response_tx };

    if state.get_command_tx().send(cmd).await.is_err() {
        return Err(anyhow!("Failed to send command: channel closed"));
    }

    match response_rx.await {
        Ok(result) => result,
        Err(_) => Err(anyhow!("Command response channel closed")),
    }
}
