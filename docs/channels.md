# Channel Integrations

MyLobster supports 8 messaging platform integrations through a unified channel plugin architecture. Each channel normalizes inbound messages into a common format and formats outbound responses for the target platform.

## Architecture

```
Inbound messages                    Outbound messages
    │                                     ▲
    ▼                                     │
┌──────────────────────────────────────────────┐
│              ChannelManager                   │
│  (registers, starts, stops all channels)      │
└──────┬────┬────┬────┬────┬────┬────┬────┬────┘
       │    │    │    │    │    │    │    │
       ▼    ▼    ▼    ▼    ▼    ▼    ▼    ▼
      TG  Disc Slack  WA  Sig  iMsg  IRC Plugin
```

## ChannelPlugin Trait

All channels implement the `ChannelPlugin` trait (`src/channels/plugin.rs`):

```rust
#[async_trait]
pub trait ChannelPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn meta(&self) -> ChannelMeta;
    fn capabilities(&self) -> Vec<ChannelCapability>;
    async fn start_account(&mut self, state: Arc<GatewayState>) -> Result<()>;
    async fn stop_account(&mut self) -> Result<()>;
    async fn send_message(&self, to: &str, message: &str) -> Result<()>;
}
```

## Capabilities

Each channel declares its supported capabilities:

| Capability | TG | Discord | Slack | WA | Signal | iMessage |
|-----------|:--:|:-------:|:-----:|:--:|:------:|:--------:|
| SendText | x | x | x | x | x | x |
| ReceiveText | x | x | x | x | x | x |
| SendMedia | x | x | x | x | x | x |
| ReceiveMedia | x | x | x | x | x | x |
| Reactions | x | x | x | x | | |
| Groups | x | x | x | x | x | x |
| Threads | x | x | x | | | |
| ReadReceipts | x | x | | x | | x |
| TypingIndicators | x | x | x | | | |
| EditMessage | x | x | x | | | |
| DeleteMessage | x | x | x | | | |
| Voice | x | x | x | | x | |
| Stickers | x | x | | | x | |
| Polls | x | x | | | | |

## Message Normalization (`src/channels/normalize.rs`)

All inbound messages are normalized to a common `NormalizedMessage` format:

```rust
pub struct NormalizedMessage {
    pub id: String,
    pub channel: String,          // "telegram", "discord", etc.
    pub account_id: String,
    pub chat_id: String,
    pub chat_name: Option<String>,
    pub chat_type: ChatType,      // Dm, Group, or Thread
    pub sender: NormalizedSender,
    pub text: Option<String>,
    pub attachments: Vec<NormalizedAttachment>,
    pub reply_to_id: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub raw: serde_json::Value,   // Original platform payload
}
```

Outbound messages use `NormalizedOutbound` and are formatted for each platform. Helper functions `strip_markdown()` and `markdown_to_platform()` handle format conversion.

## Channel Implementations

### Telegram (`src/channels/telegram.rs`)

- **Library**: teloxide 0.13
- **Connection**: Long-polling or webhook
- **Config key**: `channels.telegram.default_account.bot_token`
- **Env var**: `TELEGRAM_BOT_TOKEN`
- **Full capabilities**: all 14

### Discord (`src/channels/discord.rs`)

- **Library**: serenity 0.12
- **Connection**: Gateway (WebSocket)
- **Config key**: `channels.discord.default_account.token`
- **Env var**: `DISCORD_BOT_TOKEN`
- **Full capabilities**: all 14

### Slack (`src/channels/slack.rs`)

- **Library**: slack-morphism 2
- **Connection**: Socket Mode or Events API webhook
- **Config keys**: `channels.slack.default_account.bot_token`, `.app_token`
- **Env vars**: `SLACK_BOT_TOKEN`, `SLACK_APP_TOKEN`
- **Capabilities**: 10 (excludes Voice, Stickers, Polls, ReadReceipts)

### WhatsApp (`src/channels/whatsapp.rs`)

- **API**: WhatsApp Business API (or compatible bridge like Baileys)
- **Config key**: `channels.whatsapp.default_account`
- **Env var**: `WHATSAPP_API_TOKEN`
- **Capabilities**: 8 (excludes EditMessage, DeleteMessage, Stickers, Polls, Threads, TypingIndicators)

### Signal (`src/channels/signal.rs`)

- **Connection**: Signal CLI or signal-cli REST API
- Stub implementation

### iMessage (`src/channels/imessage.rs`)

- **Platform**: macOS only (AppleScript / Shortcuts bridge)
- Stub implementation

### Plugin Channels (`src/channels/plugin.rs`)

Custom channels can be registered via the plugin system by implementing the `ChannelPlugin` trait.

## ChannelManager (`src/channels/mod.rs`)

The `ChannelManager` orchestrates all channel instances:

```rust
let mut manager = ChannelManager::new(&config);
manager.start_all(gateway_state.clone()).await?;

// Send a message through a specific channel
send_message(&config, "telegram", "123456789", "Hello!").await?;

// Check channel health
let status = manager.get_status();

manager.stop_all().await?;
```

## Configuration

```json
{
  "channels": {
    "telegram": {
      "enabled": true,
      "defaultAccount": {
        "botToken": "123456:ABC-DEF..."
      }
    },
    "discord": {
      "enabled": true,
      "defaultAccount": {
        "token": "..."
      }
    },
    "slack": {
      "enabled": true,
      "defaultAccount": {
        "botToken": "xoxb-...",
        "appToken": "xapp-..."
      }
    }
  }
}
```

Each channel supports multi-account configuration, action policies (allow/deny lists), DM policies, and allow-from lists for restricting which users can interact with the agent.
