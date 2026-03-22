//! MCP Server - Standard Model Context Protocol implementation.

use crate::channel::ChatId;
use crate::orchestrator::Orchestrator;
use axum::{
    Router,
    extract::{Path, Request, State},
    http::{StatusCode, request::Parts},
    response::{IntoResponse, Response},
    routing::post,
};
use futures::{FutureExt, future::BoxFuture};
use rmcp::{
    ErrorData, RoleServer,
    handler::server::{
        ServerHandler,
        router::tool::{CallToolHandlerExt, ToolRouter},
        wrapper::Parameters,
    },
    model::{
        InitializeResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
        ServerCapabilities,
    },
    schemars::{self, JsonSchema},
    service::RequestContext,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
    },
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Clone)]
pub struct McpServerState {
    http_service: StreamableHttpService<McpService, NeverSessionManager>,
}

impl McpServerState {
    pub fn new(orchestrator: Arc<Orchestrator>) -> Self {
        let http_service = StreamableHttpService::new(
            move || Ok(McpService::new(orchestrator.clone())),
            Arc::new(NeverSessionManager::default()),
            StreamableHttpServerConfig {
                stateful_mode: false,
                json_response: true,
                ..Default::default()
            },
        );

        Self { http_service }
    }
}

#[derive(Clone, Debug)]
struct McpRequestContext {
    channel_name: String,
    bot_name: String,
    chat_id: ChatId,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
struct SearchBotsInput {}

#[derive(Debug, Deserialize, JsonSchema)]
struct MentionBotInput {
    target_bot: String,
    message: String,
}

struct McpService {
    orchestrator: Arc<Orchestrator>,
    tool_router: ToolRouter<Self>,
}

impl McpService {
    fn new(orchestrator: Arc<Orchestrator>) -> Self {
        Self {
            orchestrator,
            tool_router: Self::tool_router(),
        }
    }

    fn tool_router() -> ToolRouter<Self> {
        ToolRouter::new()
            .with_route(
                Self::search_bots_tool
                .name("search_bots")
                .description("Search for available AI agent bots in the current chat. Returns a list of bot names and descriptions that can be mentioned.")
                .parameters::<SearchBotsInput>(),
            )
            .with_route(
                Self::mention_bot_tool
                .name("mention_bot")
                .description("Mention a target bot in the chat and send a message. The target bot must be discovered via search_bots first.")
                .parameters::<MentionBotInput>(),
            )
    }

    fn search_bots_tool(
        &self,
        Parameters(_): Parameters<SearchBotsInput>,
        context: RequestContext<RoleServer>,
    ) -> BoxFuture<'_, Result<String, ErrorData>> {
        async move { self.search_bots(context).await }.boxed()
    }

    fn mention_bot_tool(
        &self,
        Parameters(params): Parameters<MentionBotInput>,
        context: RequestContext<RoleServer>,
    ) -> BoxFuture<'_, Result<String, ErrorData>> {
        async move { self.mention_bot(context, params).await }.boxed()
    }

    async fn search_bots(&self, context: RequestContext<RoleServer>) -> Result<String, ErrorData> {
        let request_context = request_context_from(&context)?;
        let bots = self
            .orchestrator
            .search_bots(
                &request_context.channel_name,
                &request_context.chat_id,
                &request_context.bot_name,
            )
            .await;

        serde_json::to_string_pretty(&bots)
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))
    }

    async fn mention_bot(
        &self,
        context: RequestContext<RoleServer>,
        params: MentionBotInput,
    ) -> Result<String, ErrorData> {
        let request_context = request_context_from(&context)?;

        if params.target_bot.is_empty() {
            return Err(ErrorData::invalid_params(
                "Missing required parameter: target_bot",
                None,
            ));
        }

        self.orchestrator
            .mention_bot(
                &request_context.channel_name,
                &request_context.chat_id,
                &request_context.bot_name,
                &params.target_bot,
                &params.message,
            )
            .await
            .map_err(|error| {
                ErrorData::internal_error(format!("Failed to mention bot: {}", error), None)
            })?;

        Ok("Message sent successfully".to_string())
    }
}

impl ServerHandler for McpService {
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        self.tool_router
            .call(rmcp::handler::server::tool::ToolCallContext::new(
                self, request, context,
            ))
            .await
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tool_router.list_all()))
    }

    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        self.tool_router.get(name).cloned()
    }

    fn get_info(&self) -> rmcp::model::ServerInfo {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
    }
}

/// Bind the MCP server listener eagerly so port conflicts fail at startup.
///
/// Returns a bound listener ready to be passed to [`serve_mcp`].
pub async fn bind_mcp_server(port: u16) -> anyhow::Result<tokio::net::TcpListener> {
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind MCP server on port {}: {}", port, e))?;
    tracing::info!(port = port, "MCP Server bound");
    Ok(listener)
}

/// Serve the MCP HTTP server on an already-bound listener.
pub async fn serve_mcp(
    listener: tokio::net::TcpListener,
    orchestrator: Arc<Orchestrator>,
) -> anyhow::Result<()> {
    let state = McpServerState::new(orchestrator);
    let app = Router::new()
        .route("/mcp/{*path}", post(handle_mcp_request))
        .with_state(state);

    axum::serve(listener, app).await?;

    Ok(())
}

async fn handle_mcp_request(
    Path(path): Path<String>,
    State(state): State<McpServerState>,
    mut request: Request,
) -> Response {
    let request_context = match parse_request_context(&path) {
        Ok(context) => context,
        Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
    };

    tracing::debug!(
        channel_name = %request_context.channel_name,
        bot_name = %request_context.bot_name,
        chat_id = %request_context.chat_id.0,
        "MCP request"
    );

    request.extensions_mut().insert(request_context);
    state.http_service.handle(request).await.into_response()
}

fn parse_request_context(path: &str) -> Result<McpRequestContext, &'static str> {
    let path_decoded = urlencoding::decode(path).unwrap_or_else(|_| path.into());
    let parts: Vec<&str> = path_decoded.splitn(3, '/').collect();

    if parts.len() != 3 {
        return Err("Invalid URL path. Expected: {channel_name}/{bot_name}/{chat_id}");
    }

    Ok(McpRequestContext {
        channel_name: parts[0].to_string(),
        bot_name: parts[1].to_string(),
        chat_id: ChatId(parts[2].to_string()),
    })
}

fn request_context_from(
    context: &RequestContext<RoleServer>,
) -> Result<McpRequestContext, ErrorData> {
    let parts = context
        .extensions
        .get::<Parts>()
        .ok_or_else(|| ErrorData::invalid_request("Missing HTTP request context", None))?;

    parts
        .extensions
        .get::<McpRequestContext>()
        .cloned()
        .ok_or_else(|| ErrorData::invalid_request("Missing MCP request context", None))
}

#[cfg(test)]
mod tests {
    use super::parse_request_context;

    #[test]
    fn parses_mcp_path() {
        let context = parse_request_context("telegram/bot-a/chat-1").unwrap();
        assert_eq!(context.channel_name, "telegram");
        assert_eq!(context.bot_name, "bot-a");
        assert_eq!(context.chat_id.0, "chat-1");
    }

    #[test]
    fn decodes_url_encoded_chat_id() {
        let context = parse_request_context("telegram/bot-a/chat%3Athread").unwrap();
        assert_eq!(context.chat_id.0, "chat:thread");
    }

    #[test]
    fn rejects_invalid_path() {
        assert!(parse_request_context("telegram/bot-a").is_err());
    }
}
