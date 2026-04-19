use super::openai_request::{build_responses_input, build_tools};
use super::{EventStream, Provider};
use crate::auth::codex::CodexCredentials;
use crate::auth::oauth;
#[cfg(test)]
use crate::message::TOOL_OUTPUT_MISSING_TEXT;
use crate::message::{Message as ChatMessage, StreamEvent, ToolDefinition};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt as FuturesStreamExt};
use reqwest::header::HeaderValue;
use reqwest::{Client, StatusCode};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, LazyLock, RwLock as StdRwLock};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const OPENAI_API_BASE: &str = "https://api.openai.com/v1";
const CHATGPT_API_BASE: &str = "https://chatgpt.com/backend-api/codex";
const RESPONSES_PATH: &str = "responses";
const DEFAULT_MODEL: &str = "gpt-5.4";
const ORIGINATOR: &str = "codex_cli_rs";
const CHATGPT_INSTRUCTIONS: &str = include_str!("../prompts/openai_chatgpt.md");
const SELFDEV_SECTION_HEADER: &str = "# Self-Development Mode";

/// Maximum number of retries for transient errors
const MAX_RETRIES: u32 = 3;

/// Base delay for exponential backoff (in milliseconds)
const RETRY_BASE_DELAY_MS: u64 = 1000;
const WEBSOCKET_UPGRADE_REQUIRED_ERROR: StatusCode = StatusCode::UPGRADE_REQUIRED;
const WEBSOCKET_FALLBACK_NOTICE: &str = "falling back from websockets to https transport";
const WEBSOCKET_CONNECT_TIMEOUT_SECS: u64 = 8;
const WEBSOCKET_FIRST_EVENT_TIMEOUT_SECS: u64 = 8;
const WEBSOCKET_COMPLETION_TIMEOUT_SECS: u64 = 300;
/// Maximum age of a persistent WebSocket connection before forcing reconnect
const WEBSOCKET_PERSISTENT_MAX_AGE_SECS: u64 = 3000; // 50 min (server limit is 60 min)
/// If a persistent socket sits idle this long, reconnect before reuse instead of
/// discovering a dead socket on the next turn.
const WEBSOCKET_PERSISTENT_IDLE_RECONNECT_SECS: u64 = 90;
/// If a persistent socket has been idle for a while, send a lightweight ping
/// before reuse so we can proactively detect half-closed connections.
const WEBSOCKET_PERSISTENT_HEALTHCHECK_IDLE_SECS: u64 = 15;
const WEBSOCKET_PERSISTENT_HEALTHCHECK_TIMEOUT_MS: u64 = 1500;
/// Base websocket cooldown after a fallback in auto mode.
/// Keep this short so one flaky attempt does not pin the TUI to HTTPS for a long time.
const WEBSOCKET_MODEL_COOLDOWN_BASE_SECS: u64 = 60;
/// Maximum websocket cooldown after repeated fallback streaks.
const WEBSOCKET_MODEL_COOLDOWN_MAX_SECS: u64 = 600;
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 32_768;
static FALLBACK_TOOL_CALL_COUNTER: AtomicU64 = AtomicU64::new(1);
static RECOVERED_TEXT_WRAPPED_TOOL_CALLS: AtomicU64 = AtomicU64::new(0);
static NORMALIZED_NULL_TOOL_ARGUMENTS: AtomicU64 = AtomicU64::new(0);
static WEBSOCKET_COOLDOWNS: LazyLock<Arc<RwLock<HashMap<String, Instant>>>> =
    LazyLock::new(|| Arc::new(RwLock::new(HashMap::new())));
static WEBSOCKET_FAILURE_STREAKS: LazyLock<Arc<RwLock<HashMap<String, u32>>>> =
    LazyLock::new(|| Arc::new(RwLock::new(HashMap::new())));

#[expect(
    clippy::upper_case_acronyms,
    reason = "transport names mirror user-facing configuration values like https and websocket"
)]
#[derive(Clone, Copy)]
enum OpenAITransportMode {
    Auto,
    WebSocket,
    HTTPS,
}

impl OpenAITransportMode {
    fn from_config(raw: Option<&str>) -> Self {
        let Some(raw) = raw else {
            return Self::Auto;
        };
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Self::Auto,
            "websocket" | "ws" | "wss" => Self::WebSocket,
            "https" | "http" | "sse" => Self::HTTPS,
            other => {
                crate::logging::warn(&format!(
                    "Unknown JCODE_OPENAI_TRANSPORT '{}'; using auto. Use: auto, websocket, or https.",
                    other
                ));
                Self::Auto
            }
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::WebSocket => "websocket",
            Self::HTTPS => "https",
        }
    }
}

#[derive(Debug)]
enum OpenAIStreamFailure {
    FallbackToHttps(anyhow::Error),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for OpenAIStreamFailure {
    fn from(err: anyhow::Error) -> Self {
        Self::Other(err)
    }
}

#[expect(
    clippy::upper_case_acronyms,
    reason = "transport names mirror user-facing configuration values like https and websocket"
)]
#[derive(Clone, Copy)]
enum OpenAITransport {
    WebSocket,
    HTTPS,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenAINativeCompactionMode {
    Auto,
    Explicit,
    Off,
}

impl OpenAINativeCompactionMode {
    fn from_config(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Self::Auto,
            "explicit" | "manual" => Self::Explicit,
            "off" | "disabled" | "none" => Self::Off,
            other => {
                crate::logging::warn(&format!(
                    "Unknown OpenAI native compaction mode '{}'; using auto. Use: auto, explicit, or off.",
                    other
                ));
                Self::Auto
            }
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Explicit => "explicit",
            Self::Off => "off",
        }
    }
}

impl OpenAITransport {
    fn as_str(self) -> &'static str {
        match self {
            Self::WebSocket => "websocket",
            Self::HTTPS => "https",
        }
    }
}

/// Persistent WebSocket connection state for incremental continuation.
/// Keeps the connection alive across turns so we can use `previous_response_id`
/// to send only new items instead of the full conversation each turn.
struct PersistentWsState {
    ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    last_response_id: String,
    connected_at: Instant,
    last_activity_at: Instant,
    /// Number of messages sent in this conversation chain
    message_count: usize,
    /// Number of items we sent in the last full request (for detecting conversation changes)
    last_input_item_count: usize,
}

#[derive(Debug, Clone)]
struct PersistentWsDiagSnapshot {
    present: bool,
    connected_age_ms: Option<u128>,
    idle_age_ms: Option<u128>,
    message_count: Option<usize>,
    last_input_item_count: Option<usize>,
    previous_response_id_present: Option<bool>,
}

impl PersistentWsDiagSnapshot {
    fn absent() -> Self {
        Self {
            present: false,
            connected_age_ms: None,
            idle_age_ms: None,
            message_count: None,
            last_input_item_count: None,
            previous_response_id_present: None,
        }
    }

    fn log_fields(&self) -> String {
        if !self.present {
            return "persistent_ws=absent".to_string();
        }

        format!(
            "persistent_ws=present connected_age_ms={} idle_age_ms={} message_count={} last_input_items={} previous_response_id_present={}",
            self.connected_age_ms
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            self.idle_age_ms
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            self.message_count
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            self.last_input_item_count
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            self.previous_response_id_present
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
        )
    }
}

impl PersistentWsState {
    fn diag_snapshot(&self) -> PersistentWsDiagSnapshot {
        PersistentWsDiagSnapshot {
            present: true,
            connected_age_ms: Some(self.connected_at.elapsed().as_millis()),
            idle_age_ms: Some(self.last_activity_at.elapsed().as_millis()),
            message_count: Some(self.message_count),
            last_input_item_count: Some(self.last_input_item_count),
            previous_response_id_present: Some(!self.last_response_id.is_empty()),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct WsInputStats {
    total_items: usize,
    message_items: usize,
    function_call_items: usize,
    function_call_output_items: usize,
    other_items: usize,
}

impl WsInputStats {
    fn tool_callback_count(self) -> usize {
        self.function_call_output_items
    }

    fn log_fields(self) -> String {
        format!(
            "items={} messages={} function_calls={} tool_outputs={} other={}",
            self.total_items,
            self.message_items,
            self.function_call_items,
            self.function_call_output_items,
            self.other_items
        )
    }
}

fn summarize_ws_input(items: &[Value]) -> WsInputStats {
    let mut stats = WsInputStats::default();
    for item in items {
        stats.total_items += 1;
        match item.get("type").and_then(|value| value.as_str()) {
            Some("message") => stats.message_items += 1,
            Some("function_call") => stats.function_call_items += 1,
            Some("function_call_output") => stats.function_call_output_items += 1,
            _ => stats.other_items += 1,
        }
    }
    stats
}

fn persistent_ws_idle_needs_healthcheck(idle_for: Duration) -> bool {
    idle_for >= Duration::from_secs(WEBSOCKET_PERSISTENT_HEALTHCHECK_IDLE_SECS)
}

fn persistent_ws_idle_requires_reconnect(idle_for: Duration) -> bool {
    idle_for >= Duration::from_secs(WEBSOCKET_PERSISTENT_IDLE_RECONNECT_SECS)
}

async fn emit_connection_phase(
    tx: &mpsc::Sender<Result<StreamEvent>>,
    phase: crate::message::ConnectionPhase,
) {
    let _ = tx.send(Ok(StreamEvent::ConnectionPhase { phase })).await;
}

async fn emit_status_detail(tx: &mpsc::Sender<Result<StreamEvent>>, detail: impl Into<String>) {
    let _ = tx
        .send(Ok(StreamEvent::StatusDetail {
            detail: detail.into(),
        }))
        .await;
}

fn format_status_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs >= 3600 {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        format!("{}h {}m", hours, mins)
    } else if secs >= 60 {
        let mins = secs / 60;
        let rem_secs = secs % 60;
        format!("{}m {}s", mins, rem_secs)
    } else {
        format!("{}s", secs)
    }
}

async fn ensure_persistent_ws_is_healthy(state: &mut PersistentWsState) -> Result<bool, String> {
    let idle_for = state.last_activity_at.elapsed();
    if persistent_ws_idle_requires_reconnect(idle_for) {
        crate::logging::info(&format!(
            "Persistent WS idle for {}s; reconnecting before reuse",
            idle_for.as_secs()
        ));
        return Ok(false);
    }

    if !persistent_ws_idle_needs_healthcheck(idle_for) {
        return Ok(true);
    }

    crate::logging::info(&format!(
        "Persistent WS idle for {}ms; sending healthcheck ping before reuse",
        idle_for.as_millis()
    ));

    state
        .ws_stream
        .send(WsMessage::Ping(Vec::new()))
        .await
        .map_err(|err| format!("healthcheck ping send error: {}", err))?;

    let started_at = Instant::now();
    let timeout = Duration::from_millis(WEBSOCKET_PERSISTENT_HEALTHCHECK_TIMEOUT_MS);

    while started_at.elapsed() < timeout {
        let remaining = timeout.saturating_sub(started_at.elapsed());
        let next_item = tokio::time::timeout(remaining, state.ws_stream.next())
            .await
            .map_err(|_| {
                format!(
                    "healthcheck pong timeout after {}ms",
                    WEBSOCKET_PERSISTENT_HEALTHCHECK_TIMEOUT_MS
                )
            })?;

        match next_item {
            Some(Ok(WsMessage::Pong(_))) => {
                state.last_activity_at = Instant::now();
                crate::logging::info(&format!(
                    "Persistent WS healthcheck pong after {}ms",
                    started_at.elapsed().as_millis()
                ));
                return Ok(true);
            }
            Some(Ok(WsMessage::Ping(payload))) => {
                state
                    .ws_stream
                    .send(WsMessage::Pong(payload))
                    .await
                    .map_err(|err| format!("healthcheck pong send error: {}", err))?;
                state.last_activity_at = Instant::now();
            }
            Some(Ok(WsMessage::Close(_))) => {
                return Ok(false);
            }
            Some(Ok(other)) => {
                return Err(format!(
                    "unexpected websocket frame during healthcheck: {:?}",
                    other
                ));
            }
            Some(Err(err)) => {
                return Err(format!("healthcheck receive error: {}", err));
            }
            None => {
                return Ok(false);
            }
        }
    }

    Ok(false)
}

pub struct OpenAIProvider {
    client: Client,
    credentials: Arc<RwLock<CodexCredentials>>,
    model: Arc<RwLock<String>>,
    prompt_cache_key: Option<String>,
    prompt_cache_retention: Option<String>,
    max_output_tokens: Option<u32>,
    reasoning_effort: Arc<RwLock<Option<String>>>,
    service_tier: Arc<StdRwLock<Option<String>>>,
    native_compaction_mode: OpenAINativeCompactionMode,
    native_compaction_threshold_tokens: usize,
    transport_mode: Arc<RwLock<OpenAITransportMode>>,
    websocket_cooldowns: Arc<RwLock<HashMap<String, Instant>>>,
    websocket_failure_streaks: Arc<RwLock<HashMap<String, u32>>>,
    /// Persistent WebSocket connection for incremental continuation
    persistent_ws: Arc<Mutex<Option<PersistentWsState>>>,
}

impl OpenAIProvider {
    pub fn new(credentials: CodexCredentials) -> Self {
        // Check for model override from environment
        let mut model =
            std::env::var("JCODE_OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        if !crate::provider::known_openai_model_ids()
            .iter()
            .any(|known| known == &model)
        {
            crate::logging::info(&format!(
                "Warning: '{}' is not supported; falling back to '{}'",
                model, DEFAULT_MODEL
            ));
            model = DEFAULT_MODEL.to_string();
        }

        let prompt_cache_key = std::env::var("JCODE_OPENAI_PROMPT_CACHE_KEY")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let prompt_cache_retention = std::env::var("JCODE_OPENAI_PROMPT_CACHE_RETENTION")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let prompt_cache_retention = match prompt_cache_retention.as_deref() {
            Some("in_memory") | Some("24h") => prompt_cache_retention,
            Some(other) => {
                crate::logging::info(&format!(
                    "Warning: Unsupported JCODE_OPENAI_PROMPT_CACHE_RETENTION '{}'; expected 'in_memory' or '24h'",
                    other
                ));
                None
            }
            None => None,
        };
        let max_output_tokens = Self::load_max_output_tokens();
        let reasoning_effort = crate::config::config()
            .provider
            .openai_reasoning_effort
            .as_deref()
            .and_then(Self::normalize_reasoning_effort);
        let service_tier = Self::load_service_tier(
            crate::config::config()
                .provider
                .openai_service_tier
                .as_deref(),
        );
        let transport_mode = OpenAITransportMode::from_config(
            crate::config::config().provider.openai_transport.as_deref(),
        );
        let native_compaction_mode = OpenAINativeCompactionMode::from_config(
            &crate::config::config()
                .provider
                .openai_native_compaction_mode,
        );
        let native_compaction_threshold_tokens = crate::config::config()
            .provider
            .openai_native_compaction_threshold_tokens
            .max(1000);

        Self {
            client: crate::provider::shared_http_client(),
            credentials: Arc::new(RwLock::new(credentials)),
            model: Arc::new(RwLock::new(model)),
            prompt_cache_key,
            prompt_cache_retention,
            max_output_tokens,
            reasoning_effort: Arc::new(RwLock::new(reasoning_effort)),
            service_tier: Arc::new(StdRwLock::new(service_tier)),
            native_compaction_mode,
            native_compaction_threshold_tokens,
            transport_mode: Arc::new(RwLock::new(transport_mode)),
            websocket_cooldowns: Arc::clone(&WEBSOCKET_COOLDOWNS),
            websocket_failure_streaks: Arc::clone(&WEBSOCKET_FAILURE_STREAKS),
            persistent_ws: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn reload_credentials_now(&self) {
        if let Ok(credentials) = crate::auth::codex::load_credentials() {
            match self.credentials.try_write() {
                Ok(mut guard) => {
                    *guard = credentials;
                }
                Err(_) => {
                    crate::logging::info(
                        "OpenAI credentials were updated on disk, but the in-memory credential lock was busy; async refresh will retry",
                    );
                }
            }
        }

        self.clear_persistent_ws_try("credentials reloaded");
    }

    fn clear_persistent_ws_try(&self, reason: &str) {
        if let Ok(mut persistent_ws) = self.persistent_ws.try_lock() {
            if persistent_ws.is_some() {
                crate::logging::info(&format!("Clearing persistent OpenAI WS state: {}", reason));
            }
            *persistent_ws = None;
        }
    }

    async fn clear_persistent_ws(&self, reason: &str) {
        let mut persistent_ws = self.persistent_ws.lock().await;
        if persistent_ws.is_some() {
            crate::logging::info(&format!("Clearing persistent OpenAI WS state: {}", reason));
        }
        *persistent_ws = None;
    }

    #[cfg(test)]
    pub(crate) async fn test_access_token(&self) -> String {
        self.credentials.read().await.access_token.clone()
    }

    fn is_chatgpt_mode(credentials: &CodexCredentials) -> bool {
        !credentials.refresh_token.is_empty() || credentials.id_token.is_some()
    }

    fn chatgpt_instructions_with_selfdev(system: &str) -> String {
        if let Some(selfdev_section) = extract_selfdev_section(system) {
            format!("{}\n\n{}", CHATGPT_INSTRUCTIONS.trim_end(), selfdev_section)
        } else {
            CHATGPT_INSTRUCTIONS.to_string()
        }
    }

    fn should_prefer_websocket(model: &str) -> bool {
        !model.trim().is_empty()
    }

    fn normalize_reasoning_effort(raw: &str) -> Option<String> {
        let value = raw.trim().to_lowercase();
        if value.is_empty() {
            return None;
        }
        match value.as_str() {
            "none" | "low" | "medium" | "high" | "xhigh" => Some(value),
            other => {
                crate::logging::info(&format!(
                    "Warning: Unsupported OpenAI reasoning effort '{}'; expected none|low|medium|high|xhigh. Using 'xhigh'.",
                    other
                ));
                Some("xhigh".to_string())
            }
        }
    }

    fn native_compaction_threshold_for_context_window(
        &self,
        context_window: usize,
    ) -> Option<usize> {
        if self.native_compaction_mode != OpenAINativeCompactionMode::Auto {
            return None;
        }
        Some(
            self.native_compaction_threshold_tokens
                .max(1000)
                .min(context_window.max(1000)),
        )
    }

    fn parse_max_output_tokens(raw: Option<&str>) -> Option<u32> {
        let raw = match raw {
            Some(value) => value.trim(),
            None => return Some(DEFAULT_MAX_OUTPUT_TOKENS),
        };
        if raw.is_empty() {
            return Some(DEFAULT_MAX_OUTPUT_TOKENS);
        }
        match raw.parse::<u32>() {
            Ok(0) => None,
            Ok(value) => Some(value),
            Err(_) => {
                crate::logging::warn(&format!(
                    "Invalid JCODE_OPENAI_MAX_OUTPUT_TOKENS='{}'; using default {}",
                    raw, DEFAULT_MAX_OUTPUT_TOKENS
                ));
                Some(DEFAULT_MAX_OUTPUT_TOKENS)
            }
        }
    }

    fn normalize_service_tier(raw: &str) -> Result<Option<String>> {
        let value = raw.trim().to_ascii_lowercase();
        if value.is_empty() {
            return Ok(None);
        }

        match value.as_str() {
            "fast" | "priority" => Ok(Some("priority".to_string())),
            "flex" => Ok(Some("flex".to_string())),
            "default" | "auto" | "none" | "off" => Ok(None),
            other => anyhow::bail!(
                "Unsupported OpenAI service tier '{}'; expected priority|fast|flex|default|off",
                other
            ),
        }
    }

    fn load_service_tier(raw: Option<&str>) -> Option<String> {
        let raw = raw?;
        match Self::normalize_service_tier(raw) {
            Ok(value) => value,
            Err(err) => {
                crate::logging::warn(&format!(
                    "{}; ignoring configured service tier override",
                    err
                ));
                None
            }
        }
    }

    fn load_max_output_tokens() -> Option<u32> {
        let raw = std::env::var("JCODE_OPENAI_MAX_OUTPUT_TOKENS").ok();
        let parsed = Self::parse_max_output_tokens(raw.as_deref());
        if raw.is_some() {
            match parsed {
                Some(value) => crate::logging::info(&format!(
                    "OpenAI max_output_tokens configured to {}",
                    value
                )),
                None => crate::logging::info(
                    "OpenAI max_output_tokens disabled (JCODE_OPENAI_MAX_OUTPUT_TOKENS=0)",
                ),
            }
        }
        parsed
    }

    fn responses_url(credentials: &CodexCredentials) -> String {
        let base = if Self::is_chatgpt_mode(credentials) {
            CHATGPT_API_BASE
        } else {
            OPENAI_API_BASE
        };
        format!("{}/{}", base.trim_end_matches('/'), RESPONSES_PATH)
    }

    fn responses_ws_url(credentials: &CodexCredentials) -> String {
        let base = Self::responses_url(credentials);
        base.replace("https://", "wss://")
            .replace("http://", "ws://")
    }

    fn responses_compact_url(credentials: &CodexCredentials) -> String {
        format!("{}/compact", Self::responses_url(credentials))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "request construction threads explicit per-request OpenAI settings without hidden state"
    )]
    fn build_response_request(
        model_id: &str,
        instructions: String,
        input: &[Value],
        api_tools: &[Value],
        is_chatgpt_mode: bool,
        max_output_tokens: Option<u32>,
        reasoning_effort: Option<&str>,
        service_tier: Option<&str>,
        prompt_cache_key: Option<&str>,
        prompt_cache_retention: Option<&str>,
        native_compaction_threshold: Option<usize>,
    ) -> Value {
        let mut request = serde_json::json!({
            "model": model_id,
            "instructions": instructions,
            "input": input,
            "tools": api_tools,
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "stream": true,
            "store": false,
            "include": ["reasoning.encrypted_content"],
        });

        if !is_chatgpt_mode && let Some(max_output_tokens) = max_output_tokens {
            request["max_output_tokens"] = serde_json::json!(max_output_tokens);
        }

        if let Some(effort) = reasoning_effort {
            request["reasoning"] = serde_json::json!({ "effort": effort });
        }

        if let Some(service_tier) = service_tier {
            request["service_tier"] = serde_json::json!(service_tier);
        }

        if let Some(compact_threshold) = native_compaction_threshold {
            request["context_management"] = serde_json::json!([
                {
                    "type": "compaction",
                    "compact_threshold": compact_threshold,
                }
            ]);
        }

        if !is_chatgpt_mode {
            if let Some(key) = prompt_cache_key {
                request["prompt_cache_key"] = serde_json::json!(key);
            }
            if let Some(retention) = prompt_cache_retention {
                request["prompt_cache_retention"] = serde_json::json!(retention);
            }
        }

        request
    }

    async fn model_id(&self) -> String {
        let current = self.model.read().await.clone();
        let availability = crate::provider::model_availability_for_account(&current);

        match availability.state {
            crate::provider::AccountModelAvailabilityState::Unavailable => {
                if let Some(detail) = availability.reason {
                    crate::logging::info(&format!(
                        "Model '{}' currently unavailable ({}); selecting fallback",
                        current, detail
                    ));
                }
                if let Some(fallback) = crate::provider::get_best_available_openai_model()
                    && fallback != current
                {
                    crate::logging::info(&format!(
                        "Model '{}' not available for account; falling back to '{}'",
                        current, fallback
                    ));
                    {
                        let mut w = self.model.write().await;
                        *w = fallback.clone();
                    }
                    self.clear_persistent_ws(
                        "automatic OpenAI model fallback changed the response chain",
                    )
                    .await;
                    return fallback;
                }
            }
            crate::provider::AccountModelAvailabilityState::Unknown => {
                if crate::provider::should_refresh_openai_model_catalog()
                    && crate::provider::begin_openai_model_catalog_refresh()
                {
                    let creds = self.credentials.read().await;
                    let token = creds.access_token.clone();
                    drop(creds);
                    crate::provider::refresh_openai_model_catalog_in_background(
                        token,
                        "openai-request-setup",
                    );
                }
            }
            crate::provider::AccountModelAvailabilityState::Available => {}
        }

        current.strip_suffix("[1m]").unwrap_or(&current).to_string()
    }

    fn diagnostic_persistent_ws_summary(&self) -> String {
        match self.persistent_ws.try_lock() {
            Ok(guard) => guard
                .as_ref()
                .map(|state| state.diag_snapshot().log_fields())
                .unwrap_or_else(|| PersistentWsDiagSnapshot::absent().log_fields()),
            Err(_) => "persistent_ws=busy".to_string(),
        }
    }

    pub fn diagnostic_state_summary(&self) -> String {
        let transport_mode = self
            .transport_mode
            .try_read()
            .map(|mode| mode.as_str().to_string())
            .unwrap_or_else(|_| "busy".to_string());
        format!(
            "transport_mode={} {}",
            transport_mode,
            self.diagnostic_persistent_ws_summary()
        )
    }
}

fn extract_selfdev_section(system: &str) -> Option<&str> {
    let start = system.find(SELFDEV_SECTION_HEADER)?;
    let end = if let Some(rel_end) = system[start + 1..].find("\n# ") {
        start + 1 + rel_end
    } else {
        system.len()
    };
    let section = system[start..end].trim();
    if section.is_empty() {
        None
    } else {
        Some(section)
    }
}

mod stream;

use self::openai_stream_runtime::{
    PersistentWsResult, extract_error_with_retry, is_retryable_error, openai_access_token,
};

use self::stream::{OpenAIResponsesStream, parse_openai_response_event};
#[cfg(test)]
use self::stream::{handle_openai_output_item, parse_text_wrapped_tool_call};

#[path = "openai_provider_impl.rs"]
mod openai_provider_impl;
#[path = "openai_stream_runtime.rs"]
mod openai_stream_runtime;

mod websocket_health;

#[cfg(test)]
use self::websocket_health::{
    WebsocketFallbackReason, clear_websocket_cooldown, normalize_transport_model,
    set_websocket_cooldown, websocket_cooldown_for_streak, websocket_remaining_timeout_secs,
};
use self::websocket_health::{
    classify_websocket_fallback_reason, is_stream_activity_event, is_websocket_activity_payload,
    is_websocket_fallback_notice, is_websocket_first_activity_payload, record_websocket_fallback,
    record_websocket_success, summarize_websocket_fallback_reason, websocket_activity_timeout_kind,
    websocket_cooldown_remaining, websocket_next_activity_timeout_secs,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::codex::CodexCredentials;
    use crate::message::{ContentBlock, Role};
    use anyhow::Result;
    use futures::{SinkExt, StreamExt};
    use std::collections::{HashMap, HashSet};
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};
    use std::time::{Duration, Instant};
    const BRIGHT_PEARL_WRAPPED_TOOL_CALL_FIXTURE: &str =
        include_str!("../../tests/fixtures/openai/bright_pearl_wrapped_tool_call.txt");
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            crate::env::set_var(key, value);
            Self { key, previous }
        }

        fn set_path(key: &'static str, value: &std::path::Path) -> Self {
            let previous = std::env::var_os(key);
            crate::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(previous) = &self.previous {
                crate::env::set_var(self.key, previous);
            } else {
                crate::env::remove_var(self.key);
            }
        }
    }

    async fn test_persistent_ws_state() -> (PersistentWsState, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test websocket listener");
        let addr = listener.local_addr().expect("listener local addr");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept websocket client");
            let mut ws = tokio_tungstenite::accept_async(stream)
                .await
                .expect("accept websocket handshake");
            while let Some(message) = ws.next().await {
                match message {
                    Ok(WsMessage::Ping(payload)) => {
                        let _ = ws.send(WsMessage::Pong(payload)).await;
                    }
                    Ok(WsMessage::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
        });

        let (client_ws, _) = connect_async(format!("ws://{}", addr))
            .await
            .expect("connect websocket client");
        (
            PersistentWsState {
                ws_stream: client_ws,
                last_response_id: "resp_test".to_string(),
                connected_at: Instant::now(),
                last_activity_at: Instant::now(),
                message_count: 1,
                last_input_item_count: 1,
            },
            server,
        )
    }

    struct LiveOpenAITestEnv {
        _lock: MutexGuard<'static, ()>,
        _jcode_home: EnvVarGuard,
        _transport: EnvVarGuard,
        _temp: tempfile::TempDir,
    }

    impl LiveOpenAITestEnv {
        fn new() -> Result<Option<Self>> {
            let lock = ENV_LOCK.lock().unwrap();
            let Some(source_auth) = real_codex_auth_path() else {
                return Ok(None);
            };

            let temp = tempfile::Builder::new()
                .prefix("jcode-openai-live-")
                .tempdir()?;
            let target_auth = temp
                .path()
                .join("external")
                .join(".codex")
                .join("auth.json");
            std::fs::create_dir_all(
                target_auth
                    .parent()
                    .expect("temp auth target should have a parent"),
            )?;
            std::fs::copy(source_auth, &target_auth)?;

            let jcode_home = EnvVarGuard::set_path("JCODE_HOME", temp.path());
            let transport = EnvVarGuard::set("JCODE_OPENAI_TRANSPORT", "https");

            Ok(Some(Self {
                _lock: lock,
                _jcode_home: jcode_home,
                _transport: transport,
                _temp: temp,
            }))
        }
    }

    fn real_codex_auth_path() -> Option<PathBuf> {
        let home = dirs::home_dir()?;
        let path = home.join(".codex").join("auth.json");
        path.exists().then_some(path)
    }

    async fn live_openai_catalog() -> Result<Option<crate::provider::OpenAIModelCatalog>> {
        let Some(_env) = LiveOpenAITestEnv::new()? else {
            return Ok(None);
        };
        let creds = crate::auth::codex::load_credentials()?;
        if !OpenAIProvider::is_chatgpt_mode(&creds) {
            return Ok(None);
        }

        let token = openai_access_token(&Arc::new(RwLock::new(creds))).await?;
        Ok(Some(
            crate::provider::fetch_openai_model_catalog(&token).await?,
        ))
    }

    async fn live_openai_smoke(model: &str, sentinel: &str) -> Result<Option<String>> {
        let Some(_env) = LiveOpenAITestEnv::new()? else {
            return Ok(None);
        };
        let creds = crate::auth::codex::load_credentials()?;
        if !OpenAIProvider::is_chatgpt_mode(&creds) {
            return Ok(None);
        }

        let provider = OpenAIProvider::new(creds);
        provider.set_model(model)?;
        let response = provider
            .complete_simple(&format!("Reply with exactly {}.", sentinel), "")
            .await?;
        Ok(Some(response))
    }

    #[test]
    fn test_openai_supports_codex_models() {
        let creds = CodexCredentials {
            access_token: "test".to_string(),
            refresh_token: String::new(),
            id_token: None,
            account_id: None,
            expires_at: None,
        };

        let provider = OpenAIProvider::new(creds);
        assert!(provider.available_models().contains(&"gpt-5.2-codex"));
        assert!(provider.available_models().contains(&"gpt-5.1-codex-mini"));

        provider.set_model("gpt-5.1-codex").unwrap();
        assert_eq!(provider.model(), "gpt-5.1-codex");

        provider.set_model("gpt-5.1-codex-mini").unwrap();
        assert_eq!(provider.model(), "gpt-5.1-codex-mini");
    }

    #[test]
    fn test_openai_switching_models_include_dynamic_catalog_entries() {
        let _guard = crate::storage::lock_test_env();
        let dynamic_model = "gpt-5.9-switching-test";
        crate::auth::codex::set_active_account_override(Some("switching-test".to_string()));
        crate::provider::populate_account_models(vec![dynamic_model.to_string()]);

        let provider = OpenAIProvider::new(CodexCredentials {
            access_token: "test".to_string(),
            refresh_token: String::new(),
            id_token: None,
            account_id: None,
            expires_at: None,
        });

        let models = provider.available_models_for_switching();
        assert!(models.contains(&"gpt-5.4".to_string()));
        assert!(models.contains(&dynamic_model.to_string()));

        crate::auth::codex::set_active_account_override(None);
    }

    #[test]
    fn test_summarize_ws_input_counts_tool_outputs() {
        let items = vec![
            serde_json::json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }),
            serde_json::json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "bash",
                "arguments": "{}"
            }),
            serde_json::json!({
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "ok"
            }),
            serde_json::json!({"type": "unknown"}),
        ];

        assert_eq!(
            summarize_ws_input(&items),
            WsInputStats {
                total_items: 4,
                message_items: 1,
                function_call_items: 1,
                function_call_output_items: 1,
                other_items: 1,
            }
        );
    }

    #[test]
    fn test_persistent_ws_idle_policy_thresholds() {
        assert!(!persistent_ws_idle_needs_healthcheck(Duration::from_secs(
            5
        )));
        assert!(persistent_ws_idle_needs_healthcheck(Duration::from_secs(
            WEBSOCKET_PERSISTENT_HEALTHCHECK_IDLE_SECS
        )));
        assert!(!persistent_ws_idle_requires_reconnect(Duration::from_secs(
            30
        )));
        assert!(persistent_ws_idle_requires_reconnect(Duration::from_secs(
            WEBSOCKET_PERSISTENT_IDLE_RECONNECT_SECS
        )));
    }

    #[tokio::test]
    async fn test_set_model_clears_persistent_ws_state() {
        let provider = OpenAIProvider::new(CodexCredentials {
            access_token: "test".to_string(),
            refresh_token: String::new(),
            id_token: None,
            account_id: None,
            expires_at: None,
        });
        let (state, server) = test_persistent_ws_state().await;
        *provider.persistent_ws.lock().await = Some(state);

        provider.set_model("gpt-5.3-codex").expect("set model");

        assert!(
            provider.persistent_ws.lock().await.is_none(),
            "changing models should reset the persistent websocket chain"
        );
        server.abort();
    }

    #[tokio::test]
    async fn test_switching_to_https_clears_persistent_ws_state() {
        let provider = OpenAIProvider::new(CodexCredentials {
            access_token: "test".to_string(),
            refresh_token: String::new(),
            id_token: None,
            account_id: None,
            expires_at: None,
        });
        let (state, server) = test_persistent_ws_state().await;
        *provider.persistent_ws.lock().await = Some(state);

        provider
            .set_transport("https")
            .expect("switch transport to https");

        assert!(
            provider.persistent_ws.lock().await.is_none(),
            "switching to HTTPS should drop the websocket continuation chain"
        );
        server.abort();
    }

    #[test]
    fn test_service_tier_can_be_changed_while_a_request_snapshot_is_held() {
        let provider = Arc::new(OpenAIProvider::new(CodexCredentials {
            access_token: "test".to_string(),
            refresh_token: String::new(),
            id_token: None,
            account_id: None,
            expires_at: None,
        }));

        let read_guard = provider
            .service_tier
            .read()
            .expect("service tier read lock should be available");

        let (tx, rx) = std::sync::mpsc::channel();
        let provider_for_write = Arc::clone(&provider);
        let handle = std::thread::spawn(move || {
            let result = provider_for_write.set_service_tier("priority");
            tx.send(result).expect("send result from setter thread");
        });

        std::thread::sleep(Duration::from_millis(20));
        assert!(
            rx.try_recv().is_err(),
            "writer should wait for the in-flight snapshot to finish"
        );

        drop(read_guard);

        rx.recv()
            .expect("receive service tier setter result")
            .expect("service tier update should succeed once read lock is released");
        handle.join().expect("join setter thread");

        assert_eq!(provider.service_tier(), Some("priority".to_string()));
    }

    #[test]
    fn test_build_responses_input_injects_missing_tool_output() {
        let expected_missing = format!("[Error] {}", TOOL_OUTPUT_MISSING_TEXT);
        let messages = vec![
            ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hi".to_string(),
                    cache_control: None,
                }],
                timestamp: None,
                tool_duration_ms: None,
            },
            ChatMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!({"command": "ls"}),
                }],
                timestamp: None,
                tool_duration_ms: None,
            },
        ];

        let items = build_responses_input(&messages);
        let mut saw_call = false;
        let mut saw_output = false;

        for item in &items {
            let item_type = item.get("type").and_then(|v| v.as_str());
            match item_type {
                Some("function_call") => {
                    if item.get("call_id").and_then(|v| v.as_str()) == Some("call_1") {
                        saw_call = true;
                    }
                }
                Some("function_call_output") => {
                    if item.get("call_id").and_then(|v| v.as_str()) == Some("call_1") {
                        let output = item.get("output").and_then(|v| v.as_str());
                        assert_eq!(output, Some(expected_missing.as_str()));
                        saw_output = true;
                    }
                }
                _ => {}
            }
        }

        assert!(saw_call);
        assert!(saw_output);
    }

    #[test]
    fn test_build_responses_input_preserves_tool_output() {
        let messages = vec![
            ChatMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!({"command": "ls"}),
                }],
                timestamp: None,
                tool_duration_ms: None,
            },
            ChatMessage::tool_result("call_1", "ok", false),
        ];

        let items = build_responses_input(&messages);
        let mut outputs = Vec::new();

        for item in &items {
            if item.get("type").and_then(|v| v.as_str()) == Some("function_call_output")
                && item.get("call_id").and_then(|v| v.as_str()) == Some("call_1")
                && let Some(output) = item.get("output").and_then(|v| v.as_str())
            {
                outputs.push(output.to_string());
            }
        }

        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0], "ok");
    }

    #[test]
    fn test_build_responses_input_reorders_early_tool_output() {
        let messages = vec![
            ChatMessage::tool_result("call_1", "ok", false),
            ChatMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!({"command": "ls"}),
                }],
                timestamp: None,
                tool_duration_ms: None,
            },
        ];

        let items = build_responses_input(&messages);
        let mut call_pos = None;
        let mut output_pos = None;
        let mut outputs = Vec::new();

        for (idx, item) in items.iter().enumerate() {
            let item_type = item.get("type").and_then(|v| v.as_str());
            match item_type {
                Some("function_call") => {
                    if item.get("call_id").and_then(|v| v.as_str()) == Some("call_1") {
                        call_pos = Some(idx);
                    }
                }
                Some("function_call_output") => {
                    if item.get("call_id").and_then(|v| v.as_str()) == Some("call_1") {
                        output_pos = Some(idx);
                        if let Some(output) = item.get("output").and_then(|v| v.as_str()) {
                            outputs.push(output.to_string());
                        }
                    }
                }
                _ => {}
            }
        }

        assert!(call_pos.is_some());
        assert!(output_pos.is_some());
        assert!(output_pos.unwrap() > call_pos.unwrap());
        assert_eq!(outputs, vec!["ok".to_string()]);
    }

    #[test]
    fn test_build_responses_input_keeps_image_context_after_tool_output() {
        let messages = vec![
            ChatMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "read".to_string(),
                    input: serde_json::json!({"file_path": "screenshot.png"}),
                }],
                timestamp: None,
                tool_duration_ms: None,
            },
            ChatMessage {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "call_1".to_string(),
                        content: "Image: screenshot.png\nImage sent to model for vision analysis."
                            .to_string(),
                        is_error: None,
                    },
                    ContentBlock::Image {
                        media_type: "image/png".to_string(),
                        data: "ZmFrZQ==".to_string(),
                    },
                    ContentBlock::Text {
                        text: "[Attached image associated with the preceding tool result: screenshot.png]"
                            .to_string(),
                        cache_control: None,
                    },
                ],
                timestamp: None,
                tool_duration_ms: None,
            },
        ];

        let items = build_responses_input(&messages);
        let mut output_pos = None;
        let mut image_msg_pos = None;

        for (idx, item) in items.iter().enumerate() {
            match item.get("type").and_then(|v| v.as_str()) {
                Some("function_call_output")
                    if item.get("call_id").and_then(|v| v.as_str()) == Some("call_1") =>
                {
                    output_pos = Some(idx);
                    assert_eq!(
                        item.get("output").and_then(|v| v.as_str()),
                        Some("Image: screenshot.png\nImage sent to model for vision analysis.")
                    );
                }
                Some("message") if item.get("role").and_then(|v| v.as_str()) == Some("user") => {
                    let Some(content) = item.get("content").and_then(|v| v.as_array()) else {
                        continue;
                    };
                    let has_image = content.iter().any(|part| {
                        part.get("type").and_then(|v| v.as_str()) == Some("input_image")
                    });
                    let has_label = content.iter().any(|part| {
                        part.get("type").and_then(|v| v.as_str()) == Some("input_text")
                            && part
                                .get("text")
                                .and_then(|v| v.as_str())
                                .map(|text| text.contains("screenshot.png"))
                                .unwrap_or(false)
                    });
                    if has_image && has_label {
                        image_msg_pos = Some(idx);
                    }
                }
                _ => {}
            }
        }

        assert!(output_pos.is_some(), "expected function call output item");
        assert!(
            image_msg_pos.is_some(),
            "expected follow-up user image message"
        );
        assert!(
            image_msg_pos.unwrap() > output_pos.unwrap(),
            "image context should stay after the tool output"
        );
    }

    #[test]
    fn test_build_responses_input_injects_only_missing_outputs() {
        let expected_missing = format!("[Error] {}", TOOL_OUTPUT_MISSING_TEXT);
        let messages = vec![
            ChatMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_a".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!({"command": "pwd"}),
                }],
                timestamp: None,
                tool_duration_ms: None,
            },
            ChatMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_b".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!({"command": "whoami"}),
                }],
                timestamp: None,
                tool_duration_ms: None,
            },
            ChatMessage::tool_result("call_b", "done", false),
        ];

        let items = build_responses_input(&messages);
        let mut output_a = None;
        let mut output_b = None;

        for item in &items {
            if item.get("type").and_then(|v| v.as_str()) == Some("function_call_output") {
                match item.get("call_id").and_then(|v| v.as_str()) {
                    Some("call_a") => {
                        output_a = item
                            .get("output")
                            .and_then(|v| v.as_str())
                            .map(|v| v.to_string());
                    }
                    Some("call_b") => {
                        output_b = item
                            .get("output")
                            .and_then(|v| v.as_str())
                            .map(|v| v.to_string());
                    }
                    _ => {}
                }
            }
        }

        assert_eq!(output_a.as_deref(), Some(expected_missing.as_str()));
        assert_eq!(output_b.as_deref(), Some("done"));
    }

    #[test]
    fn test_openai_retryable_error_patterns() {
        assert!(is_retryable_error(
            "stream disconnected before completion: transport error"
        ));
        assert!(is_retryable_error(
            "falling back from websockets to https transport. stream disconnected before completion"
        ));
        assert!(is_retryable_error(
            "OpenAI HTTPS stream ended before message completion marker"
        ));
    }

    #[test]
    fn test_parse_max_output_tokens_defaults_to_safe_value() {
        assert_eq!(
            OpenAIProvider::parse_max_output_tokens(None),
            Some(DEFAULT_MAX_OUTPUT_TOKENS)
        );
        assert_eq!(
            OpenAIProvider::parse_max_output_tokens(Some("")),
            Some(DEFAULT_MAX_OUTPUT_TOKENS)
        );
    }

    #[test]
    fn test_parse_max_output_tokens_allows_disable_and_override() {
        assert_eq!(OpenAIProvider::parse_max_output_tokens(Some("0")), None);
        assert_eq!(
            OpenAIProvider::parse_max_output_tokens(Some("32768")),
            Some(32768)
        );
        assert_eq!(
            OpenAIProvider::parse_max_output_tokens(Some("not-a-number")),
            Some(DEFAULT_MAX_OUTPUT_TOKENS)
        );
    }

    #[test]
    fn test_build_response_request_for_gpt_5_4_1m_uses_base_model_without_extra_flags() {
        let request = OpenAIProvider::build_response_request(
            "gpt-5.4",
            "system".to_string(),
            &[],
            &[],
            true,
            Some(DEFAULT_MAX_OUTPUT_TOKENS),
            Some("xhigh"),
            Some("unused"),
            Some("unused"),
            None,
            None,
        );

        assert_eq!(request["model"], serde_json::json!("gpt-5.4"));
        assert!(request.get("model_context_window").is_none());
        assert!(request.get("max_output_tokens").is_none());
        assert!(request.get("prompt_cache_key").is_none());
        assert!(request.get("prompt_cache_retention").is_none());
    }

    #[test]
    fn test_build_response_request_omits_long_context_for_plain_gpt_5_4() {
        let request = OpenAIProvider::build_response_request(
            "gpt-5.4",
            "system".to_string(),
            &[],
            &[],
            true,
            Some(DEFAULT_MAX_OUTPUT_TOKENS),
            None,
            None,
            None,
            None,
            None,
        );

        assert!(request.get("model_context_window").is_none());
    }

    #[tokio::test]
    #[ignore = "requires real OpenAI OAuth credentials"]
    async fn live_openai_catalog_lists_gpt_5_4_family() -> Result<()> {
        let Some(catalog) = live_openai_catalog().await? else {
            eprintln!("skipping live OpenAI catalog test: no real OAuth credentials");
            return Ok(());
        };

        crate::provider::populate_context_limits(catalog.context_limits.clone());
        crate::provider::populate_account_models(catalog.available_models.clone());

        assert!(
            catalog
                .available_models
                .iter()
                .any(|model| model.starts_with("gpt-5.4")),
            "expected GPT-5.4 family in live catalog, got {:?}",
            catalog.available_models
        );
        assert!(
            crate::provider::known_openai_model_ids()
                .iter()
                .any(|model| model == "gpt-5.4"),
            "expected GPT-5.4 in display model list"
        );

        let reports_long_context = catalog
            .context_limits
            .get("gpt-5.4")
            .copied()
            .unwrap_or_default()
            >= 1_000_000;
        assert_eq!(
            crate::provider::known_openai_model_ids()
                .iter()
                .any(|model| model == "gpt-5.4[1m]"),
            reports_long_context,
            "displayed 1m alias should follow the live catalog"
        );

        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires real OpenAI OAuth credentials"]
    async fn live_openai_gpt_5_4_and_fast_requests_succeed() -> Result<()> {
        let Some(catalog) = live_openai_catalog().await? else {
            eprintln!("skipping live OpenAI response test: no real OAuth credentials");
            return Ok(());
        };
        crate::provider::populate_context_limits(catalog.context_limits.clone());
        crate::provider::populate_account_models(catalog.available_models.clone());

        let Some(plain_response) = live_openai_smoke("gpt-5.4", "JCODE_GPT54_OK").await? else {
            eprintln!("skipping live OpenAI response test: no real OAuth credentials");
            return Ok(());
        };
        assert!(
            plain_response.contains("JCODE_GPT54_OK"),
            "unexpected GPT-5.4 response: {}",
            plain_response
        );

        if catalog
            .available_models
            .iter()
            .any(|model| model == "gpt-5.3-codex-spark")
        {
            let Some(fast_response) =
                live_openai_smoke("gpt-5.3-codex-spark", "JCODE_GPT53_SPARK_OK").await?
            else {
                eprintln!("skipping live OpenAI fast-model test: no real OAuth credentials");
                return Ok(());
            };
            assert!(
                fast_response.contains("JCODE_GPT53_SPARK_OK"),
                "unexpected gpt-5.3-codex-spark response: {}",
                fast_response
            );
        }

        if crate::provider::known_openai_model_ids()
            .iter()
            .any(|model| model == "gpt-5.4[1m]")
        {
            let Some(long_context_response) =
                live_openai_smoke("gpt-5.4[1m]", "JCODE_GPT54_1M_OK").await?
            else {
                eprintln!("skipping live OpenAI 1m test: no real OAuth credentials");
                return Ok(());
            };
            assert!(
                long_context_response.contains("JCODE_GPT54_1M_OK"),
                "unexpected GPT-5.4[1m] response: {}",
                long_context_response
            );
        }

        Ok(())
    }

    #[test]
    fn test_should_prefer_websocket_enabled_for_named_models() {
        assert!(OpenAIProvider::should_prefer_websocket(
            "gpt-5.3-codex-spark"
        ));
        assert!(OpenAIProvider::should_prefer_websocket("gpt-5.3-codex"));
        assert!(OpenAIProvider::should_prefer_websocket("gpt-5"));
        assert!(OpenAIProvider::should_prefer_websocket("codex-mini"));
        assert!(!OpenAIProvider::should_prefer_websocket(""));
    }

    #[test]
    fn test_openai_transport_mode_defaults_to_auto() {
        let mode = OpenAITransportMode::from_config(None);
        assert_eq!(mode.as_str(), "auto");
    }

    #[test]
    fn test_openai_transport_mode_auto_prefers_websocket_for_openai_models() {
        let mode = OpenAITransportMode::from_config(Some("auto"));
        assert_eq!(mode.as_str(), "auto");
        assert!(OpenAIProvider::should_prefer_websocket("gpt-5.4"));
    }

    #[tokio::test]
    async fn test_record_websocket_fallback_sets_cooldown_for_auto_default_models() {
        let cooldowns = Arc::new(RwLock::new(HashMap::new()));
        let streaks = Arc::new(RwLock::new(HashMap::new()));
        let model = "gpt-5.4";

        let (streak, cooldown) = record_websocket_fallback(
            &cooldowns,
            &streaks,
            model,
            WebsocketFallbackReason::StreamTimeout,
        )
        .await;
        assert_eq!(streak, 1);
        assert_eq!(
            cooldown,
            Duration::from_secs(WEBSOCKET_MODEL_COOLDOWN_BASE_SECS)
        );
        assert!(
            websocket_cooldown_remaining(&cooldowns, model)
                .await
                .is_some(),
            "auto websocket default must still be guarded by cooldown after fallback"
        );
    }

    #[tokio::test]
    async fn test_websocket_cooldown_helpers_set_clear_and_expire() {
        let cooldowns = Arc::new(RwLock::new(HashMap::new()));
        let model = "gpt-5.3-codex";

        assert!(
            websocket_cooldown_remaining(&cooldowns, model)
                .await
                .is_none()
        );

        set_websocket_cooldown(&cooldowns, model).await;
        let remaining = websocket_cooldown_remaining(&cooldowns, model).await;
        assert!(remaining.is_some());

        clear_websocket_cooldown(&cooldowns, model).await;
        assert!(
            websocket_cooldown_remaining(&cooldowns, model)
                .await
                .is_none()
        );

        {
            let mut guard = cooldowns.write().await;
            guard.insert(model.to_string(), Instant::now() - Duration::from_secs(1));
        }
        assert!(
            websocket_cooldown_remaining(&cooldowns, model)
                .await
                .is_none()
        );
        assert!(!cooldowns.read().await.contains_key(model));
    }

    #[test]
    fn test_websocket_cooldown_for_streak_scales_and_caps() {
        assert_eq!(
            websocket_cooldown_for_streak(1, WebsocketFallbackReason::StreamTimeout),
            Duration::from_secs(WEBSOCKET_MODEL_COOLDOWN_BASE_SECS)
        );
        assert_eq!(
            websocket_cooldown_for_streak(2, WebsocketFallbackReason::StreamTimeout),
            Duration::from_secs(WEBSOCKET_MODEL_COOLDOWN_BASE_SECS * 2)
        );
        assert_eq!(
            websocket_cooldown_for_streak(3, WebsocketFallbackReason::StreamTimeout),
            Duration::from_secs(WEBSOCKET_MODEL_COOLDOWN_BASE_SECS * 4)
        );
        assert_eq!(
            websocket_cooldown_for_streak(32, WebsocketFallbackReason::StreamTimeout),
            Duration::from_secs(WEBSOCKET_MODEL_COOLDOWN_MAX_SECS)
        );
    }

    #[test]
    fn test_websocket_cooldown_for_reason_adjusts_by_failure_type() {
        assert_eq!(
            websocket_cooldown_for_streak(1, WebsocketFallbackReason::ConnectTimeout),
            Duration::from_secs((WEBSOCKET_MODEL_COOLDOWN_BASE_SECS / 2).max(1))
        );
        assert_eq!(
            websocket_cooldown_for_streak(1, WebsocketFallbackReason::ServerRequestedHttps),
            Duration::from_secs(WEBSOCKET_MODEL_COOLDOWN_BASE_SECS * 5)
        );
        assert_eq!(
            websocket_cooldown_for_streak(32, WebsocketFallbackReason::ServerRequestedHttps),
            Duration::from_secs(WEBSOCKET_MODEL_COOLDOWN_MAX_SECS * 3)
        );
    }

    #[tokio::test]
    async fn test_record_websocket_fallback_tracks_streak_and_cooldown() {
        let cooldowns = Arc::new(RwLock::new(HashMap::new()));
        let streaks = Arc::new(RwLock::new(HashMap::new()));
        let model = "gpt-5.3-codex-spark";

        let (streak1, cooldown1) = record_websocket_fallback(
            &cooldowns,
            &streaks,
            model,
            WebsocketFallbackReason::StreamTimeout,
        )
        .await;
        assert_eq!(streak1, 1);
        assert_eq!(
            cooldown1,
            Duration::from_secs(WEBSOCKET_MODEL_COOLDOWN_BASE_SECS)
        );
        let remaining1 = websocket_cooldown_remaining(&cooldowns, model)
            .await
            .expect("cooldown should be set");
        assert!(remaining1 <= cooldown1);

        let (streak2, cooldown2) = record_websocket_fallback(
            &cooldowns,
            &streaks,
            model,
            WebsocketFallbackReason::StreamTimeout,
        )
        .await;
        assert_eq!(streak2, 2);
        assert_eq!(
            cooldown2,
            Duration::from_secs(WEBSOCKET_MODEL_COOLDOWN_BASE_SECS * 2)
        );
        let remaining2 = websocket_cooldown_remaining(&cooldowns, model)
            .await
            .expect("cooldown should be set");
        assert!(remaining2 <= cooldown2);

        record_websocket_success(&cooldowns, &streaks, model).await;
        assert!(
            websocket_cooldown_remaining(&cooldowns, model)
                .await
                .is_none()
        );
        let normalized = normalize_transport_model(model).expect("normalized model");
        assert!(!streaks.read().await.contains_key(&normalized));
    }

    #[test]
    fn test_websocket_activity_payload_detection() {
        assert!(is_websocket_activity_payload(
            r#"{"type":"response.created","response":{"id":"resp_1"}}"#
        ));
        assert!(is_websocket_activity_payload(
            r#"{"type":"response.reasoning.delta","delta":"thinking"}"#
        ));
        assert!(!is_websocket_activity_payload("not json"));
        assert!(!is_websocket_activity_payload(r#"{"foo":"bar"}"#));
    }

    #[test]
    fn test_websocket_first_activity_payload_counts_typed_control_events() {
        assert!(is_websocket_first_activity_payload(
            r#"{"type":"rate_limits.updated"}"#
        ));
        assert!(is_websocket_first_activity_payload(
            r#"{"type":"session.created","session":{}}"#
        ));
        assert!(!is_websocket_first_activity_payload(r#"{"foo":"bar"}"#));
        assert!(!is_websocket_first_activity_payload("not json"));
    }

    #[test]
    fn test_websocket_completion_timeout_is_long_enough_for_reasoning() {
        let timeout = std::hint::black_box(WEBSOCKET_COMPLETION_TIMEOUT_SECS);
        assert!(
            timeout >= 120,
            "completion timeout regressed to {}s; reasoning models may need several minutes",
            timeout
        );
    }

    #[test]
    fn test_stream_activity_event_treats_any_stream_event_as_activity() {
        assert!(is_stream_activity_event(&StreamEvent::ThinkingStart));
        assert!(is_stream_activity_event(&StreamEvent::ThinkingDelta(
            "working".to_string()
        )));
        assert!(is_stream_activity_event(&StreamEvent::TextDelta(
            "hello".to_string()
        )));
        assert!(is_stream_activity_event(&StreamEvent::MessageEnd {
            stop_reason: None
        }));
    }

    #[test]
    fn test_websocket_activity_payload_counts_response_completed() {
        assert!(is_websocket_activity_payload(
            r#"{"type":"response.completed","response":{"status":"completed"}}"#
        ));
    }

    #[test]
    fn test_websocket_activity_payload_counts_in_progress_events() {
        assert!(is_websocket_activity_payload(
            r#"{"type":"response.in_progress","response":{"status":"in_progress"}}"#
        ));
    }

    #[test]
    fn test_websocket_activity_payload_ignores_non_response_events() {
        assert!(!is_websocket_activity_payload(
            r#"{"type":"session.created","session":{}}"#
        ));
        assert!(!is_websocket_activity_payload(
            r#"{"type":"rate_limits.updated"}"#
        ));
        assert!(!is_websocket_activity_payload(r#"not json at all"#));
    }

    #[test]
    fn test_websocket_remaining_timeout_secs_uses_idle_time_budget() {
        let recent = Instant::now() - Duration::from_secs(2);
        let remaining = websocket_remaining_timeout_secs(recent, 8).expect("still within budget");
        assert!(
            (6..=7).contains(&remaining),
            "expected remaining idle budget near 6-7s, got {remaining}"
        );
    }

    #[test]
    fn test_websocket_remaining_timeout_secs_expires_after_budget() {
        let expired = Instant::now() - Duration::from_secs(9);
        assert!(websocket_remaining_timeout_secs(expired, 8).is_none());
    }

    #[test]
    fn test_websocket_next_activity_timeout_uses_request_start_before_first_event() {
        let ws_started_at = Instant::now() - Duration::from_secs(3);
        let last_api_activity_at = Instant::now() - Duration::from_secs(1);
        let remaining =
            websocket_next_activity_timeout_secs(ws_started_at, last_api_activity_at, false)
                .expect("first-event timeout should still be active");
        assert!(
            (5..=6).contains(&remaining),
            "expected first-event timeout near 5-6s, got {remaining}"
        );
    }

    #[test]
    fn test_websocket_next_activity_timeout_resets_after_api_activity() {
        let ws_started_at = Instant::now() - Duration::from_secs(299);
        let last_api_activity_at = Instant::now() - Duration::from_secs(2);
        let remaining =
            websocket_next_activity_timeout_secs(ws_started_at, last_api_activity_at, true)
                .expect("idle timeout should use last activity, not total request age");
        assert!(
            remaining >= WEBSOCKET_COMPLETION_TIMEOUT_SECS.saturating_sub(3),
            "expected full idle budget to reset after activity, got {remaining}"
        );
    }

    #[test]
    fn test_websocket_activity_timeout_kind_labels_first_and_next() {
        assert_eq!(websocket_activity_timeout_kind(false), "first");
        assert_eq!(websocket_activity_timeout_kind(true), "next");
    }

    #[test]
    fn test_format_status_duration_uses_compact_human_labels() {
        assert_eq!(format_status_duration(Duration::from_secs(9)), "9s");
        assert_eq!(format_status_duration(Duration::from_secs(125)), "2m 5s");
        assert_eq!(format_status_duration(Duration::from_secs(7260)), "2h 1m");
    }

    #[test]
    fn test_summarize_websocket_fallback_reason_classifies_common_failures() {
        assert_eq!(
            summarize_websocket_fallback_reason("WebSocket connect timed out after 8s"),
            "connect timeout"
        );
        assert_eq!(
            summarize_websocket_fallback_reason(
                "WebSocket stream timed out waiting for first websocket activity (8s)"
            ),
            "first response timeout"
        );
        assert_eq!(
            summarize_websocket_fallback_reason(
                "WebSocket stream timed out waiting for next websocket activity (300s)"
            ),
            "stream timeout"
        );
        assert_eq!(
            summarize_websocket_fallback_reason("server requested fallback"),
            "server requested https"
        );
        assert_eq!(
            summarize_websocket_fallback_reason(
                "WebSocket stream closed before response.completed"
            ),
            "stream closed early"
        );
    }

    #[test]
    fn test_normalize_transport_model_trims_and_lowercases() {
        assert_eq!(
            normalize_transport_model("  GPT-5.4  "),
            Some("gpt-5.4".to_string())
        );
        assert_eq!(normalize_transport_model("   \t\n  "), None);
    }

    #[tokio::test]
    async fn test_record_websocket_success_clears_normalized_keys() {
        let cooldowns = Arc::new(RwLock::new(HashMap::new()));
        let streaks = Arc::new(RwLock::new(HashMap::new()));
        let canonical = "gpt-5.4";

        record_websocket_fallback(
            &cooldowns,
            &streaks,
            canonical,
            WebsocketFallbackReason::StreamTimeout,
        )
        .await;
        assert!(
            websocket_cooldown_remaining(&cooldowns, canonical)
                .await
                .is_some()
        );

        record_websocket_success(&cooldowns, &streaks, " GPT-5.4 ").await;

        assert!(
            websocket_cooldown_remaining(&cooldowns, canonical)
                .await
                .is_none(),
            "success should clear normalized cooldown entries"
        );
        assert!(
            !streaks.read().await.contains_key(canonical),
            "success should clear normalized failure streak entries"
        );
    }

    #[test]
    fn test_build_response_request_includes_stream_for_http() {
        let request = OpenAIProvider::build_response_request(
            "gpt-5.4",
            "system".to_string(),
            &[],
            &[],
            false,
            Some(DEFAULT_MAX_OUTPUT_TOKENS),
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(request["stream"], serde_json::json!(true));
        assert_eq!(request["store"], serde_json::json!(false));
    }

    #[test]
    fn test_websocket_payload_strips_stream_and_background() {
        let mut request = OpenAIProvider::build_response_request(
            "gpt-5.4",
            "system".to_string(),
            &[serde_json::json!({"role": "user", "content": "hello"})],
            &[],
            false,
            Some(DEFAULT_MAX_OUTPUT_TOKENS),
            None,
            None,
            None,
            None,
            None,
        );

        assert_eq!(request["stream"], serde_json::json!(true));

        request["background"] = serde_json::json!(true);

        let obj = request.as_object_mut().expect("request is object");
        obj.insert(
            "type".to_string(),
            serde_json::Value::String("response.create".to_string()),
        );
        obj.remove("stream");
        obj.remove("background");

        assert!(
            request.get("stream").is_none(),
            "stream must be stripped for WebSocket payloads"
        );
        assert!(
            request.get("background").is_none(),
            "background must be stripped for WebSocket payloads"
        );
        assert_eq!(request["type"], serde_json::json!("response.create"));
    }

    #[test]
    fn test_websocket_payload_preserves_required_fields() {
        let mut request = OpenAIProvider::build_response_request(
            "gpt-5.4",
            "system prompt".to_string(),
            &[serde_json::json!({"role": "user", "content": "hello"})],
            &[serde_json::json!({"type": "function", "name": "bash"})],
            false,
            Some(16384),
            Some("high"),
            None,
            None,
            None,
            None,
        );

        let obj = request.as_object_mut().expect("request is object");
        obj.insert(
            "type".to_string(),
            serde_json::Value::String("response.create".to_string()),
        );
        obj.remove("stream");
        obj.remove("background");

        assert_eq!(request["type"], "response.create");
        assert_eq!(request["model"], "gpt-5.4");
        assert_eq!(request["instructions"], "system prompt");
        assert!(request["input"].is_array());
        assert!(request["tools"].is_array());
        assert_eq!(request["max_output_tokens"], serde_json::json!(16384));
        assert_eq!(request["reasoning"], serde_json::json!({"effort": "high"}));
        assert_eq!(request["tool_choice"], "auto");
    }

    #[test]
    fn test_websocket_continuation_request_excludes_transport_fields() {
        let base_request = OpenAIProvider::build_response_request(
            "gpt-5.4",
            "system".to_string(),
            &[],
            &[serde_json::json!({"type": "function", "name": "bash"})],
            false,
            Some(DEFAULT_MAX_OUTPUT_TOKENS),
            None,
            None,
            None,
            None,
            Some(160_000),
        );

        let mut continuation = serde_json::json!({
            "type": "response.create",
            "previous_response_id": "resp_abc123",
            "input": [{"role": "user", "content": "follow up"}],
        });

        if let Some(model) = base_request.get("model") {
            continuation["model"] = model.clone();
        }
        if let Some(tools) = base_request.get("tools") {
            continuation["tools"] = tools.clone();
        }
        if let Some(instructions) = base_request.get("instructions") {
            continuation["instructions"] = instructions.clone();
        }
        if let Some(context_management) = base_request.get("context_management") {
            continuation["context_management"] = context_management.clone();
        }
        continuation["store"] = serde_json::json!(false);
        continuation["parallel_tool_calls"] = serde_json::json!(false);

        assert!(
            continuation.get("stream").is_none(),
            "continuation request must not include stream"
        );
        assert!(
            continuation.get("background").is_none(),
            "continuation request must not include background"
        );
        assert_eq!(continuation["type"], "response.create");
        assert_eq!(continuation["previous_response_id"], "resp_abc123");
        assert_eq!(continuation["model"], "gpt-5.4");
        assert_eq!(
            continuation["context_management"],
            serde_json::json!([
                {
                    "type": "compaction",
                    "compact_threshold": 160_000,
                }
            ])
        );
    }

    #[test]
    fn test_parse_openai_response_completed_captures_incomplete_stop_reason() {
        let data = r#"{"type":"response.completed","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}}"#;
        let mut saw_text_delta = false;
        let mut streaming_tool_calls = HashMap::new();
        let mut completed_tool_items = HashSet::new();
        let mut pending = VecDeque::new();

        let event = parse_openai_response_event(
            data,
            &mut saw_text_delta,
            &mut streaming_tool_calls,
            &mut completed_tool_items,
            &mut pending,
        )
        .expect("expected message end");
        match event {
            StreamEvent::MessageEnd { stop_reason } => {
                assert_eq!(stop_reason.as_deref(), Some("max_output_tokens"));
            }
            other => panic!("expected MessageEnd, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_openai_response_completed_without_stop_reason() {
        let data = r#"{"type":"response.completed","response":{"status":"completed"}}"#;
        let mut saw_text_delta = false;
        let mut streaming_tool_calls = HashMap::new();
        let mut completed_tool_items = HashSet::new();
        let mut pending = VecDeque::new();

        let event = parse_openai_response_event(
            data,
            &mut saw_text_delta,
            &mut streaming_tool_calls,
            &mut completed_tool_items,
            &mut pending,
        )
        .expect("expected message end");
        match event {
            StreamEvent::MessageEnd { stop_reason } => {
                assert!(stop_reason.is_none());
            }
            other => panic!("expected MessageEnd, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_openai_response_completed_commentary_phase_sets_stop_reason() {
        let data = r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"Still working"}]}]}}"#;
        let mut saw_text_delta = false;
        let mut streaming_tool_calls = HashMap::new();
        let mut completed_tool_items = HashSet::new();
        let mut pending = VecDeque::new();

        let event = parse_openai_response_event(
            data,
            &mut saw_text_delta,
            &mut streaming_tool_calls,
            &mut completed_tool_items,
            &mut pending,
        )
        .expect("expected message end");
        match event {
            StreamEvent::MessageEnd { stop_reason } => {
                assert_eq!(stop_reason.as_deref(), Some("commentary"));
            }
            other => panic!("expected MessageEnd, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_openai_response_incomplete_emits_message_end_with_reason() {
        let data = r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"content_filter"}}}"#;
        let mut saw_text_delta = false;
        let mut streaming_tool_calls = HashMap::new();
        let mut completed_tool_items = HashSet::new();
        let mut pending = VecDeque::new();

        let event = parse_openai_response_event(
            data,
            &mut saw_text_delta,
            &mut streaming_tool_calls,
            &mut completed_tool_items,
            &mut pending,
        )
        .expect("expected message end");
        match event {
            StreamEvent::MessageEnd { stop_reason } => {
                assert_eq!(stop_reason.as_deref(), Some("content_filter"));
            }
            other => panic!("expected MessageEnd, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_openai_response_function_call_arguments_streaming() {
        let mut saw_text_delta = false;
        let mut streaming_tool_calls = HashMap::new();
        let mut completed_tool_items = HashSet::new();
        let mut pending = VecDeque::new();

        let added = r#"{"type":"response.output_item.added","item":{"id":"fc_123","type":"function_call","call_id":"call_123","name":"batch","arguments":""}}"#;
        assert!(
            parse_openai_response_event(
                added,
                &mut saw_text_delta,
                &mut streaming_tool_calls,
                &mut completed_tool_items,
                &mut pending,
            )
            .is_none(),
            "output_item.added should just seed tool state"
        );

        let delta = r#"{"type":"response.function_call_arguments.delta","item_id":"fc_123","delta":"{\"tool_calls\":[{\"tool\":\"read\"}]"}"#;
        assert!(
            parse_openai_response_event(
                delta,
                &mut saw_text_delta,
                &mut streaming_tool_calls,
                &mut completed_tool_items,
                &mut pending,
            )
            .is_none(),
            "argument delta should accumulate state only"
        );

        let done = r#"{"type":"response.function_call_arguments.done","item_id":"fc_123","arguments":"{\"tool_calls\":[{\"tool\":\"read\"}]}"}"#;
        let first = parse_openai_response_event(
            done,
            &mut saw_text_delta,
            &mut streaming_tool_calls,
            &mut completed_tool_items,
            &mut pending,
        )
        .expect("expected tool start");

        match first {
            StreamEvent::ToolUseStart { id, name } => {
                assert_eq!(id, "call_123");
                assert_eq!(name, "batch");
            }
            other => panic!("expected ToolUseStart, got {:?}", other),
        }

        match pending.pop_front() {
            Some(StreamEvent::ToolInputDelta(delta)) => {
                let parsed: Value = serde_json::from_str(&delta).expect("valid args json");
                let tool_calls = parsed
                    .get("tool_calls")
                    .and_then(|v| v.as_array())
                    .expect("tool_calls array");
                assert_eq!(tool_calls.len(), 1);
            }
            other => panic!("expected ToolInputDelta, got {:?}", other),
        }

        assert!(matches!(pending.pop_front(), Some(StreamEvent::ToolUseEnd)));
        assert!(streaming_tool_calls.is_empty());
        assert!(completed_tool_items.contains("fc_123"));
    }

    #[test]
    fn test_parse_openai_response_output_item_done_skips_duplicate_after_arguments_done() {
        let mut saw_text_delta = false;
        let mut streaming_tool_calls = HashMap::new();
        let mut completed_tool_items = HashSet::from(["fc_123".to_string()]);
        let mut pending = VecDeque::new();

        let duplicate_done = r#"{"type":"response.output_item.done","item":{"id":"fc_123","type":"function_call","call_id":"call_123","name":"batch","arguments":"{\"tool_calls\":[]}"}}"#;
        let event = parse_openai_response_event(
            duplicate_done,
            &mut saw_text_delta,
            &mut streaming_tool_calls,
            &mut completed_tool_items,
            &mut pending,
        );

        assert!(event.is_none(), "duplicate function call should be skipped");
        assert!(pending.is_empty());
        assert!(!completed_tool_items.contains("fc_123"));
    }

    #[test]
    fn test_parse_openai_response_output_item_done_emits_native_compaction() {
        let mut saw_text_delta = false;
        let mut streaming_tool_calls = HashMap::new();
        let mut completed_tool_items = HashSet::new();
        let mut pending = VecDeque::new();

        let compaction_done = r#"{"type":"response.output_item.done","item":{"id":"cmp_123","type":"compaction","encrypted_content":"enc_abc"}}"#;
        let event = parse_openai_response_event(
            compaction_done,
            &mut saw_text_delta,
            &mut streaming_tool_calls,
            &mut completed_tool_items,
            &mut pending,
        )
        .expect("expected compaction event");

        match event {
            StreamEvent::Compaction {
                trigger,
                pre_tokens,
                openai_encrypted_content,
            } => {
                assert_eq!(trigger, "openai_native_auto");
                assert_eq!(pre_tokens, None);
                assert_eq!(openai_encrypted_content.as_deref(), Some("enc_abc"));
            }
            other => panic!("expected Compaction, got {:?}", other),
        }
        assert!(pending.is_empty());
    }

    #[test]
    fn test_build_tools_sets_strict_true() {
        let defs = vec![ToolDefinition {
            name: "bash".to_string(),
            description: "run shell".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["command"],
                "properties": { "command": { "type": "string" } }
            }),
        }];
        let api_tools = build_tools(&defs);
        assert_eq!(api_tools.len(), 1);
        assert_eq!(api_tools[0]["strict"], serde_json::json!(true));
    }

    #[test]
    fn test_build_tools_disables_strict_for_free_form_object_nodes() {
        let defs = vec![ToolDefinition {
            name: "batch".to_string(),
            description: "batch calls".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["tool_calls"],
                "properties": {
                    "tool_calls": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["tool", "parameters"],
                            "properties": {
                                "tool": { "type": "string" },
                                "parameters": { "type": "object" }
                            }
                        }
                    }
                }
            }),
        }];
        let api_tools = build_tools(&defs);
        assert_eq!(api_tools.len(), 1);
        assert_eq!(api_tools[0]["strict"], serde_json::json!(false));
        assert_eq!(
            api_tools[0]["parameters"]["properties"]["tool_calls"]["items"]["properties"]["parameters"]
                ["type"],
            serde_json::json!("object")
        );
    }

    #[test]
    fn test_build_tools_normalizes_object_schema_additional_properties() {
        let defs = vec![ToolDefinition {
            name: "edit".to_string(),
            description: "apply edit".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "options": {
                        "type": "object",
                        "properties": {
                            "force": { "type": "boolean" }
                        }
                    },
                    "description": {
                        "type": "string"
                    }
                },
                "required": ["path"]
            }),
        }];
        let api_tools = build_tools(&defs);
        assert_eq!(
            api_tools[0]["parameters"]["additionalProperties"],
            serde_json::json!(false)
        );
        assert_eq!(
            api_tools[0]["parameters"]["properties"]["options"]["additionalProperties"],
            serde_json::json!(false)
        );
        assert_eq!(
            api_tools[0]["parameters"]["required"],
            serde_json::json!(["description", "options", "path"])
        );
        assert_eq!(
            api_tools[0]["parameters"]["properties"]["description"]["type"],
            serde_json::json!(["string", "null"])
        );
    }

    #[test]
    fn test_build_tools_rewrites_oneof_to_anyof_for_openai() {
        let defs = vec![ToolDefinition {
            name: "batch".to_string(),
            description: "batch calls".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["tool_calls"],
                "properties": {
                    "tool_calls": {
                        "type": "array",
                        "items": {
                            "oneOf": [
                                {
                                    "type": "object",
                                    "required": ["tool"],
                                    "properties": {
                                        "tool": { "type": "string" }
                                    }
                                }
                            ]
                        }
                    }
                }
            }),
        }];
        let api_tools = build_tools(&defs);
        assert!(api_tools[0]["parameters"]["properties"]["tool_calls"]["items"]["oneOf"].is_null());
        assert_eq!(
            api_tools[0]["parameters"]["properties"]["tool_calls"]["items"]["anyOf"][0]["type"],
            serde_json::json!("object")
        );
    }

    #[test]
    fn test_build_tools_keeps_strict_for_anyof_object_branches_with_properties() {
        let defs = vec![ToolDefinition {
            name: "schedule".to_string(),
            description: "schedule work".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["task"],
                "anyOf": [
                    {
                        "type": "object",
                        "required": ["wake_in_minutes"],
                        "properties": {
                            "wake_in_minutes": { "type": "integer" }
                        },
                        "additionalProperties": false
                    },
                    {
                        "type": "object",
                        "required": ["wake_at"],
                        "properties": {
                            "wake_at": { "type": "string" }
                        },
                        "additionalProperties": false
                    }
                ],
                "properties": {
                    "task": { "type": "string" },
                    "wake_in_minutes": { "type": "integer" },
                    "wake_at": { "type": "string" }
                }
            }),
        }];
        let api_tools = build_tools(&defs);
        assert_eq!(api_tools[0]["strict"], serde_json::json!(true));
        assert_eq!(
            api_tools[0]["parameters"]["anyOf"][0]["additionalProperties"],
            serde_json::json!(false)
        );
        assert_eq!(
            api_tools[0]["parameters"]["anyOf"][1]["additionalProperties"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn test_parse_text_wrapped_tool_call_prefers_trailing_json_object() {
        let text = "Status update\nassistant to=functions.batch commentary {}json\n{\"tool_calls\":[{\"tool\":\"read\",\"file_path\":\"src/main.rs\"}]}";
        let parsed = parse_text_wrapped_tool_call(text).expect("should parse wrapped tool call");
        assert_eq!(parsed.1, "batch");
        assert!(parsed.0.contains("Status update"));
        let args: Value = serde_json::from_str(&parsed.2).expect("valid args json");
        assert!(args.get("tool_calls").is_some());
    }

    #[test]
    fn test_handle_openai_output_item_normalizes_null_arguments() {
        let item = serde_json::json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "bash",
            "arguments": "null",
        });
        let mut saw_text_delta = false;
        let mut pending = VecDeque::new();
        let first = handle_openai_output_item(item, &mut saw_text_delta, &mut pending)
            .expect("expected tool event");

        match first {
            StreamEvent::ToolUseStart { id, name } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "bash");
            }
            _ => panic!("expected ToolUseStart"),
        }
        match pending.pop_front() {
            Some(StreamEvent::ToolInputDelta(delta)) => assert_eq!(delta, "{}"),
            _ => panic!("expected ToolInputDelta"),
        }
        assert!(matches!(pending.pop_front(), Some(StreamEvent::ToolUseEnd)));
    }

    #[test]
    fn test_handle_openai_output_item_recovers_bright_pearl_fixture() {
        let item = serde_json::json!({
            "type": "message",
            "content": [{
                "type": "output_text",
                "text": BRIGHT_PEARL_WRAPPED_TOOL_CALL_FIXTURE,
            }],
        });

        let mut saw_text_delta = false;
        let mut pending = VecDeque::new();
        let mut events = Vec::new();

        if let Some(first) = handle_openai_output_item(item, &mut saw_text_delta, &mut pending) {
            events.push(first);
        }
        while let Some(ev) = pending.pop_front() {
            events.push(ev);
        }

        let mut saw_prefix = false;
        let mut saw_tool = false;
        let mut saw_input = false;

        for event in events {
            match event {
                StreamEvent::TextDelta(text) => {
                    if text.contains("Status: I detected pre-existing local edits") {
                        saw_prefix = true;
                    }
                }
                StreamEvent::ToolUseStart { name, .. } => {
                    if name == "batch" {
                        saw_tool = true;
                    }
                }
                StreamEvent::ToolInputDelta(delta) => {
                    let args: Value = serde_json::from_str(&delta).expect("valid tool args");
                    let calls = args
                        .get("tool_calls")
                        .and_then(|v| v.as_array())
                        .expect("tool_calls array");
                    assert_eq!(calls.len(), 3);
                    saw_input = true;
                }
                _ => {}
            }
        }

        assert!(saw_prefix);
        assert!(saw_tool);
        assert!(saw_input);
    }

    #[test]
    fn test_build_responses_input_rewrites_orphan_tool_output_as_user_message() {
        let messages = vec![ChatMessage::tool_result(
            "call_orphan",
            "orphan result",
            false,
        )];

        let items = build_responses_input(&messages);
        let mut saw_rewritten_message = false;

        for item in &items {
            assert_ne!(
                item.get("type").and_then(|v| v.as_str()),
                Some("function_call_output")
            );
            if item.get("type").and_then(|v| v.as_str()) == Some("message")
                && item.get("role").and_then(|v| v.as_str()) == Some("user")
                && let Some(content) = item.get("content").and_then(|v| v.as_array())
            {
                for part in content {
                    if part.get("type").and_then(|v| v.as_str()) == Some("input_text") {
                        let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if text.contains("[Recovered orphaned tool output: call_orphan]")
                            && text.contains("orphan result")
                        {
                            saw_rewritten_message = true;
                        }
                    }
                }
            }
        }

        assert!(saw_rewritten_message);
    }

    #[test]
    fn test_extract_selfdev_section_missing_returns_none() {
        let system = "# Environment\nDate: 2026-01-01\n\n# Available Skills\n- test";
        assert!(extract_selfdev_section(system).is_none());
    }

    #[test]
    fn test_extract_selfdev_section_stops_at_next_top_level_header() {
        let system = "# Environment\nDate: 2026-01-01\n\n# Self-Development Mode\nUse selfdev tool\n## selfdev Tool\nreload\n\n# Available Skills\n- test";
        let section = extract_selfdev_section(system).expect("expected self-dev section");
        assert!(section.starts_with("# Self-Development Mode"));
        assert!(section.contains("Use selfdev tool"));
        assert!(section.contains("## selfdev Tool"));
        assert!(!section.contains("# Available Skills"));
    }

    #[test]
    fn test_chatgpt_instructions_with_selfdev_appends_selfdev_block() {
        let system = "# Environment\nDate: 2026-01-01\n\n# Self-Development Mode\nUse selfdev tool\n\n# Available Skills\n- test";

        let instructions = OpenAIProvider::chatgpt_instructions_with_selfdev(system);
        assert!(instructions.contains("Jcode Agent, in the Jcode harness"));
        assert!(instructions.contains("# Self-Development Mode"));
        assert!(instructions.contains("Use selfdev tool"));
    }
}
