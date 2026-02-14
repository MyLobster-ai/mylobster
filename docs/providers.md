# Provider Architecture

MyLobster supports 6 AI model providers through a unified `ModelProvider` trait. Providers are resolved at runtime based on model name and configuration.

## Provider Trait

All providers implement the `ModelProvider` trait defined in `src/providers/mod.rs`:

```rust
#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn chat(&self, request: ProviderRequest) -> Result<ProviderResponse>;
    async fn stream_chat(&self, request: ProviderRequest) -> Result<mpsc::Receiver<StreamEvent>>;
    fn name(&self) -> &str;
}
```

## Provider Tiers

### Tier 1: Custom Protocol

These providers have unique API formats and require dedicated request/response type definitions.

**Anthropic** (`src/providers/anthropic.rs`)
- API: Messages API at `{base_url}/v1/messages`
- Auth: `x-api-key` header + `anthropic-version` header
- Streaming: SSE with typed events (`message_start`, `content_block_delta`, `message_delta`, etc.)
- Features: Prompt caching (tracks `cache_read_input_tokens` and `cache_creation_input_tokens`)
- Default base URL: `https://api.anthropic.com`
- Env var: `ANTHROPIC_API_KEY`

**Ollama** (`src/providers/ollama.rs`)
- API: Native Ollama at `{base_url}/api/chat` (not OpenAI-compatible `/v1/chat/completions`)
- Auth: Optional Bearer token (Ollama typically runs unauthenticated)
- Streaming: NDJSON (one JSON object per line, not SSE)
- Features:
  - Strips `/v1` suffix from configured base URL before appending `/api/chat`
  - Sets `options.num_ctx = 65536` (Ollama default of 4096 is too small for agent use)
  - Maps `max_tokens` to `num_predict` and `temperature` to Ollama's `options` object
  - Tool calls arrive in intermediate chunks (done=false) and are forwarded as `StreamEvent::ToolCall`
  - Usage tracked via `prompt_eval_count` and `eval_count` fields
- Default base URL: `http://127.0.0.1:11434`
- Env var: `OLLAMA_API_KEY` (optional)

**Gemini** (`src/providers/gemini.rs`)
- API: Google Generative AI with camelCase JSON
- Role mapping: `assistant` -> `model`
- Content structure: `parts` array for multimodal inputs
- Default endpoint: Google's versioned API path
- Env var: `GOOGLE_API_KEY`

### Tier 2: OpenAI-Compatible

These providers use the standard OpenAI chat completions API format and delegate to the shared `openai_compat` module.

**OpenAI** (`src/providers/openai.rs`)
- Delegates to `openai_compat::openai_compat_chat()` and `openai_compat_stream_chat()`
- Default base URL: `https://api.openai.com/v1`
- Env var: `OPENAI_API_KEY`

**Groq** (`src/providers/groq.rs`)
- Identical API format to OpenAI, delegates to same `openai_compat` functions
- Default base URL: `https://api.groq.com/openai/v1`
- Env var: `GROQ_API_KEY`
- Groq provides extremely fast inference for open-source models (Llama, Mixtral)

### Tier 3: Stub

**Bedrock** (`src/providers/bedrock.rs`)
- Struct defined but `chat()` and `stream_chat()` both return errors
- Would require AWS SDK integration for Signature V4 signing

## Shared OpenAI-Compatible Base (`src/providers/openai_compat.rs`)

This module contains all shared types and logic for OpenAI-compatible providers:

**Types**: `OpenAiRequest`, `OpenAiMessage`, `OpenAiResponse`, `OpenAiChoice`, `OpenAiUsage`, `OpenAiStreamChunk`, `OpenAiStreamChoice`, `OpenAiStreamDelta`

**Functions**:
- `convert_messages()` — Convert `ProviderMessage` to `OpenAiMessage`
- `build_request()` — Build an `OpenAiRequest` from a `ProviderRequest`
- `openai_compat_chat()` — Non-streaming chat request
- `openai_compat_stream_chat()` — SSE streaming chat request

Adding a new OpenAI-compatible provider (e.g. HuggingFace, Together, Fireworks) requires only:
1. A ~40 line struct with `new()` constructor
2. `ModelProvider` impl that delegates to `openai_compat` functions
3. Provider resolution entry in `mod.rs`

## Provider Resolution

Provider detection happens in `detect_provider()` based on model name patterns:

| Pattern | Provider |
|---------|----------|
| Contains `claude` or starts with `anthropic` | `anthropic` |
| Starts with `gpt`, `o1`, `o3`, `o4` | `openai` |
| Starts with `gemini` | `google` |
| Starts with `llama-` or `mixtral-` (when groq configured) | `groq` |
| Contains `:` (tag separator, e.g. `llama3.3:latest`) | `ollama` |
| Default fallback | `anthropic` |

The Groq detection requires the `groq` provider to be explicitly configured to avoid ambiguity with Ollama model names that may also start with `llama`.

## Configuration

Providers are configured under `models.providers` in the config file:

```json
{
  "models": {
    "providers": {
      "anthropic": {
        "baseUrl": "https://api.anthropic.com",
        "apiKey": "sk-ant-..."
      },
      "ollama": {
        "baseUrl": "http://127.0.0.1:11434"
      },
      "groq": {
        "baseUrl": "https://api.groq.com/openai/v1",
        "apiKey": "gsk_..."
      }
    }
  }
}
```

Each provider config has:
- `baseUrl` — API endpoint (provider has a sensible default)
- `apiKey` — API key (can also come from env var; optional for Ollama)
- `api` — Override the API type (e.g. `ModelApi::Ollama`)
- `models` — Custom model definitions with cost, context window, headers
