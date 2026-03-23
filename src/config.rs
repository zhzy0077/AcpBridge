//! Configuration parsing for ACP Connector

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Root configuration structure
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub mcp_server: Option<McpServerConfig>,
    /// Bot definitions (top-level, reusable)
    pub bots: Vec<BotConfig>,
    /// Channel definitions (top-level, references bots by name)
    pub channels: Vec<ChannelConfig>,
}

/// Configuration for a single bot (top-level entity)
///
/// A Bot defines an AI agent identity: what command to run, what instructions
/// to give it, what model to use, etc. Bots are independent of channels and
/// can be referenced by multiple channels.
#[derive(Debug, Clone, Deserialize)]
pub struct BotConfig {
    /// Unique name for this bot (used as identifier)
    pub name: String,
    /// Human-readable description
    pub description: Option<String>,
    /// Agent process configuration
    pub agent: AgentConfig,
    /// Working directory for the agent
    pub working_dir: Option<PathBuf>,
    /// Model to use
    pub model: Option<String>,
    /// Mode to use
    pub mode: Option<String>,
    /// ACP session configuration options to apply (for example: reasoning_effort)
    #[serde(default)]
    pub config_options: BTreeMap<String, String>,
    /// Instructions to prepend to the first user message
    pub instructions: Option<String>,
    /// Whether to show agent thinking chunks to users
    #[serde(default = "default_show_thinking")]
    pub show_thinking: bool,
    /// Whether to show "[Auto-approved tool]" messages to users
    #[serde(default = "default_show_auto_approved")]
    pub show_auto_approved: bool,
}

fn default_show_thinking() -> bool {
    true
}

fn default_show_auto_approved() -> bool {
    false
}

/// Configuration for a channel (top-level entity)
///
/// A Channel is a connection to a messaging platform (Telegram, QQ, Lark).
/// It references one or more Bots by name. Users can switch between bots
/// within the same channel using the /bot command.
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelConfig {
    /// Unique name for this channel
    pub name: String,
    /// Platform-specific connection configuration
    #[serde(flatten)]
    pub platform: PlatformConfig,
    /// Default bot to use for new chats
    pub default_bot: Option<String>,
    /// List of bot names available on this channel
    pub bots: Vec<String>,
    /// If true, group-chat text is only forwarded when the bot is mentioned
    #[serde(default)]
    pub mention_only: bool,
}

/// Platform-specific configuration (enum, one variant per platform)
///
/// Uses untagged enum for YAML deserialization so that the platform
/// type is inferred from which key is present:
/// ```yaml
/// # This becomes PlatformConfig::Telegram { ... }
/// telegram:
///   token: "xxx"
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum PlatformConfig {
    Telegram { telegram: TelegramConfig },
    Qq { qq: QqConfig },
    Lark { lark: LarkConfig },
    Wechat { wechat: WeChatConfig },
    Discord { discord: DiscordConfig },
}

/// MCP Server configuration
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    pub port: u16,
}

/// Agent process configuration
#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    /// The command to run (e.g., "claude-code", "gemini")
    pub command: String,
    /// Optional arguments to pass to the command
    #[serde(default)]
    pub args: Vec<String>,
}

/// Telegram-specific configuration
#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    pub token: String,
}

/// QQ Bot-specific configuration
#[derive(Debug, Clone, Deserialize)]
pub struct QqConfig {
    pub app_id: String,
    pub client_secret: String,
}

/// Lark/Feishu Bot-specific configuration
#[derive(Debug, Clone, Deserialize)]
pub struct LarkConfig {
    pub app_id: String,
    pub app_secret: String,
    #[serde(default = "default_lark_base_url")]
    pub base_url: String,
}

fn default_lark_base_url() -> String {
    "https://open.feishu.cn".to_string()
}

/// WeChat iLink Bot-specific configuration
#[derive(Debug, Clone, Deserialize)]
pub struct WeChatConfig {
    pub bot_token: String,
    #[serde(default = "default_wechat_base_url")]
    pub base_url: String,
}

fn default_wechat_base_url() -> String {
    "https://ilinkai.weixin.qq.com".to_string()
}

/// Discord Bot-specific configuration
#[derive(Debug, Clone, Deserialize)]
pub struct DiscordConfig {
    pub bot_token: String,
    /// Optional: restrict to specific guild (server) IDs
    pub guild_ids: Option<Vec<String>>,
}

impl Config {
    /// Load configuration from a YAML file
    pub fn from_file(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        let content = std::fs::read_to_string(&path)?;
        let config: Config = serde_yaml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Validate the configuration: all channel bot references must exist
    fn validate(&self) -> anyhow::Result<()> {
        let bot_names: std::collections::HashSet<&str> =
            self.bots.iter().map(|b| b.name.as_str()).collect();

        for channel in &self.channels {
            // Check that all referenced bots exist
            for bot_name in &channel.bots {
                if !bot_names.contains(bot_name.as_str()) {
                    anyhow::bail!(
                        "Channel '{}' references unknown bot '{}'",
                        channel.name,
                        bot_name
                    );
                }
            }
            // Check that default_bot exists in the channel's bot list
            if let Some(ref default) = channel.default_bot
                && !channel.bots.contains(default)
            {
                anyhow::bail!(
                    "Channel '{}' default_bot '{}' is not in its bots list",
                    channel.name,
                    default
                );
            }
            // Channel must have at least one bot
            if channel.bots.is_empty() {
                anyhow::bail!("Channel '{}' has no bots configured", channel.name);
            }
        }
        Ok(())
    }

    /// Find a bot config by name
    pub fn get_bot(&self, name: &str) -> Option<&BotConfig> {
        self.bots.iter().find(|b| b.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_new_config_format() {
        let yaml = r#"
bots:
  - name: test-bot
    agent:
      command: claude-code
      args: ["--acp"]
    working_dir: /home/user/projects

channels:
  - name: test-channel
    telegram:
      token: "123456:ABC-DEF"
    default_bot: test-bot
    bots: [test-bot]
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.bots.len(), 1);
        assert_eq!(config.bots[0].name, "test-bot");
        assert_eq!(config.bots[0].agent.command, "claude-code");
        assert!(config.bots[0].show_thinking); // default true
        assert_eq!(config.channels.len(), 1);
        assert_eq!(config.channels[0].name, "test-channel");
        assert_eq!(config.channels[0].bots, vec!["test-bot"]);
        assert_eq!(config.channels[0].default_bot, Some("test-bot".to_string()));
    }

    #[test]
    fn test_parse_multi_bot_channel() {
        let yaml = r#"
bots:
  - name: planner
    agent:
      command: claude-code
      args: ["--acp"]
    model: claude-sonnet
    instructions: "You are a planner"

  - name: executor
    agent:
      command: gemini
      args: ["--acp"]
    model: gemini-pro

channels:
  - name: my-lark
    lark:
      app_id: "xxx"
      app_secret: "yyy"
    default_bot: planner
    bots: [planner, executor]
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.bots.len(), 2);
        assert_eq!(config.channels[0].bots.len(), 2);
        assert!(!config.channels[0].mention_only);
    }

    #[test]
    fn test_bot_reused_across_channels() {
        let yaml = r#"
bots:
  - name: shared-bot
    agent:
      command: claude-code
      args: ["--acp"]

channels:
  - name: lark-channel
    lark:
      app_id: "xxx"
      app_secret: "yyy"
    bots: [shared-bot]

  - name: tg-channel
    telegram:
      token: "xxx"
    bots: [shared-bot]
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.channels.len(), 2);
        // Both channels reference the same bot
        assert_eq!(config.channels[0].bots[0], "shared-bot");
        assert_eq!(config.channels[1].bots[0], "shared-bot");
    }

    #[test]
    fn test_validate_unknown_bot_reference() {
        let yaml = r#"
bots:
  - name: real-bot
    agent:
      command: claude-code
channels:
  - name: ch
    telegram:
      token: "xxx"
    bots: [nonexistent-bot]
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_default_bot_not_in_list() {
        let yaml = r#"
bots:
  - name: bot-a
    agent:
      command: claude-code
  - name: bot-b
    agent:
      command: gemini
channels:
  - name: ch
    telegram:
      token: "xxx"
    default_bot: bot-b
    bots: [bot-a]
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_show_thinking_default() {
        let yaml = r#"
bots:
  - name: bot
    agent:
      command: claude-code
channels:
  - name: ch
    telegram:
      token: "xxx"
    bots: [bot]
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.bots[0].show_thinking);
    }

    #[test]
    fn test_show_thinking_false() {
        let yaml = r#"
bots:
  - name: bot
    agent:
      command: claude-code
    show_thinking: false
channels:
  - name: ch
    telegram:
      token: "xxx"
    bots: [bot]
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(!config.bots[0].show_thinking);
    }

    #[test]
    fn test_parse_config_options() {
        let yaml = r#"
bots:
  - name: bot
    agent:
      command: copilot
      args: ["--acp"]
    config_options:
      reasoning_effort: high
      custom_selector: enabled
channels:
  - name: ch
    telegram:
      token: "xxx"
    bots: [bot]
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            config.bots[0].config_options.get("reasoning_effort"),
            Some(&"high".to_string())
        );
        assert_eq!(
            config.bots[0].config_options.get("custom_selector"),
            Some(&"enabled".to_string())
        );
    }

    #[test]
    fn test_parse_mention_only() {
        let yaml = r#"
bots:
  - name: bot
    agent:
      command: claude-code
channels:
  - name: ch
    telegram:
      token: "xxx"
    bots: [bot]
    mention_only: true
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.channels[0].mention_only);
    }

    #[test]
    fn test_show_auto_approved_default() {
        let yaml = r#"
bots:
  - name: bot
    agent:
      command: claude-code
channels:
  - name: ch
    telegram:
      token: "xxx"
    bots: [bot]
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(!config.bots[0].show_auto_approved); // default false
    }

    #[test]
    fn test_show_auto_approved_false() {
        let yaml = r#"
bots:
  - name: bot
    agent:
      command: claude-code
    show_auto_approved: false
channels:
  - name: ch
    telegram:
      token: "xxx"
    bots: [bot]
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(!config.bots[0].show_auto_approved);
    }
}
