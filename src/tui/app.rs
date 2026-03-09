#![allow(dead_code)]

use super::keybind::{CenteredToggleKeys, ModelSwitchKeys, ScrollKeys};
use super::markdown::IncrementalMarkdownRenderer;
use super::stream_buffer::StreamBuffer;
use crate::bus::{Bus, BusEvent, LoginCompleted, ToolEvent, ToolStatus};
use crate::compaction::CompactionEvent;
use crate::config::config;
use crate::id;
use crate::mcp::McpManager;
use crate::message::{
    ContentBlock, Message, Role, StreamEvent, ToolCall, TOOL_OUTPUT_MISSING_TEXT,
};
use crate::provider::Provider;
use crate::session::Session;
use crate::skill::SkillRegistry;
use crate::tool::selfdev::ReloadContext;
use crate::tool::{Registry, ToolContext};
use anyhow::Result;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use futures::StreamExt;
use auth::PendingLogin;
use debug::DebugTrace;
use helpers::*;
use ratatui::DefaultTerminal;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::interval;

mod auth;
mod commands;
mod debug;
mod helpers;
mod input;
mod local;
mod navigation;
mod picker;
mod remote;
mod replay;
mod tui_state;
mod turn;

#[derive(Debug, Clone)]
struct PendingRemoteMessage {
    content: String,
    images: Vec<(String, String)>,
    is_system: bool,
    auto_retry: bool,
    retry_attempts: u8,
    retry_at: Option<Instant>,
}

const MEMORY_INJECTION_SUPPRESSION_SECS: u64 = 90;

#[cfg(target_os = "macos")]
fn ctrl_bracket_fallback_to_esc(code: &mut KeyCode, modifiers: &mut KeyModifiers) {
    if !modifiers.contains(KeyModifiers::CONTROL) {
        return;
    }
    match code {
        KeyCode::Esc => {
            *code = KeyCode::Char('[');
        }
        KeyCode::Char('5') => {
            // Legacy tty mapping for Ctrl+]
            *code = KeyCode::Char(']');
        }
        _ => {}
    }
}

#[cfg(not(target_os = "macos"))]
fn ctrl_bracket_fallback_to_esc(_code: &mut KeyCode, _modifiers: &mut KeyModifiers) {}

/// Debug command file path
fn debug_cmd_path() -> PathBuf {
    if let Ok(path) = std::env::var("JCODE_DEBUG_CMD_PATH") {
        return PathBuf::from(path);
    }
    std::env::temp_dir().join("jcode_debug_cmd")
}

/// Debug response file path
fn debug_response_path() -> PathBuf {
    if let Ok(path) = std::env::var("JCODE_DEBUG_RESPONSE_PATH") {
        return PathBuf::from(path);
    }
    std::env::temp_dir().join("jcode_debug_response")
}

/// Parse rate limit reset time from error message
/// Returns the Duration until rate limit resets, if this is a rate limit error
fn parse_rate_limit_error(error: &str) -> Option<Duration> {
    let error_lower = error.to_lowercase();

    // Check if this is a rate limit error
    if !error_lower.contains("rate limit")
        && !error_lower.contains("rate_limit")
        && !error_lower.contains("429")
        && !error_lower.contains("too many requests")
        && !error_lower.contains("hit your limit")
    {
        return None;
    }

    // Try to extract time from common patterns

    // Pattern: "retry after X seconds" or "retry in X seconds"
    if let Some(idx) = error_lower.find("retry") {
        let after = &error_lower[idx..];
        for word in after.split_whitespace() {
            if let Ok(secs) = word
                .trim_matches(|c: char| !c.is_ascii_digit())
                .parse::<u64>()
            {
                if secs > 0 && secs < 86400 {
                    return Some(Duration::from_secs(secs));
                }
            }
        }
    }

    // Pattern: "resets Xam" or "resets Xpm" (clock time like "resets 5am")
    if let Some(idx) = error_lower.find("resets") {
        let after = &error_lower[idx..];
        for word in after.split_whitespace() {
            let word = word.trim_matches(|c: char| c == '·' || c == ' ');
            // Check for time like "5am", "12pm", "5:30am"
            if word.ends_with("am") || word.ends_with("pm") {
                if let Some(duration) = parse_clock_time_to_duration(word) {
                    return Some(duration);
                }
            }
        }
    }

    // Pattern: "reset in X seconds"
    if let Some(idx) = error_lower.find("reset") {
        let after = &error_lower[idx..];
        for word in after.split_whitespace() {
            if let Ok(secs) = word
                .trim_matches(|c: char| !c.is_ascii_digit())
                .parse::<u64>()
            {
                if secs > 0 && secs < 86400 {
                    return Some(Duration::from_secs(secs));
                }
            }
        }
    }

    // No default - only auto-retry if we know the actual reset time
    None
}

fn is_context_limit_error(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("context length")
        || lower.contains("context window")
        || lower.contains("maximum context")
        || lower.contains("max context")
        || lower.contains("token limit")
        || lower.contains("too many tokens")
        || lower.contains("prompt is too long")
        || lower.contains("input is too long")
        || lower.contains("request too large")
        || lower.contains("length limit")
        || lower.contains("maximum tokens")
        || (lower.contains("exceeded") && lower.contains("tokens"))
}

/// Parse a clock time like "5am" or "12:30pm" and return duration until that time
fn parse_clock_time_to_duration(time_str: &str) -> Option<Duration> {
    let time_lower = time_str.to_lowercase();
    let is_pm = time_lower.ends_with("pm");
    let time_part = time_lower.trim_end_matches("am").trim_end_matches("pm");

    // Parse hour (and optional minutes)
    let (hour, minute) = if time_part.contains(':') {
        let parts: Vec<&str> = time_part.split(':').collect();
        if parts.len() != 2 {
            return None;
        }
        let h: u32 = parts[0].parse().ok()?;
        let m: u32 = parts[1].parse().ok()?;
        (h, m)
    } else {
        let h: u32 = time_part.parse().ok()?;
        (h, 0)
    };

    // Convert to 24-hour format
    let hour_24 = if is_pm && hour != 12 {
        hour + 12
    } else if !is_pm && hour == 12 {
        0
    } else {
        hour
    };

    if hour_24 >= 24 || minute >= 60 {
        return None;
    }

    // Get current time and calculate duration until target time
    let now = chrono::Local::now();
    let today = now.date_naive();

    // Try today first, then tomorrow if the time has passed
    let target_time = chrono::NaiveTime::from_hms_opt(hour_24, minute, 0)?;
    let mut target_datetime = today.and_time(target_time);

    // If target time is in the past, use tomorrow
    if target_datetime <= now.naive_local() {
        target_datetime = (today + chrono::Duration::days(1)).and_time(target_time);
    }

    let duration_secs = (target_datetime - now.naive_local()).num_seconds();
    if duration_secs > 0 {
        Some(Duration::from_secs(duration_secs as u64))
    } else {
        None
    }
}

fn format_cache_footer(read_tokens: Option<u64>, write_tokens: Option<u64>) -> Option<String> {
    let _ = (read_tokens, write_tokens);
    None
}

/// Format token count for display (e.g., 63000 -> "63K")
fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.0}k", tokens as f64 / 1_000.0)
    } else {
        format!("{}", tokens)
    }
}

/// Current processing status
#[derive(Clone, Default, Debug)]
pub enum ProcessingStatus {
    #[default]
    Idle,
    /// Sending request to API (with optional connection phase detail)
    Sending,
    /// Connection phase update from transport layer
    Connecting(crate::message::ConnectionPhase),
    /// Model is reasoning/thinking (real-time duration tracking)
    Thinking(Instant),
    /// Receiving streaming response
    Streaming,
    /// Executing a tool
    RunningTool(String),
}

/// A message in the conversation for display
#[derive(Clone)]
pub struct DisplayMessage {
    pub role: String,
    pub content: String,
    pub tool_calls: Vec<String>,
    pub duration_secs: Option<f32>,
    pub title: Option<String>,
    /// Full tool call data (for role="tool" messages)
    pub tool_data: Option<ToolCall>,
}

/// Result from running the TUI
#[derive(Debug, Default)]
pub struct RunResult {
    /// Session ID to reload (hot-reload, no rebuild)
    pub reload_session: Option<String>,
    /// Session ID to rebuild (full git pull + cargo build + tests)
    pub rebuild_session: Option<String>,
    /// Session ID to update (download from GitHub releases and reload)
    pub update_session: Option<String>,
    /// Exit code to use (for canary wrapper communication)
    pub exit_code: Option<i32>,
    /// The session ID that was active (for resume hints on exit)
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendAction {
    Submit,
    Queue,
    Interleave,
}


/// State for an in-progress OAuth/API-key login flow triggered by `/login`.

/// TUI Application state
pub struct App {
    provider: Arc<dyn Provider>,
    registry: Registry,
    skills: SkillRegistry,
    mcp_manager: Arc<RwLock<McpManager>>,
    messages: Vec<Message>,
    session: Session,
    display_messages: Vec<DisplayMessage>,
    display_messages_version: u64,
    input: String,
    cursor_pos: usize,
    scroll_offset: usize,
    /// Pauses auto-scroll when user scrolls up during streaming
    auto_scroll_paused: bool,
    active_skill: Option<String>,
    is_processing: bool,
    streaming_text: String,
    should_quit: bool,
    // Message queueing
    queued_messages: Vec<String>,
    // Live token usage (per turn)
    streaming_input_tokens: u64,
    streaming_output_tokens: u64,
    streaming_cache_read_tokens: Option<u64>,
    streaming_cache_creation_tokens: Option<u64>,
    // Upstream provider (e.g., which provider OpenRouter routed to)
    upstream_provider: Option<String>,
    // Active stream connection type (websocket/https/etc.)
    connection_type: Option<String>,
    // Total session token usage (accumulated across all turns)
    total_input_tokens: u64,
    total_output_tokens: u64,
    // Total cost in USD (for API-key providers)
    total_cost: f32,
    // Cached pricing (input $/1M tokens, output $/1M tokens)
    cached_prompt_price: Option<f32>,
    cached_completion_price: Option<f32>,
    // Context limit tracking (for compaction warning)
    context_limit: u64,
    context_warning_shown: bool,
    // Context info (what's loaded in system prompt)
    context_info: crate::prompt::ContextInfo,
    // Track last streaming activity for "stale" detection
    last_stream_activity: Option<Instant>,
    // Accurate TPS tracking: only counts actual token streaming time, not tool execution
    /// Set when first TextDelta arrives in a streaming response
    streaming_tps_start: Option<Instant>,
    /// Accumulated streaming-only time across agentic loop iterations
    streaming_tps_elapsed: Duration,
    /// Accumulated output tokens across all API calls in a turn.
    ///
    /// Providers may emit repeated cumulative usage snapshots for a single API call,
    /// so we accumulate per-call deltas to avoid double counting.
    streaming_total_output_tokens: u64,
    // Current status
    status: ProcessingStatus,
    // Subagent status (shown during Task tool execution)
    subagent_status: Option<String>,
    processing_started: Option<Instant>,
    // When the last API response completed (for cache TTL tracking)
    last_api_completed: Option<Instant>,
    // Input tokens from the last completed turn (for cache TTL display)
    last_turn_input_tokens: Option<u64>,
    // Pending turn to process (allows UI to redraw before processing starts)
    pending_turn: bool,
    // Tool calls detected during streaming (shown in real-time with details)
    streaming_tool_calls: Vec<ToolCall>,
    // Provider-specific session ID for conversation resume
    provider_session_id: Option<String>,
    // Cancel flag for interrupting generation
    cancel_requested: bool,
    // Quit confirmation: tracks when first Ctrl+C was pressed
    quit_pending: Option<Instant>,
    // Cached MCP server names and tool counts (updated on connect/disconnect)
    mcp_server_names: Vec<(String, usize)>,
    // Semantic stream buffer for chunked output
    stream_buffer: StreamBuffer,
    // Track thinking start time for extended thinking display
    thinking_start: Option<Instant>,
    // Whether we've inserted the current turn's thought line
    thought_line_inserted: bool,
    // Buffer for accumulating thinking content during a thinking session
    thinking_buffer: String,
    // Whether we've emitted the 💭 prefix for the current thinking session
    thinking_prefix_emitted: bool,
    // Hot-reload: if set, exec into new binary with this session ID (no rebuild)
    reload_requested: Option<String>,
    // Hot-rebuild: if set, do full git pull + cargo build + tests then exec
    rebuild_requested: Option<String>,
    // Update: if set, check for and download update from GitHub releases then exec
    update_requested: Option<String>,
    // Pasted content storage (displayed as placeholders, expanded on submit)
    pasted_contents: Vec<String>,
    // Pending pasted images (media_type, base64_data) attached to next message
    pending_images: Vec<(String, String)>,
    // Debug socket broadcast channel (if enabled)
    debug_tx: Option<tokio::sync::broadcast::Sender<super::backend::DebugEvent>>,
    // Remote provider info (set when running in remote mode)
    remote_provider_name: Option<String>,
    remote_provider_model: Option<String>,
    remote_reasoning_effort: Option<String>,
    remote_available_models: Vec<String>,
    remote_model_routes: Vec<crate::provider::ModelRoute>,
    // Remote MCP servers and skills (set from server in remote mode)
    remote_mcp_servers: Vec<String>,
    remote_skills: Vec<String>,
    // Total session token usage (from server in remote mode)
    remote_total_tokens: Option<(u64, u64)>,
    // Whether the remote session is canary/self-dev (from server)
    remote_is_canary: Option<bool>,
    // Remote server version (from server)
    remote_server_version: Option<String>,
    // Whether the remote server has a newer binary available
    remote_server_has_update: Option<bool>,
    // Auto-reload server when stale (set on first connect if server_has_update)
    pending_server_reload: bool,
    // Remote server short name (e.g., "running", "blazing")
    remote_server_short_name: Option<String>,
    // Remote server icon (e.g., "🔥", "🌫️")
    remote_server_icon: Option<String>,
    // Current message request ID (for remote mode - to match Done events)
    current_message_id: Option<u64>,
    // Whether running in remote mode
    is_remote: bool,
    // Server was just spawned - allow initial connection retries in run_remote
    server_spawning: bool,
    // Whether running in replay mode (readonly playback of a saved session)
    pub is_replay: bool,
    /// Override for elapsed time during headless video replay.
    pub replay_elapsed_override: Option<Duration>,
    /// Sim-time at which processing started (video replay only)
    replay_processing_started_ms: Option<f64>,
    // Remember tool call ids that already have outputs
    tool_result_ids: HashSet<String>,
    // Current session ID (from server in remote mode)
    remote_session_id: Option<String>,
    // All sessions on the server (remote mode only)
    remote_sessions: Vec<String>,
    // Swarm member status snapshots (remote mode only)
    remote_swarm_members: Vec<crate::protocol::SwarmMemberStatus>,
    // Latest swarm plan snapshot (local or remote server event stream)
    swarm_plan_items: Vec<crate::plan::PlanItem>,
    swarm_plan_version: Option<u64>,
    swarm_plan_swarm_id: Option<String>,
    // Number of connected clients (remote mode only)
    remote_client_count: Option<usize>,
    // Build version tracking for auto-migration
    known_stable_version: Option<String>,
    // Last time we checked for stable version
    last_version_check: Option<Instant>,
    // Pending migration to new stable version
    pending_migration: Option<String>,
    // Session to resume on connect (remote mode)
    resume_session_id: Option<String>,
    // Exit code to use when quitting (for canary wrapper communication)
    requested_exit_code: Option<i32>,
    // Memory feature toggle for this session
    memory_enabled: bool,
    // Suppress duplicate memory injection messages for near-identical prompts.
    last_injected_memory_signature: Option<(String, Instant)>,
    // Swarm feature toggle for this session
    swarm_enabled: bool,
    // Diff display mode (toggle with Shift+Tab)
    diff_mode: crate::config::DiffDisplayMode,
    // Center all content (from config)
    pub(crate) centered: bool,
    // Diagram display mode (from config)
    diagram_mode: crate::config::DiagramDisplayMode,
    // Whether the pinned diagram pane has focus
    diagram_focus: bool,
    // Selected diagram index in pinned mode (most recent = 0)
    diagram_index: usize,
    // Diagram scroll offsets in cells (only used when focused)
    diagram_scroll_x: i32,
    diagram_scroll_y: i32,
    // Diagram pane width ratio (percentage)
    diagram_pane_ratio: u8,
    // Animation state for smooth pane ratio transitions
    diagram_pane_ratio_from: u8,
    diagram_pane_ratio_target: u8,
    diagram_pane_anim_start: Option<Instant>,
    // Whether the pinned diagram pane is visible
    diagram_pane_enabled: bool,
    // Position of pinned diagram pane (side or top)
    diagram_pane_position: crate::config::DiagramPanePosition,
    // Diagram zoom percentage (100 = normal)
    diagram_zoom: u8,
    // Whether the user is dragging the diagram pane border
    diagram_pane_dragging: bool,
    // Scroll offset for pinned diff pane
    diff_pane_scroll: usize,
    diff_pane_focus: bool,
    diff_pane_auto_scroll: bool,
    // Pin read images to side pane
    pin_images: bool,
    // Interactive model/provider picker
    picker_state: Option<super::PickerState>,
    // Pending model switch from picker (for remote mode async processing)
    pending_model_switch: Option<String>,
    // Keybindings for model switching
    model_switch_keys: ModelSwitchKeys,
    // Keybindings for effort switching
    effort_switch_keys: super::keybind::EffortSwitchKeys,
    // Keybindings for scrolling
    scroll_keys: ScrollKeys,
    // Keybinding for centered-mode toggle
    centered_toggle_keys: CenteredToggleKeys,
    // Scroll bookmark: stashed scroll position for quick teleport back
    scroll_bookmark: Option<usize>,
    // Stashed input: saved via Ctrl+S for later retrieval
    stashed_input: Option<(String, usize)>,
    // Short-lived notice for status feedback (model switch, cycle diff mode, etc.)
    status_notice: Option<(String, Instant)>,
    // Message to interleave during processing (set via Shift+Enter)
    interleave_message: Option<String>,
    // Message sent as soft interrupt but not yet injected (shown in queue preview until injected)
    pending_soft_interrupts: Vec<String>,
    // Queue mode: if true, Enter during processing queues; if false, Enter queues to send next
    // Toggle with Ctrl+Tab or Ctrl+T
    queue_mode: bool,
    // Tab completion state: (base_input, suggestion_index)
    // base_input is the original input before cycling, suggestion_index is current position
    tab_completion_state: Option<(String, usize)>,
    // Time when app started (for startup animations)
    app_started: Instant,
    // Binary modification time when client started (for smart reload detection)
    client_binary_mtime: Option<std::time::SystemTime>,
    // Rate limit state: when rate limit resets (if rate limited)
    rate_limit_reset: Option<Instant>,
    // Message being sent when rate limit hit (to auto-retry in remote mode)
    rate_limit_pending_message: Option<PendingRemoteMessage>,
    // Last turn-level stream error (used by /fix to choose recovery actions)
    last_stream_error: Option<String>,
    // Store reload info to pass to agent after reconnection (remote mode)
    reload_info: Vec<String>,
    // Debug trace for scripted testing
    debug_trace: DebugTrace,
    // Incremental markdown renderer for streaming text (uses RefCell for interior mutability)
    streaming_md_renderer: RefCell<IncrementalMarkdownRenderer>,
    /// Ambient mode system prompt override (when running as visible ambient cycle)
    ambient_system_prompt: Option<String>,
    /// Pending login flow: if set, next input is intercepted as OAuth code or API key
    pending_login: Option<PendingLogin>,
    /// Last mouse scroll event timestamp (for trackpad velocity detection)
    last_mouse_scroll: Option<Instant>,
    /// Scroll offset for changelog overlay (None = not visible)
    changelog_scroll: Option<usize>,
    help_scroll: Option<usize>,
    /// Session picker overlay (None = not visible)
    session_picker_overlay: Option<RefCell<super::session_picker::SessionPicker>>,
}


/// A placeholder provider for remote mode (never actually called)
struct NullProvider;

#[async_trait::async_trait]
impl Provider for NullProvider {
    fn name(&self) -> &str {
        "remote"
    }
    fn model(&self) -> String {
        "unknown".to_string()
    }

    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[crate::message::ToolDefinition],
        _system: &str,
        _session_id: Option<&str>,
    ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>> {
        Err(anyhow::anyhow!(
            "NullProvider cannot be used for completion"
        ))
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(NullProvider)
    }
}

impl App {
    const AUTO_RETRY_BASE_DELAY_SECS: u64 = 2;
    const AUTO_RETRY_MAX_ATTEMPTS: u8 = 3;

    async fn begin_remote_send(
        &mut self,
        remote: &mut super::backend::RemoteConnection,
        content: String,
        images: Vec<(String, String)>,
        is_system: bool,
    ) -> Result<u64> {
        remote::begin_remote_send(self, remote, content, images, is_system, false, 0).await
    }

    fn schedule_pending_remote_retry(&mut self, reason: &str) -> bool {
        let Some(pending) = self.rate_limit_pending_message.as_mut() else {
            return false;
        };
        if !pending.auto_retry {
            return false;
        }
        let outcome = {
            let current_attempts = pending.retry_attempts;
            if current_attempts >= Self::AUTO_RETRY_MAX_ATTEMPTS {
                Err(current_attempts)
            } else {
                pending.retry_attempts += 1;
                let retry_attempts = pending.retry_attempts;
                let backoff_secs = Self::AUTO_RETRY_BASE_DELAY_SECS * u64::from(retry_attempts);
                let retry_at = Instant::now() + Duration::from_secs(backoff_secs);
                pending.retry_at = Some(retry_at);
                Ok((retry_attempts, backoff_secs, retry_at))
            }
        };

        match outcome {
            Err(current_attempts) => {
                self.rate_limit_pending_message = None;
                self.rate_limit_reset = None;
                self.push_display_message(DisplayMessage::error(format!(
                    "{} Auto-retry limit reached after {} attempt{}. Use `/poke` again to retry manually.",
                    reason,
                    current_attempts,
                    if current_attempts == 1 { "" } else { "s" }
                )));
                false
            }
            Ok((retry_attempts, backoff_secs, retry_at)) => {
                self.rate_limit_reset = Some(retry_at);
                self.push_display_message(DisplayMessage::system(format!(
                    "{} Auto-retrying in {} second{} (attempt {}/{}).",
                    reason,
                    backoff_secs,
                    if backoff_secs == 1 { "" } else { "s" },
                    retry_attempts,
                    Self::AUTO_RETRY_MAX_ATTEMPTS
                )));
                true
            }
        }
    }

    fn clear_pending_remote_retry(&mut self) {
        self.rate_limit_pending_message = None;
        self.rate_limit_reset = None;
    }

    pub fn new(provider: Arc<dyn Provider>, registry: Registry) -> Self {
        let t0 = std::time::Instant::now();
        let skills = SkillRegistry::load().unwrap_or_default();
        let t_skills = t0.elapsed();
        let mcp_manager = Arc::new(RwLock::new(McpManager::new()));
        let mut session = Session::create(None, None);
        session.model = Some(provider.model());
        let display = config().display.clone();
        let features = config().features.clone();
        let context_limit = provider.context_window() as u64;
        let t_session = t0.elapsed();

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let provider_clone = Arc::clone(&provider);
            handle.spawn(async move {
                let _ = provider_clone.prefetch_models().await;
            });
        }

        // Pre-compute context info so it shows on startup
        let available_skills: Vec<crate::prompt::SkillInfo> = skills
            .list()
            .iter()
            .map(|s| crate::prompt::SkillInfo {
                name: s.name.clone(),
                description: s.description.clone(),
            })
            .collect();
        let (_, context_info) = crate::prompt::build_system_prompt_with_context(
            None,
            &available_skills,
            session.is_canary,
        );
        let t_prompt = t0.elapsed();
        crate::logging::info(&format!(
            "App::new timings: skills={:.1}ms session={:.1}ms prompt={:.1}ms total={:.1}ms",
            t_skills.as_secs_f64() * 1000.0,
            (t_session - t_skills).as_secs_f64() * 1000.0,
            (t_prompt - t_session).as_secs_f64() * 1000.0,
            t_prompt.as_secs_f64() * 1000.0,
        ));

        let mut app = Self {
            provider,
            registry,
            skills,
            mcp_manager,
            messages: Vec::new(),
            session,
            display_messages: Vec::new(),
            display_messages_version: 0,
            input: String::new(),
            cursor_pos: 0,
            scroll_offset: 0,
            auto_scroll_paused: false,
            active_skill: None,
            is_processing: false,
            streaming_text: String::new(),
            should_quit: false,
            queued_messages: Vec::new(),
            streaming_input_tokens: 0,
            streaming_output_tokens: 0,
            streaming_cache_read_tokens: None,
            streaming_cache_creation_tokens: None,
            upstream_provider: None,
            connection_type: None,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cost: 0.0,
            cached_prompt_price: None,
            cached_completion_price: None,
            context_limit,
            context_warning_shown: false,
            context_info,
            last_stream_activity: None,
            streaming_tps_start: None,
            streaming_tps_elapsed: Duration::ZERO,
            streaming_total_output_tokens: 0,
            status: ProcessingStatus::default(),
            subagent_status: None,
            processing_started: None,
            last_api_completed: None,
            last_turn_input_tokens: None,
            pending_turn: false,
            streaming_tool_calls: Vec::new(),
            provider_session_id: None,
            cancel_requested: false,
            quit_pending: None,
            mcp_server_names: Vec::new(), // Vec<(name, tool_count)>
            stream_buffer: StreamBuffer::new(),
            thinking_start: None,
            thought_line_inserted: false,
            thinking_buffer: String::new(),
            thinking_prefix_emitted: false,
            reload_requested: None,
            rebuild_requested: None,
            update_requested: None,
            pasted_contents: Vec::new(),
            pending_images: Vec::new(),
            debug_tx: None,
            remote_provider_name: None,
            remote_provider_model: None,
            remote_reasoning_effort: None,
            remote_available_models: Vec::new(),
            remote_model_routes: Vec::new(),
            remote_mcp_servers: Vec::new(),
            remote_skills: Vec::new(),
            remote_total_tokens: None,
            remote_is_canary: None,
            remote_server_version: None,
            remote_server_has_update: None,
            pending_server_reload: false,
            remote_server_short_name: None,
            remote_server_icon: None,
            current_message_id: None,
            is_remote: false,
            server_spawning: false,
            is_replay: false,
            replay_elapsed_override: None,
            replay_processing_started_ms: None,
            tool_result_ids: HashSet::new(),
            remote_session_id: None,
            remote_sessions: Vec::new(),
            remote_swarm_members: Vec::new(),
            swarm_plan_items: Vec::new(),
            swarm_plan_version: None,
            swarm_plan_swarm_id: None,
            known_stable_version: crate::build::read_stable_version().ok().flatten(),
            last_version_check: Some(Instant::now()),
            pending_migration: None,
            remote_client_count: None,
            resume_session_id: None,
            requested_exit_code: None,
            memory_enabled: features.memory,
            last_injected_memory_signature: None,
            swarm_enabled: features.swarm,
            diff_mode: display.diff_mode,
            centered: display.centered,
            diagram_mode: display.diagram_mode,
            diagram_focus: false,
            diagram_index: 0,
            diagram_scroll_x: 0,
            diagram_scroll_y: 0,
            diagram_pane_ratio: 40,
            diagram_pane_ratio_from: 40,
            diagram_pane_ratio_target: 40,
            diagram_pane_anim_start: None,
            diagram_pane_enabled: true,
            diagram_pane_position: crate::config::DiagramPanePosition::default(),
            diagram_zoom: 100,
            diagram_pane_dragging: false,
            diff_pane_scroll: 0,
            diff_pane_focus: false,
            diff_pane_auto_scroll: true,
            pin_images: display.pin_images,
            picker_state: None,
            pending_model_switch: None,
            model_switch_keys: super::keybind::load_model_switch_keys(),
            effort_switch_keys: super::keybind::load_effort_switch_keys(),
            centered_toggle_keys: super::keybind::load_centered_toggle_key(),
            scroll_keys: super::keybind::load_scroll_keys(),
            scroll_bookmark: None,
            stashed_input: None,
            status_notice: None,
            interleave_message: None,
            pending_soft_interrupts: Vec::new(),
            queue_mode: display.queue_mode,
            tab_completion_state: None,
            app_started: Instant::now(),
            client_binary_mtime: std::env::current_exe()
                .ok()
                .and_then(|p| std::fs::metadata(&p).ok())
                .and_then(|m| m.modified().ok()),
            rate_limit_reset: None,
            rate_limit_pending_message: None,
            last_stream_error: None,
            reload_info: Vec::new(),
            debug_trace: DebugTrace::new(),
            streaming_md_renderer: RefCell::new(IncrementalMarkdownRenderer::new(None)),
            ambient_system_prompt: None,
            pending_login: None,
            last_mouse_scroll: None,
            changelog_scroll: None,
            help_scroll: None,
            session_picker_overlay: None,
        };

        for notice in app.provider.drain_startup_notices() {
            app.status_notice = Some((notice, Instant::now()));
        }

        app
    }

    /// Configure ambient mode: override system prompt and queue an initial message.
    pub fn set_ambient_mode(&mut self, system_prompt: String, initial_message: String) {
        self.ambient_system_prompt = Some(system_prompt);
        crate::tool::ambient::register_ambient_session(self.session.id.clone());
        self.queued_messages.push(initial_message);
        self.is_processing = true;
        self.status = ProcessingStatus::Sending;
        self.processing_started = Some(Instant::now());
        self.pending_turn = true;
    }

    /// Queue a startup message that should be auto-sent when the TUI starts.
    pub fn queue_startup_message(&mut self, message: String) {
        if message.trim().is_empty() {
            return;
        }
        self.queued_messages.push(message);
        self.is_processing = true;
        self.status = ProcessingStatus::Sending;
        self.processing_started = Some(Instant::now());
        self.pending_turn = true;
    }

    /// Create an App instance for remote mode (connecting to server)
    pub fn new_for_remote(resume_session: Option<String>) -> Self {
        let provider: Arc<dyn Provider> = Arc::new(NullProvider);
        let registry = Registry::empty();
        let mut app = Self::new(provider, registry);
        app.is_remote = true;

        // Load session to get canary status (for "client self-dev" badge)
        if let Some(ref session_id) = resume_session {
            if let Ok(session) = Session::load(session_id) {
                app.session = session;
            }
            if let Some((input, cursor)) = Self::restore_input_from_reload(session_id) {
                app.input = input;
                app.cursor_pos = cursor;
            }
        }

        app.resume_session_id = resume_session;
        app
    }

    /// Mark that a server was just spawned - run_remote will retry initial connection
    /// instead of failing fatally, allowing the TUI to show while the server starts.
    pub fn set_server_spawning(&mut self) {
        self.server_spawning = true;
    }

    /// Create an App instance for replay mode (playing back a saved session)
    pub fn new_for_replay(session: crate::session::Session) -> Self {
        let provider: Arc<dyn Provider> = Arc::new(NullProvider);
        let registry = Registry::empty();
        let mut app = Self::new(provider, registry);
        app.is_remote = false;
        app.is_replay = true;
        let model_name = session.model.clone().unwrap_or_default();
        let session_name = session.short_name.clone().unwrap_or_default();

        // Set provider/model info so status widgets show correct values
        let effective_model = if model_name.is_empty() {
            // Try to infer model from message content (e.g., usage events)
            // Default to a sensible value for demo purposes
            "claude-sonnet-4-20250514".to_string()
        } else {
            model_name
        };
        app.remote_provider_model = Some(effective_model.clone());
        // Infer provider name from model string
        let provider_name = if effective_model.contains("claude")
            || effective_model.contains("opus")
            || effective_model.contains("sonnet")
            || effective_model.contains("haiku")
        {
            "anthropic"
        } else if effective_model.contains("gpt")
            || effective_model.contains("o1")
            || effective_model.contains("o3")
            || effective_model.contains("o4")
        {
            "openai"
        } else if effective_model.contains('/') {
            "openrouter"
        } else {
            "claude"
        };
        app.remote_provider_name = Some(provider_name.to_string());

        app.session = session;
        if !session_name.is_empty() {
            let icon = crate::id::session_icon(&session_name);
            let _ = crossterm::execute!(
                std::io::stdout(),
                crossterm::terminal::SetTitle(format!("{} replay: {}", icon, session_name))
            );
        }
        app
    }

    /// Get the current session ID
    pub fn session_id(&self) -> &str {
        &self.session.id
    }

    fn update_terminal_title(&self) {
        let session_id = if self.is_remote {
            self.remote_session_id
                .as_deref()
                .unwrap_or(&self.session.id)
        } else {
            &self.session.id
        };
        let session_name = crate::id::extract_session_name(session_id)
            .map(|s| s.to_string())
            .unwrap_or_else(|| session_id.to_string());
        let session_icon = crate::id::session_icon(&session_name);
        let is_canary = if self.is_remote {
            self.remote_is_canary.unwrap_or(self.session.is_canary)
        } else {
            self.session.is_canary
        };
        let suffix = if is_canary { " [self-dev]" } else { "" };
        let server_name = self.remote_server_short_name.as_deref().unwrap_or("jcode");
        let server_icon = self.remote_server_icon.as_deref().unwrap_or("");
        let icons = if server_icon.is_empty() {
            session_icon.to_string()
        } else {
            format!("{}{}", server_icon, session_icon)
        };
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::SetTitle(format!(
                "{} {} {}{}",
                super::ui::capitalize(server_name),
                super::ui::capitalize(&session_name),
                icons,
                suffix
            ))
        );
    }

    fn reconnect_target_session_id(&self) -> Option<String> {
        self.remote_session_id
            .clone()
            .or_else(|| self.resume_session_id.clone())
    }

    /// Check if the selected reload candidate is newer than startup.
    /// Candidate selection matches `/reload` so the `cli↑` badge and reload target stay aligned.
    fn has_newer_binary(&self) -> bool {
        let Some(startup_mtime) = self.client_binary_mtime else {
            return false;
        };

        let is_selfdev_session = if self.is_remote {
            self.remote_is_canary.unwrap_or(self.session.is_canary)
        } else {
            self.session.is_canary
        };

        let Some((candidate, _label)) = crate::build::client_update_candidate(is_selfdev_session)
        else {
            return false;
        };

        std::fs::metadata(&candidate)
            .ok()
            .and_then(|m| m.modified().ok())
            .is_some_and(|mtime| mtime > startup_mtime)
    }

    /// Initialize MCP servers (call after construction)
    pub async fn init_mcp(&mut self) {
        // Always register the MCP management tool so agent can connect servers
        let mcp_tool = crate::tool::mcp::McpManagementTool::new(Arc::clone(&self.mcp_manager))
            .with_registry(self.registry.clone());
        self.registry
            .register("mcp".to_string(), Arc::new(mcp_tool))
            .await;

        let manager = self.mcp_manager.read().await;
        let server_count = manager.config().servers.len();
        if server_count > 0 {
            drop(manager);

            // Log configured servers
            crate::logging::info(&format!("MCP: Found {} server(s) in config", server_count));

            let (successes, failures) = {
                let manager = self.mcp_manager.write().await;
                let result = manager.connect_all().await.unwrap_or((0, Vec::new()));
                // Cache server names with tool counts
                let servers = manager.connected_servers().await;
                let all_tools = manager.all_tools().await;
                self.mcp_server_names = servers
                    .into_iter()
                    .map(|name| {
                        let count = all_tools.iter().filter(|(s, _)| s == &name).count();
                        (name, count)
                    })
                    .collect();
                result
            };

            // Show connection results
            if successes > 0 {
                let msg = format!("MCP: Connected to {} server(s)", successes);
                crate::logging::info(&msg);
                self.set_status_notice(&format!("mcp: {} connected", successes));
            }

            if !failures.is_empty() {
                for (name, error) in &failures {
                    let msg = format!("MCP '{}' failed: {}", name, error);
                    self.push_display_message(DisplayMessage::error(msg));
                }
                if successes == 0 {
                    self.set_status_notice("MCP: all connections failed");
                }
            }

            // Register MCP server tools
            let tools = crate::mcp::create_mcp_tools(Arc::clone(&self.mcp_manager)).await;
            for (name, tool) in tools {
                self.registry.register(name, tool).await;
            }
        }

        // Register self-dev tools if this is a canary session
        if self.session.is_canary {
            self.registry.register_selfdev_tools().await;
        }
    }

    /// Restore a previous session (for hot-reload)
    pub fn restore_session(&mut self, session_id: &str) {
        if let Some((input, cursor)) = Self::restore_input_from_reload(session_id) {
            self.input = input;
            self.cursor_pos = cursor;
        }
        if let Ok(session) = Session::load(session_id) {
            // Count stats before restoring
            let mut user_turns = 0;
            let mut assistant_turns = 0;
            let mut total_chars = 0;

            // Convert session messages to display messages (including tools)
            for item in crate::session::render_messages(&session) {
                if item.role == "user" {
                    user_turns += 1;
                } else if item.role == "assistant" {
                    assistant_turns += 1;
                }
                total_chars += item.content.len();

                self.push_display_message(DisplayMessage {
                    role: item.role,
                    content: item.content,
                    tool_calls: item.tool_calls,
                    duration_secs: None,
                    title: None,
                    tool_data: item.tool_data,
                });
            }

            // Don't restore provider_session_id - Claude sessions don't persist across
            // process restarts. The messages are restored, so Claude will get full context.
            self.provider_session_id = None;
            self.session = session;
            self.replace_provider_messages(self.session.messages_for_provider());
            // Clear the saved provider_session_id since it's no longer valid
            self.session.provider_session_id = None;
            let mut restored_model = false;
            if let Some(model) = self.session.model.clone() {
                if let Err(e) = self.provider.set_model(&model) {
                    self.push_display_message(DisplayMessage {
                        role: "system".to_string(),
                        content: format!("⚠ Failed to restore model '{}': {}", model, e),
                        tool_calls: vec![],
                        duration_secs: None,
                        title: None,
                        tool_data: None,
                    });
                } else {
                    restored_model = true;
                }
            }

            let active_model = self.provider.model();
            if restored_model || self.session.model.is_none() {
                self.session.model = Some(active_model.clone());
            }
            self.update_context_limit_for_model(&active_model);
            // Mark session as active now that it's being used again
            self.session.mark_active();
            crate::logging::info(&format!("Restored session: {}", session_id));

            // Build stats message
            let total_turns = user_turns + assistant_turns;
            let estimated_tokens = total_chars / 4; // Rough estimate: ~4 chars per token
            let stats = if total_turns > 0 {
                format!(
                    " ({} turns, ~{}k tokens)",
                    total_turns,
                    estimated_tokens / 1000
                )
            } else {
                String::new()
            };

            // Check for reload info to show what triggered the reload
            let reload_info = if let Ok(jcode_dir) = crate::storage::jcode_dir() {
                let info_path = jcode_dir.join("reload-info");
                if info_path.exists() {
                    let info = std::fs::read_to_string(&info_path).ok();
                    let _ = std::fs::remove_file(&info_path); // Clean up
                    info
                } else {
                    None
                }
            } else {
                None
            };

            // Build the reload message based on what triggered it
            // Extract build hash for the AI notification
            let is_reload = reload_info.is_some();
            let (message, build_hash) = if let Some(info) = reload_info {
                if let Some(hash) = info.strip_prefix("reload:") {
                    let h = hash.trim().to_string();
                    (
                        format!("✓ Reloaded with build {}. Session restored{}", h, stats),
                        h,
                    )
                } else if let Some(hash) = info.strip_prefix("rebuild:") {
                    let h = hash.trim().to_string();
                    (
                        format!("✓ Rebuilt and reloaded ({}). Session restored{}", h, stats),
                        h,
                    )
                } else {
                    (
                        format!("✓ JCode reloaded. Session restored{}", stats),
                        "unknown".to_string(),
                    )
                }
            } else {
                (
                    format!("✓ JCode reloaded. Session restored{}", stats),
                    "unknown".to_string(),
                )
            };

            // Add success message with stats (only if there's actual content or a reload happened)
            if total_turns > 0 || is_reload {
                self.push_display_message(DisplayMessage {
                    role: "system".to_string(),
                    content: message,
                    tool_calls: vec![],
                    duration_secs: None,
                    title: None,
                    tool_data: None,
                });
            }

            // Queue an automatic message to notify the AI that reload completed
            let reload_ctx = ReloadContext::load_for_session(session_id).ok().flatten();
            if let Some(ctx) = reload_ctx {
                // This session initiated the reload - send the reload-specific continuation
                let task_info = ctx
                    .task_context
                    .map(|t| format!("\nTask context: {}", t))
                    .unwrap_or_default();

                let continuation_msg = format!(
                    "[SYSTEM: Reload succeeded. Build {} → {}.{}\nSession restored with {} turns.\nIMPORTANT: The reload is done. You MUST immediately continue your work. Do NOT ask the user what to do next. Do NOT summarize what happened. Just pick up exactly where you left off and keep going.]",
                    ctx.version_before,
                    ctx.version_after,
                    task_info,
                    total_turns
                );

                crate::logging::info(&format!(
                    "Queuing reload continuation message ({} chars)",
                    continuation_msg.len()
                ));
                self.queued_messages.push(continuation_msg);
                // Trigger processing so the queued message gets sent to the LLM.
                // Without this, the local event loop waits for user input since
                // process_queued_messages only runs inside process_turn_with_input.
                self.is_processing = true;
                self.status = ProcessingStatus::Sending;
                self.processing_started = Some(Instant::now());
                self.pending_turn = true;
            } else if self.was_interrupted_by_reload() {
                // This session was interrupted by another session's reload.
                // The conversation has incomplete tool results - auto-continue.
                crate::logging::info(
                    "Session was interrupted by reload (not initiator), queuing continuation",
                );
                self.push_display_message(DisplayMessage::system(
                    "⚡ Session was interrupted by server reload. Continuing...".to_string(),
                ));
                self.queued_messages.push(
                    "[SYSTEM: Your session was interrupted by a server reload while a tool was running. \
                     The tool was aborted and its results may be incomplete. \
                     Please continue exactly where you left off. Look at the conversation history \
                     to understand what you were doing and resume immediately. \
                     Do NOT ask the user what to do - just continue your work.]"
                        .to_string(),
                );
                self.is_processing = true;
                self.status = ProcessingStatus::Sending;
                self.processing_started = Some(Instant::now());
                self.pending_turn = true;
            }
        } else {
            crate::logging::error(&format!("Failed to restore session: {}", session_id));

            // Check if this was a reload that failed - inject failure message if so
            if let Ok(Some(ctx)) = ReloadContext::load_for_session(session_id) {
                let task_info = ctx
                    .task_context
                    .map(|t| format!(" You were working on: {}", t))
                    .unwrap_or_default();

                self.push_display_message(DisplayMessage {
                    role: "system".to_string(),
                    content: format!(
                        "⚠ Reload failed. Session could not be restored. Previous version: {}, Target version: {}.{}\n\
                         Starting fresh session. You may need to re-examine your changes.",
                        ctx.version_before,
                        ctx.version_after,
                        task_info
                    ),
                    tool_calls: vec![],
                    duration_secs: None,
                    title: None,
                    tool_data: None,
                });
            }
        }
    }

    /// Check if the current session was interrupted by a server reload.
    /// Detects two patterns:
    /// 1. Last message is a User ToolResult containing reload interruption text
    /// 2. Last assistant message ends with "[generation interrupted - server reloading]"
    fn was_interrupted_by_reload(&self) -> bool {
        use crate::message::{ContentBlock, Role};
        let messages = &self.session.messages;
        if messages.is_empty() {
            return false;
        }
        let last = &messages[messages.len() - 1];
        match last.role {
            Role::User => last.content.iter().any(|block| match block {
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    is_error.unwrap_or(false)
                        && (content.contains("interrupted by server reload")
                            || content.contains("Skipped - server reloading"))
                }
                _ => false,
            }),
            Role::Assistant => last.content.iter().any(|block| match block {
                ContentBlock::Text { text, .. } => {
                    text.ends_with("[generation interrupted - server reloading]")
                }
                _ => false,
            }),
        }
    }


    /// Run the TUI application
    /// Returns Some(session_id) if hot-reload was requested
    pub async fn run(mut self, mut terminal: DefaultTerminal) -> Result<RunResult> {
        let mut event_stream = EventStream::new();
        let mut redraw_period = super::redraw_interval(&self);
        let mut redraw_interval = interval(redraw_period);
        // Subscribe to bus for background task completion notifications
        let mut bus_receiver = Bus::global().subscribe();

        loop {
            let desired_redraw = super::redraw_interval(&self);
            if desired_redraw != redraw_period {
                redraw_period = desired_redraw;
                redraw_interval = interval(redraw_period);
            }

            // Draw UI
            terminal.draw(|frame| crate::tui::ui::draw(frame, &self))?;

            if self.should_quit {
                break;
            }

            // Process pending turn OR wait for input/redraw
            if self.pending_turn {
                self.pending_turn = false;
                // Process turn while still handling input
                self.process_turn_with_input(&mut terminal, &mut event_stream)
                    .await;
            } else {
                // Wait for input or redraw tick
                tokio::select! {
                    _ = redraw_interval.tick() => {
                        local::handle_tick(&mut self);
                    }
                    event = event_stream.next() => {
                        local::handle_terminal_event(&mut self, &mut terminal, event)?;
                    }
                    // Handle background task completion notifications
                    bus_event = bus_receiver.recv() => {
                        local::handle_bus_event(&mut self, bus_event);
                    }
                }
            }
        }

        self.extract_session_memories().await;

        Ok(RunResult {
            reload_session: self.reload_requested.take(),
            rebuild_session: self.rebuild_requested.take(),
            update_session: self.update_requested.take(),
            exit_code: self.requested_exit_code,
            session_id: Some(self.session.id.clone()),
        })
    }

    /// Run the TUI in remote mode, connecting to a server
    pub async fn run_remote(mut self, mut terminal: DefaultTerminal) -> Result<RunResult> {
        let mut event_stream = EventStream::new();
        let mut redraw_period = super::redraw_interval(&self);
        let mut redraw_interval = interval(redraw_period);
        let mut remote_state = remote::RemoteRunState::default();

        'outer: loop {
            let session_to_resume = self.reconnect_target_session_id();

            let mut remote = match remote::connect_with_retry(
                &mut self,
                &mut terminal,
                &mut event_stream,
                &mut remote_state,
                session_to_resume.as_deref(),
            )
            .await?
            {
                remote::ConnectOutcome::Connected(remote) => remote,
                remote::ConnectOutcome::Retry => continue,
                remote::ConnectOutcome::Quit => break 'outer,
            };

            match remote::handle_post_connect(
                &mut self,
                &mut terminal,
                &mut remote,
                &mut remote_state,
                session_to_resume.as_deref(),
            )
            .await?
            {
                remote::PostConnectOutcome::Ready => {}
                remote::PostConnectOutcome::Quit => break 'outer,
            }

            let mut bus_receiver_remote = Bus::global().subscribe();

            // Main event loop
            loop {
                let desired_redraw = super::redraw_interval(&self);
                if desired_redraw != redraw_period {
                    redraw_period = desired_redraw;
                    redraw_interval = interval(redraw_period);
                }

                terminal.draw(|frame| crate::tui::ui::draw(frame, &self))?;

                if self.should_quit {
                    break 'outer;
                }

                tokio::select! {
                    _ = redraw_interval.tick() => {
                        remote::handle_tick(&mut self, &mut remote).await;
                    }
                    event = remote.next_event() => {
                        match remote::handle_remote_event(
                            &mut self,
                            &mut terminal,
                            &mut remote,
                            &mut remote_state,
                            event,
                        )
                        .await?
                        {
                            remote::RemoteEventOutcome::Continue => {}
                            remote::RemoteEventOutcome::Reconnect => continue 'outer,
                        }
                    }
                    event = event_stream.next() => {
                        remote::handle_terminal_event(&mut self, &mut terminal, &mut remote, event).await?;
                    }
                    bus_event = bus_receiver_remote.recv() => {
                        remote::handle_bus_event(&mut self, &mut remote, bus_event).await;
                    }
                }
            }
        }

        Ok(RunResult {
            reload_session: self.reload_requested.take(),
            rebuild_session: self.rebuild_requested.take(),
            update_session: self.update_requested.take(),
            exit_code: self.requested_exit_code,
            session_id: if self.is_remote {
                self.remote_session_id.clone()
            } else {
                Some(self.session.id.clone())
            },
        })
    }

    /// Run the TUI in replay mode, playing back a timeline of events.
    pub async fn run_replay(
        self,
        terminal: DefaultTerminal,
        timeline: Vec<crate::replay::TimelineEvent>,
        speed: f64,
    ) -> Result<RunResult> {
        replay::run_replay(self, terminal, timeline, speed).await
    }

    /// Run replay headlessly, rendering each frame to an in-memory buffer.
    /// Returns a list of (timestamp_secs, Buffer) pairs for video export.
    pub async fn run_headless_replay(
        mut self,
        timeline: &[crate::replay::TimelineEvent],
        speed: f64,
        width: u16,
        height: u16,
        fps: u32,
    ) -> Result<Vec<(f64, ratatui::buffer::Buffer)>> {
        use crate::replay::ReplayEvent;
        use ratatui::backend::TestBackend;

        let replay_events = crate::replay::timeline_to_replay_events(timeline);
        if replay_events.is_empty() {
            anyhow::bail!("No replay events to export");
        }

        let backend = TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend)?;
        let mut remote = super::backend::RemoteConnection::dummy();

        let frame_duration_ms: f64 = 1000.0 / fps as f64;
        let mut frames: Vec<(f64, ratatui::buffer::Buffer)> = Vec::new();
        let mut sim_time_ms: f64 = 0.0;
        let mut next_frame_at: f64 = 0.0;

        let total_duration_ms: f64 = replay_events.iter().map(|(d, _)| *d as f64 / speed).sum();

        let mut event_schedule: Vec<(f64, &ReplayEvent)> = Vec::new();
        {
            let mut abs_time: f64 = 0.0;
            for (delay_ms, evt) in &replay_events {
                abs_time += *delay_ms as f64 / speed;
                event_schedule.push((abs_time, evt));
            }
        }

        let mut event_cursor: usize = 0;
        let mut replay_turn_id: u64 = 0;

        terminal.draw(|f| crate::tui::render_frame(f, &self))?;
        frames.push((0.0, terminal.backend().buffer().clone()));

        let progress_interval = (total_duration_ms / 20.0).max(1000.0);
        let mut next_progress = progress_interval;

        while sim_time_ms <= total_duration_ms + frame_duration_ms {
            while event_cursor < event_schedule.len()
                && event_schedule[event_cursor].0 <= sim_time_ms
            {
                let (_t, event) = event_schedule[event_cursor];
                replay::apply_replay_event(
                    &mut self,
                    &mut remote,
                    event,
                    &mut replay_turn_id,
                    Some(sim_time_ms),
                );
                event_cursor += 1;
            }

            if sim_time_ms >= next_frame_at {
                replay::update_replay_elapsed_override(&mut self, sim_time_ms);
                terminal.draw(|f| crate::tui::render_frame(f, &self))?;
                frames.push((sim_time_ms / 1000.0, terminal.backend().buffer().clone()));
                next_frame_at = sim_time_ms + frame_duration_ms;
            }

            if sim_time_ms >= next_progress {
                let pct = (sim_time_ms / total_duration_ms * 100.0).min(100.0);
                eprint!("\r  Rendering... {:.0}%", pct);
                next_progress += progress_interval;
            }

            sim_time_ms += frame_duration_ms;
        }

        eprintln!("\r  Rendering... 100%  ({} frames captured)", frames.len());

        Ok(frames)
    }

    /// Handle a server event. Returns true if we're at a "safe point" for interleaving
    /// (after a tool completes but before the turn ends).
    fn handle_server_event(
        &mut self,
        event: crate::protocol::ServerEvent,
        remote: &mut super::backend::RemoteConnection,
    ) -> bool {
        remote::handle_server_event(self, event, remote)
    }

    fn handle_remote_char_input(&mut self, c: char) {
        remote::handle_remote_char_input(self, c);
    }

    /// Handle keyboard input in remote mode
    async fn handle_remote_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        remote: &mut super::backend::RemoteConnection,
    ) -> Result<()> {
        remote::handle_remote_key(self, code, modifiers, remote).await
    }

    /// Process turn while still accepting input for queueing
    async fn process_turn_with_input(
        &mut self,
        terminal: &mut DefaultTerminal,
        event_stream: &mut EventStream,
    ) {
        local::process_turn_with_input(self, terminal, event_stream).await;
    }

    /// Handle a key event (wrapper for debug injection)
    fn handle_key_event(&mut self, event: crossterm::event::KeyEvent) {
        // Record the event if recording is active
        use super::test_harness::{record_event, TestEvent};
        let modifiers: Vec<String> = {
            let mut mods = vec![];
            if event.modifiers.contains(KeyModifiers::CONTROL) {
                mods.push("ctrl".to_string());
            }
            if event.modifiers.contains(KeyModifiers::ALT) {
                mods.push("alt".to_string());
            }
            if event.modifiers.contains(KeyModifiers::SHIFT) {
                mods.push("shift".to_string());
            }
            mods
        };
        let code_str = format!("{:?}", event.code);
        record_event(TestEvent::Key {
            code: code_str,
            modifiers,
        });

        let _ = self.handle_key(event.code, event.modifiers);
    }

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Result<()> {
        let mut code = code;
        let mut modifiers = modifiers;
        ctrl_bracket_fallback_to_esc(&mut code, &mut modifiers);

        if input::handle_modal_key(self, code.clone(), modifiers)? {
            return Ok(());
        }

        if input::handle_pre_control_shortcuts(self, code.clone(), modifiers) {
            return Ok(());
        }

        self.normalize_diagram_state();
        let diagram_available = self.diagram_available();

        // Handle ctrl combos regardless of processing state
        if modifiers.contains(KeyModifiers::CONTROL)
            && input::handle_global_control_shortcuts(self, code, diagram_available)
        {
            return Ok(());
        }

        // Shift+Enter: does opposite of queue_mode during processing
        if code == KeyCode::Enter && modifiers.contains(KeyModifiers::SHIFT) {
            input::handle_shift_enter(self);
            return Ok(());
        }

        // When the model picker preview is visible, arrow keys navigate the picker list
        if self
            .picker_state
            .as_ref()
            .map(|p| p.preview)
            .unwrap_or(false)
        {
            match code {
                KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                    return self.handle_picker_key(code, modifiers);
                }
                _ => {}
            }
        }

        if code == KeyCode::Enter {
            input::handle_enter(self);
            return Ok(());
        }

        if input::handle_basic_key(self, code) {
            return Ok(());
        }

        Ok(())
    }

    fn redraw_now(&self, terminal: &mut DefaultTerminal) -> Result<()> {
        terminal.draw(|frame| crate::tui::ui::draw(frame, self))?;
        Ok(())
    }

    /// Try to paste an image from the clipboard. Checks native image data first,
    /// then falls back to HTML clipboard for <img> URLs, then arboard text.
    /// Used by both Ctrl+V and Alt+V handlers in both local and remote mode.
    fn paste_image_from_clipboard(&mut self) {
        input::paste_image_from_clipboard(self);
    }

    /// Queue a message to be sent later
    /// Handle bracketed paste: store text content (image URLs are still detected inline)
    fn handle_paste(&mut self, text: String) {
        input::handle_paste(self, text);
    }

    /// Expand paste placeholders in input with actual content
    fn expand_paste_placeholders(&mut self, input: &str) -> String {
        input::expand_paste_placeholders(self, input)
    }

    fn queue_message(&mut self) {
        input::queue_message(self);
    }

    /// Send an interleave message immediately to the server as a soft interrupt.
    /// Skips the intermediate buffer stage - goes directly to pending_soft_interrupts.
    async fn send_interleave_now(
        &mut self,
        content: String,
        remote: &mut super::backend::RemoteConnection,
    ) {
        remote::send_interleave_now(self, content, remote).await;
    }

    /// Retrieve all pending unsent messages into the input for editing.
    /// Priority: pending soft interrupts first, then interleave, then queued.
    /// Returns true if pending soft interrupts were retrieved (caller should cancel on server).
    fn retrieve_pending_message_for_edit(&mut self) -> bool {
        input::retrieve_pending_message_for_edit(self)
    }

    fn send_action(&self, shift: bool) -> SendAction {
        input::send_action(self, shift)
    }

    fn insert_thought_line(&mut self, line: String) {
        if self.thought_line_inserted || line.is_empty() {
            return;
        }
        self.thought_line_inserted = true;
        let mut prefix = line;
        if !prefix.ends_with('\n') {
            prefix.push('\n');
        }
        prefix.push('\n');
        if self.streaming_text.is_empty() {
            self.streaming_text = prefix;
        } else {
            self.streaming_text = format!("{}{}", prefix, self.streaming_text);
        }
    }

    fn clear_streaming_render_state(&mut self) {
        self.streaming_text.clear();
        self.streaming_md_renderer.borrow_mut().reset();
        crate::tui::mermaid::clear_streaming_preview_diagram();
    }

    fn take_streaming_text(&mut self) -> String {
        let content = std::mem::take(&mut self.streaming_text);
        self.streaming_md_renderer.borrow_mut().reset();
        crate::tui::mermaid::clear_streaming_preview_diagram();
        content
    }

    fn accumulate_streaming_output_tokens(
        &mut self,
        output_tokens: u64,
        call_output_tokens_seen: &mut u64,
    ) {
        let delta = if output_tokens >= *call_output_tokens_seen {
            output_tokens - *call_output_tokens_seen
        } else {
            // Usage snapshots should be monotonic within one API call. If they are not,
            // treat this as a reset and count the full value once.
            output_tokens
        };
        self.streaming_total_output_tokens += delta;
        *call_output_tokens_seen = output_tokens;
    }

    fn command_help(&self, topic: &str) -> Option<String> {
        let topic = topic.trim().trim_start_matches('/').to_lowercase();
        let help = match topic.as_str() {
            "help" | "commands" => {
                "`/help`\nShow general command list and keyboard shortcuts.\n\n`/help <command>`\nShow detailed help for one command."
            }
            "compact" => {
                "`/compact`\nForce context compaction now.\nStarts background summarization and applies it automatically when ready."
            }
            "fix" => {
                "`/fix`\nRun recovery actions when the model cannot continue.\nRepairs missing tool outputs, resets provider session state, and starts compaction when possible."
            }
            "rewind" => {
                "`/rewind`\nShow numbered conversation history.\n\n`/rewind N`\nRewind to message N (drops everything after it and resets provider session)."
            }
            "clear" => {
                "`/clear`\nClear current conversation, queue, and display; starts a fresh session."
            }
            "model" => {
                "`/model`\nOpen model picker.\n\n`/model <name>`\nSwitch model.\n\n`/model <name>@<provider>`\nPin OpenRouter routing (`@auto` clears pin)."
            }
            "effort" => {
                "`/effort`\nShow current reasoning effort.\n\n`/effort <level>`\nSet reasoning effort (none|low|medium|high|xhigh).\n\nAlso: Alt+←/→ to cycle."
            }
            "memory" => "`/memory [on|off|status]`\nToggle memory features for this session.",
            "remember" => {
                "`/remember`\nExtract memories from current conversation and store them."
            }
            "swarm" => "`/swarm [on|off|status]`\nToggle swarm features for this session.",
            "poke" => {
                "`/poke`\nPoke the model to resume when it has stopped with incomplete todos.\n\
                Injects a reminder listing all pending/in-progress tasks and prompts the model to either\n\
                finish the work, update the todo list to reflect what is done, or ask for user input if genuinely blocked."
            }
            "reload" => "`/reload`\nReload to a newer binary if one is available.",
            "rebuild" => "`/rebuild`\nRun full update flow (git pull + cargo build + tests).",
            "split" => "`/split`\nSplit the current session into a new window. Clones the full conversation history so both sessions continue from the same point.",
            "resume" | "sessions" => "`/resume`\nOpen the interactive session picker. Browse and search all sessions, preview conversation history, and open any session in a new terminal window.\n\nPress `Esc` to return to your current session.",
            "info" => "`/info`\nShow session metadata and token usage.",
            "usage" => "`/usage`\nFetch and display subscription usage limits for all connected OAuth providers (Anthropic, OpenAI/ChatGPT).\nShows 5-hour and 7-day windows, reset times, and extra usage status.",
            "version" => "`/version`\nShow jcode version/build details.",
            "changelog" => "`/changelog`\nShow recent changes embedded in this build.",
            "quit" => "`/quit`\nExit jcode.",
            "config" => {
                "`/config`\nShow active configuration.\n\n`/config init`\nCreate default config file.\n\n`/config edit`\nOpen config in `$EDITOR`."
            }
            "auth" | "login" => {
                "`/auth`\nShow authentication status for all providers.\n\n`/login`\nInteractive provider selection - pick a provider to log into.\n\n`/login <provider>`\nStart login flow directly for any provider shown by `/login` or the `/login ` completions."
            }
            "account" | "accounts" => {
                "`/account`\nList all Anthropic OAuth accounts.\n\n`/account add <label>`\nAdd a new account via OAuth login.\n\n`/account switch <label>`\nSwitch the active account.\n\n`/account remove <label>`\nRemove an account."
            }
            "save" => {
                "`/save`\nBookmark the current session so it appears at the top of `/resume`.\n\n`/save <label>`\nBookmark with a custom label for easy identification.\n\nSaved sessions are shown in a dedicated \"Saved\" section in the session picker."
            }
            "unsave" => {
                "`/unsave`\nRemove the bookmark from the current session."
            }
            "client-reload" if self.is_remote => {
                "`/client-reload`\nForce client binary reload in remote mode."
            }
            "server-reload" if self.is_remote => {
                "`/server-reload`\nForce server binary reload in remote mode."
            }
            _ => return None,
        };
        Some(help.to_string())
    }

    /// Submit input - just sets up message and flags, processing happens in next loop iteration
    fn submit_input(&mut self) {
        if self.activate_model_picker_from_preview() {
            return;
        }

        let raw_input = std::mem::take(&mut self.input);
        let input = self.expand_paste_placeholders(&raw_input);
        self.pasted_contents.clear();
        self.cursor_pos = 0;
        self.follow_chat_bottom(); // Reset to bottom and resume auto-scroll on new input

        if let Some(pending) = self.pending_login.take() {
            self.handle_login_input(pending, input);
            return;
        }

        let trimmed = input.trim();
        if commands::handle_help_command(self, trimmed)
            || commands::handle_session_command(self, trimmed)
            || commands::handle_config_command(self, trimmed)
            || commands::handle_debug_command(self, trimmed)
            || commands::handle_model_command(self, trimmed)
            || commands::handle_info_command(self, trimmed)
            || commands::handle_auth_command(self, trimmed)
            || commands::handle_dev_command(self, trimmed)
        {
            return;
        }

        // Check for skill invocation
        if let Some(skill_name) = SkillRegistry::parse_invocation(&input) {
            if let Some(skill) = self.skills.get(skill_name) {
                self.active_skill = Some(skill_name.to_string());
                self.push_display_message(DisplayMessage {
                    role: "system".to_string(),
                    content: format!("Activated skill: {} - {}", skill.name, skill.description),
                    tool_calls: vec![],
                    duration_secs: None,
                    title: None,
                    tool_data: None,
                });
            } else {
                self.push_display_message(DisplayMessage {
                    role: "error".to_string(),
                    content: format!("Unknown skill: /{}", skill_name),
                    tool_calls: vec![],
                    duration_secs: None,
                    title: None,
                    tool_data: None,
                });
            }
            return;
        }

        // Add user message to display (show placeholder to user, not full paste)
        self.push_display_message(DisplayMessage {
            role: "user".to_string(),
            content: raw_input, // Show placeholder to user (condensed view)
            tool_calls: vec![],
            duration_secs: None,
            title: None,
            tool_data: None,
        });
        // Send expanded content (with actual pasted text) to model
        let images = std::mem::take(&mut self.pending_images);
        if !images.is_empty() {
            crate::logging::info(&format!(
                "Submitting with {} image(s): {}",
                images.len(),
                images
                    .iter()
                    .map(|(t, d)| format!("{} ({}KB)", t, d.len() / 1024))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if images.is_empty() {
            self.add_provider_message(Message::user(&input));
            self.session.add_message(
                Role::User,
                vec![ContentBlock::Text {
                    text: input.clone(),
                    cache_control: None,
                }],
            );
        } else {
            self.add_provider_message(Message::user_with_images(&input, images.clone()));
            let mut blocks: Vec<ContentBlock> = images
                .into_iter()
                .map(|(media_type, data)| ContentBlock::Image { media_type, data })
                .collect();
            blocks.push(ContentBlock::Text {
                text: input.clone(),
                cache_control: None,
            });
            self.session.add_message(Role::User, blocks);
        }
        let _ = self.session.save();

        // Set up processing state - actual processing happens after UI redraws
        self.is_processing = true;
        self.status = ProcessingStatus::Sending;
        self.clear_streaming_render_state();
        self.stream_buffer.clear();
        self.thought_line_inserted = false;
        self.thinking_prefix_emitted = false;
        self.thinking_buffer.clear();
        self.streaming_tool_calls.clear();
        self.streaming_input_tokens = 0;
        self.streaming_output_tokens = 0;
        self.streaming_cache_read_tokens = None;
        self.streaming_cache_creation_tokens = None;
        self.upstream_provider = None;
        self.streaming_tps_start = None;
        self.streaming_tps_elapsed = Duration::ZERO;
        self.streaming_total_output_tokens = 0;
        self.processing_started = Some(Instant::now());
        self.pending_turn = true;
    }

    /// Process all queued messages (combined into a single request)
    /// Loops until queue is empty (in case more messages are queued during processing)
    async fn process_queued_messages(
        &mut self,
        terminal: &mut DefaultTerminal,
        event_stream: &mut EventStream,
    ) {
        while !self.queued_messages.is_empty() {
            // Combine all currently queued messages into one
            let messages = std::mem::take(&mut self.queued_messages);
            let combined = messages.join("\n\n");

            // Display each queued message as its own user prompt
            for msg in &messages {
                self.push_display_message(DisplayMessage::user(msg.clone()));
            }

            self.add_provider_message(Message::user(&combined));
            self.session.add_message(
                Role::User,
                vec![ContentBlock::Text {
                    text: combined,
                    cache_control: None,
                }],
            );
            let _ = self.session.save();
            self.clear_streaming_render_state();
            self.stream_buffer.clear();
            self.thought_line_inserted = false;
            self.thinking_prefix_emitted = false;
            self.thinking_buffer.clear();
            self.streaming_tool_calls.clear();
            self.streaming_input_tokens = 0;
            self.streaming_output_tokens = 0;
            self.streaming_cache_read_tokens = None;
            self.streaming_cache_creation_tokens = None;
            self.upstream_provider = None;
            self.streaming_tps_start = None;
            self.streaming_tps_elapsed = Duration::ZERO;
            self.streaming_total_output_tokens = 0;
            self.processing_started = Some(Instant::now());
            self.is_processing = true;
            self.status = ProcessingStatus::Sending;

            match self.run_turn_interactive(terminal, event_stream).await {
                Ok(()) => {
                    self.last_stream_error = None;
                }
                Err(e) => {
                    let err_str = e.to_string();
                    if is_context_limit_error(&err_str) {
                        if self
                            .try_auto_compact_and_retry(terminal, event_stream)
                            .await
                        {
                            // Successfully recovered
                        } else {
                            self.handle_turn_error(err_str);
                        }
                    } else {
                        self.handle_turn_error(err_str);
                    }
                }
            }
            // Loop will check if more messages were queued during this turn
        }
    }

    fn cycle_model(&mut self, direction: i8) {
        let models = self.provider.available_models();
        if models.is_empty() {
            self.push_display_message(DisplayMessage::error(
                "Model switching is not available for this provider.",
            ));
            self.set_status_notice("Model switching not available");
            return;
        }

        let current = self.provider.model();
        let current_index = models.iter().position(|m| *m == current).unwrap_or(0);

        let len = models.len();
        let next_index = if direction >= 0 {
            (current_index + 1) % len
        } else {
            (current_index + len - 1) % len
        };
        let next_model = models[next_index];

        match self.provider.set_model(next_model) {
            Ok(()) => {
                self.provider_session_id = None;
                self.session.provider_session_id = None;
                self.upstream_provider = None;
                self.connection_type = None;
                self.update_context_limit_for_model(next_model);
                self.session.model = Some(self.provider.model());
                let _ = self.session.save();
                self.push_display_message(DisplayMessage::system(format!(
                    "✓ Switched to model: {}",
                    next_model
                )));
                self.set_status_notice(format!("Model → {}", next_model));
            }
            Err(e) => {
                self.push_display_message(DisplayMessage::error(format!(
                    "Failed to switch model: {}",
                    e
                )));
                self.set_status_notice("Model switch failed");
            }
        }
    }

    fn cycle_effort(&mut self, direction: i8) {
        let efforts = self.provider.available_efforts();
        if efforts.is_empty() {
            self.set_status_notice("Reasoning effort not available for this provider");
            return;
        }

        let current = self.provider.reasoning_effort();
        let current_index = current
            .as_ref()
            .and_then(|c| efforts.iter().position(|e| *e == c.as_str()))
            .unwrap_or(efforts.len() - 1); // default to last (xhigh)

        let len = efforts.len();
        let next_index = if direction > 0 {
            if current_index + 1 >= len {
                current_index // already at max
            } else {
                current_index + 1
            }
        } else {
            if current_index == 0 {
                0 // already at min
            } else {
                current_index - 1
            }
        };

        let next_effort = efforts[next_index];
        if Some(next_effort.to_string()) == current {
            let label = effort_display_label(next_effort);
            self.set_status_notice(format!(
                "Effort: {} (already at {})",
                label,
                if direction > 0 { "max" } else { "min" }
            ));
            return;
        }

        match self.provider.set_reasoning_effort(next_effort) {
            Ok(()) => {
                let label = effort_display_label(next_effort);
                let bar = effort_bar(next_index, len);
                self.set_status_notice(format!("Effort: {} {}", label, bar));
            }
            Err(e) => {
                self.set_status_notice(format!("Effort switch failed: {}", e));
            }
        }
    }

    fn update_context_limit_for_model(&mut self, model: &str) {
        let limit = if self.is_remote {
            crate::provider::context_limit_for_model(model)
                .unwrap_or(self.provider.context_window())
        } else {
            self.provider.context_window()
        };
        self.context_limit = limit as u64;
        self.context_warning_shown = false;

        // Also update compaction manager's budget
        {
            let compaction = self.registry.compaction();
            if let Ok(mut manager) = compaction.try_write() {
                manager.set_budget(limit);
            };
        }
    }

    fn effective_context_tokens_from_usage(
        &self,
        input_tokens: u64,
        cache_read_input_tokens: Option<u64>,
        cache_creation_input_tokens: Option<u64>,
    ) -> u64 {
        if input_tokens == 0 {
            return 0;
        }
        let cache_read = cache_read_input_tokens.unwrap_or(0);
        let cache_creation = cache_creation_input_tokens.unwrap_or(0);
        let provider_name = if self.is_remote {
            self.remote_provider_name.clone().unwrap_or_default()
        } else {
            self.provider.name().to_string()
        }
        .to_lowercase();

        // Some providers report cache tokens as separate counters, others report them as subsets.
        // When in doubt, avoid over-counting unless we have strong evidence of split accounting.
        let split_cache_accounting = provider_name.contains("anthropic")
            || provider_name.contains("claude")
            || cache_creation > 0
            || cache_read > input_tokens;

        if split_cache_accounting {
            input_tokens
                .saturating_add(cache_read)
                .saturating_add(cache_creation)
        } else {
            input_tokens
        }
    }

    fn current_stream_context_tokens(&self) -> Option<u64> {
        if self.streaming_input_tokens == 0 {
            return None;
        }
        Some(self.effective_context_tokens_from_usage(
            self.streaming_input_tokens,
            self.streaming_cache_read_tokens,
            self.streaming_cache_creation_tokens,
        ))
    }

    fn update_compaction_usage_from_stream(&mut self) {
        if self.is_remote || !self.provider.supports_compaction() {
            return;
        }
        let Some(tokens) = self.current_stream_context_tokens() else {
            return;
        };
        let compaction = self.registry.compaction();
        if let Ok(mut manager) = compaction.try_write() {
            manager.update_observed_input_tokens(tokens);
        };
    }

    fn handle_turn_error(&mut self, error: impl Into<String>) {
        let error = error.into();
        self.last_stream_error = Some(error.clone());

        if is_context_limit_error(&error) {
            let recovery = self.auto_recover_context_limit();
            let hint = match recovery {
                Some(msg) => format!(" {}", msg),
                None => " Context limit exceeded but auto-recovery failed. Run `/fix` to try manual recovery.".to_string(),
            };
            self.push_display_message(DisplayMessage::error(format!("Error: {}{}", error, hint)));
        } else {
            self.push_display_message(DisplayMessage::error(format!(
                "Error: {} Run `/fix` to attempt recovery.",
                error
            )));
        }
    }

    fn auto_recover_context_limit(&mut self) -> Option<String> {
        if self.is_remote || !self.provider.supports_compaction() {
            return None;
        }
        let compaction = self.registry.compaction();
        let mut manager = compaction.try_write().ok()?;

        let usage = manager.context_usage_with(&self.messages);
        if usage > 1.5 {
            match manager.hard_compact_with(&self.messages) {
                Ok(dropped) => {
                    let post_usage = manager.context_usage_with(&self.messages);
                    if post_usage <= 1.0 {
                        return Some(format!(
                            "⚡ Emergency compaction: dropped {} old messages (context was at {:.0}%). You can continue.",
                            dropped,
                            usage * 100.0
                        ));
                    }
                    let truncated = manager.emergency_truncate_with(&mut self.messages);
                    return Some(format!(
                        "⚡ Emergency compaction: dropped {} old messages and truncated {} tool result(s) (context was at {:.0}%). You can continue.",
                        dropped, truncated,
                        usage * 100.0
                    ));
                }
                Err(reason) => {
                    crate::logging::error(&format!(
                        "[auto_recover] hard_compact failed: {}",
                        reason
                    ));
                    let truncated = manager.emergency_truncate_with(&mut self.messages);
                    if truncated > 0 {
                        return Some(format!(
                            "⚡ Emergency truncation: shortened {} large tool result(s) to fit context. You can continue.",
                            truncated
                        ));
                    }
                }
            }
        }

        let observed_tokens = self
            .current_stream_context_tokens()
            .unwrap_or(self.context_limit as u64);
        manager.update_observed_input_tokens(observed_tokens);

        match manager.force_compact_with(&self.messages, self.provider.clone()) {
            Ok(()) => Some(
                "⚡ Auto-compaction started — summarizing old messages in background. Retry in a moment."
                    .to_string(),
            ),
            Err(reason) => {
                crate::logging::error(&format!(
                    "[auto_recover] force_compact failed: {}",
                    reason
                ));
                match manager.hard_compact_with(&self.messages) {
                    Ok(dropped) => Some(format!(
                        "⚡ Emergency compaction: dropped {} old messages. You can continue.",
                        dropped
                    )),
                    Err(_) => {
                        let truncated = manager.emergency_truncate_with(&mut self.messages);
                        if truncated > 0 {
                            Some(format!(
                                "⚡ Emergency truncation: shortened {} large tool result(s) to fit context. You can continue.",
                                truncated
                            ))
                        } else {
                            None
                        }
                    }
                }
            }
        }
    }

    /// Attempt automatic compaction and retry when context limit is exceeded.
    /// Returns true if the retry succeeded.
    async fn try_auto_compact_and_retry(
        &mut self,
        terminal: &mut DefaultTerminal,
        event_stream: &mut EventStream,
    ) -> bool {
        if self.is_remote || !self.provider.supports_compaction() {
            return false;
        }

        self.push_display_message(DisplayMessage::system(
            "⚠️ Context limit exceeded — auto-compacting and retrying...".to_string(),
        ));

        // Force the compaction manager to think we're at the limit
        let compaction = self.registry.compaction();
        let compact_started = match compaction.try_write() {
            Ok(mut manager) => {
                manager.update_observed_input_tokens(self.context_limit);
                let usage = manager.context_usage_with(&self.messages);
                if usage > 1.5 {
                    match manager.hard_compact_with(&self.messages) {
                        Ok(dropped) => {
                            self.push_display_message(DisplayMessage::system(
                                format!(
                                    "⚡ Emergency compaction: dropped {} old messages (context was at {:.0}%).",
                                    dropped,
                                    usage * 100.0
                                ),
                            ));
                            drop(manager);
                            self.provider_session_id = None;
                            self.session.provider_session_id = None;
                            self.context_warning_shown = false;
                            self.clear_streaming_render_state();
                            self.stream_buffer.clear();
                            self.streaming_tool_calls.clear();
                            self.streaming_input_tokens = 0;
                            self.streaming_output_tokens = 0;
                            self.streaming_cache_read_tokens = None;
                            self.streaming_cache_creation_tokens = None;
                            self.thought_line_inserted = false;
                            self.thinking_prefix_emitted = false;
                            self.thinking_buffer.clear();
                            self.status = ProcessingStatus::Sending;

                            self.push_display_message(DisplayMessage::system(
                                "✓ Context compacted. Retrying...".to_string(),
                            ));
                            return match self.run_turn_interactive(terminal, event_stream).await {
                                Ok(()) => {
                                    self.last_stream_error = None;
                                    true
                                }
                                Err(e) => {
                                    self.handle_turn_error(e.to_string());
                                    false
                                }
                            };
                        }
                        Err(_) => {
                            let truncated = manager.emergency_truncate_with(&mut self.messages);
                            if truncated > 0 {
                                drop(manager);
                                self.provider_session_id = None;
                                self.session.provider_session_id = None;
                                self.context_warning_shown = false;
                                self.clear_streaming_render_state();
                                self.stream_buffer.clear();
                                self.streaming_tool_calls.clear();
                                self.streaming_input_tokens = 0;
                                self.streaming_output_tokens = 0;
                                self.streaming_cache_read_tokens = None;
                                self.streaming_cache_creation_tokens = None;
                                self.thought_line_inserted = false;
                                self.thinking_prefix_emitted = false;
                                self.thinking_buffer.clear();
                                self.status = ProcessingStatus::Sending;

                                self.push_display_message(DisplayMessage::system(
                                    format!("⚡ Emergency truncation: shortened {} large tool result(s). Retrying...", truncated),
                                ));
                                return match self.run_turn_interactive(terminal, event_stream).await
                                {
                                    Ok(()) => {
                                        self.last_stream_error = None;
                                        true
                                    }
                                    Err(e) => {
                                        self.handle_turn_error(e.to_string());
                                        false
                                    }
                                };
                            }
                            false
                        }
                    }
                } else {
                    match manager.force_compact_with(&self.messages, self.provider.clone()) {
                        Ok(()) => true,
                        Err(_) => match manager.hard_compact_with(&self.messages) {
                            Ok(_) => {
                                drop(manager);
                                self.provider_session_id = None;
                                self.session.provider_session_id = None;
                                self.context_warning_shown = false;
                                self.clear_streaming_render_state();
                                self.stream_buffer.clear();
                                self.streaming_tool_calls.clear();
                                self.streaming_input_tokens = 0;
                                self.streaming_output_tokens = 0;
                                self.streaming_cache_read_tokens = None;
                                self.streaming_cache_creation_tokens = None;
                                self.thought_line_inserted = false;
                                self.thinking_prefix_emitted = false;
                                self.thinking_buffer.clear();
                                self.status = ProcessingStatus::Sending;

                                self.push_display_message(DisplayMessage::system(
                                    "✓ Context compacted (emergency). Retrying...".to_string(),
                                ));
                                return match self.run_turn_interactive(terminal, event_stream).await
                                {
                                    Ok(()) => {
                                        self.last_stream_error = None;
                                        true
                                    }
                                    Err(e) => {
                                        self.handle_turn_error(e.to_string());
                                        false
                                    }
                                };
                            }
                            Err(_) => false,
                        },
                    }
                }
            }
            Err(_) => false,
        };

        if !compact_started {
            return false;
        }

        // Wait for compaction to finish (up to 60s), reacting to Bus event
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        self.status = ProcessingStatus::RunningTool("compacting context...".to_string());
        let mut bus_rx = Bus::global().subscribe();

        loop {
            if std::time::Instant::now() >= deadline {
                self.push_display_message(DisplayMessage::error(
                    "Auto-compaction timed out.".to_string(),
                ));
                return false;
            }

            // Redraw UI while we wait
            let _ = terminal.draw(|frame| crate::tui::ui::draw(frame, self));

            let compaction = self.registry.compaction();
            let done = if let Ok(mut manager) = compaction.try_write() {
                if let Some(event) = manager.poll_compaction_event() {
                    self.handle_compaction_event(event);
                    true
                } else {
                    false
                }
            } else {
                false
            };

            if done {
                break;
            }

            // Wait for Bus notification or timeout (instead of sleep-polling)
            let timeout = tokio::time::sleep(Duration::from_secs(1));
            tokio::select! {
                _ = bus_rx.recv() => {}
                _ = timeout => {}
            }
        }

        self.push_display_message(DisplayMessage::system(
            "✓ Context compacted. Retrying...".to_string(),
        ));

        // Reset provider session since context changed
        self.provider_session_id = None;
        self.session.provider_session_id = None;
        self.context_warning_shown = false;

        // Clear streaming state for the retry
        self.clear_streaming_render_state();
        self.stream_buffer.clear();
        self.streaming_tool_calls.clear();
        self.streaming_input_tokens = 0;
        self.streaming_output_tokens = 0;
        self.streaming_cache_read_tokens = None;
        self.streaming_cache_creation_tokens = None;
        self.thought_line_inserted = false;
        self.thinking_prefix_emitted = false;
        self.thinking_buffer.clear();
        self.status = ProcessingStatus::Sending;

        // Retry the turn
        match self.run_turn_interactive(terminal, event_stream).await {
            Ok(()) => {
                self.last_stream_error = None;
                true
            }
            Err(e) => {
                self.handle_turn_error(e.to_string());
                false
            }
        }
    }

    fn handle_usage_report(&mut self, results: Vec<crate::usage::ProviderUsage>) {
        if results.is_empty() {
            self.push_display_message(DisplayMessage::system(
                "No providers with OAuth credentials found.\n\
                 Use `/login anthropic` or `/login openai` to authenticate."
                    .to_string(),
            ));
            return;
        }

        let mut output = String::from("## Subscription Usage\n\n");

        for (i, provider) in results.iter().enumerate() {
            if i > 0 {
                output.push_str("---\n\n");
            }
            output.push_str(&format!("### {}\n\n", provider.provider_name));

            if let Some(ref err) = provider.error {
                output.push_str(&format!("⚠ {}\n\n", err));
                continue;
            }

            if provider.limits.is_empty() && provider.extra_info.is_empty() {
                output.push_str("No usage data available\n\n");
                continue;
            }

            for limit in &provider.limits {
                let bar = crate::usage::format_usage_bar(limit.usage_percent, 15);
                let reset_info = if let Some(ref ts) = limit.resets_at {
                    let relative = crate::usage::format_reset_time(ts);
                    format!(" (resets in {})", relative)
                } else {
                    String::new()
                };
                output.push_str(&format!("- **{}**: {}{}\n", limit.name, bar, reset_info));
            }

            if !provider.limits.is_empty() {
                output.push('\n');
            }

            for (key, value) in &provider.extra_info {
                output.push_str(&format!("- {}: {}\n", key, value));
            }
            output.push('\n');
        }

        if self.total_input_tokens > 0 || self.total_output_tokens > 0 {
            output.push_str("---\n\n### Session Usage\n\n");
            output.push_str(&format!(
                "- **Input tokens:** {}\n- **Output tokens:** {}\n",
                self.total_input_tokens, self.total_output_tokens
            ));
            if self.total_cost > 0.0 {
                output.push_str(&format!("- **Cost:** ${:.4}\n", self.total_cost));
            }
            output.push('\n');
        }

        self.push_display_message(DisplayMessage::system(output));
    }

    fn run_fix_command(&mut self) {
        let mut actions: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let last_error = self.last_stream_error.clone();
        let context_error = last_error
            .as_deref()
            .map(is_context_limit_error)
            .unwrap_or(false);

        let repaired = self.repair_missing_tool_outputs();
        if repaired > 0 {
            actions.push(format!("Recovered {} missing tool output(s).", repaired));
        }

        if self.summarize_tool_results_missing().is_some() {
            self.recover_session_without_tools();
            actions.push("Created a recovery session with text-only history.".to_string());
        }

        if self.provider_session_id.is_some() || self.session.provider_session_id.is_some() {
            self.provider_session_id = None;
            self.session.provider_session_id = None;
            actions.push("Reset provider session resume state.".to_string());
        }

        if !self.is_remote && self.provider.supports_compaction() {
            let observed_tokens = self
                .current_stream_context_tokens()
                .or_else(|| context_error.then_some(self.context_limit));
            let compaction = self.registry.compaction();
            match compaction.try_write() {
                Ok(mut manager) => {
                    if let Some(tokens) = observed_tokens {
                        manager.update_observed_input_tokens(tokens);
                    }
                    let usage = manager.context_usage_with(&self.messages);
                    if usage > 1.5 {
                        match manager.hard_compact_with(&self.messages) {
                            Ok(dropped) => {
                                actions.push(format!(
                                    "Emergency compaction: dropped {} old messages (context was at {:.0}%).",
                                    dropped,
                                    usage * 100.0
                                ));
                            }
                            Err(reason) => {
                                notes.push(format!("Hard compaction failed: {}", reason));
                            }
                        }
                        let post_usage = manager.context_usage_with(&self.messages);
                        if post_usage > 1.0 {
                            let truncated = manager.emergency_truncate_with(&mut self.messages);
                            if truncated > 0 {
                                actions.push(format!(
                                    "Emergency truncation: shortened {} large tool result(s) to fit context.",
                                    truncated
                                ));
                            }
                        }
                    } else {
                        match manager.force_compact_with(&self.messages, self.provider.clone()) {
                            Ok(()) => {
                                actions.push("Started background context compaction.".to_string())
                            }
                            Err(reason) => match manager.hard_compact_with(&self.messages) {
                                Ok(dropped) => {
                                    actions.push(format!(
                                            "Emergency compaction: dropped {} old messages (normal compaction failed: {}).",
                                            dropped, reason
                                        ));
                                }
                                Err(hard_reason) => {
                                    notes.push(format!(
                                        "Compaction not started: {}. Emergency fallback: {}",
                                        reason, hard_reason
                                    ));
                                }
                            },
                        }
                    }
                }
                Err(_) => notes.push("Could not access compaction manager (busy).".to_string()),
            };
        } else {
            notes.push("Compaction is unavailable for this provider.".to_string());
        }

        self.context_warning_shown = false;
        self.last_stream_error = None;
        self.set_status_notice("Fix applied");

        let mut content = String::from("**Fix Results:**\n");
        if actions.is_empty() {
            content.push_str("• No structural issues detected.\n");
        } else {
            for action in &actions {
                content.push_str(&format!("• {}\n", action));
            }
        }
        for note in &notes {
            content.push_str(&format!("• {}\n", note));
        }
        if let Some(last_error) = &last_error {
            content.push_str(&format!(
                "\nLast error: `{}`",
                crate::util::truncate_str(last_error, 200)
            ));
        }
        self.push_display_message(DisplayMessage::system(content));
    }

    fn add_provider_message(&mut self, message: Message) {
        self.messages.push(message);
        if self.is_remote || !self.provider.supports_compaction() {
            return;
        }
        let compaction = self.registry.compaction();
        if let Ok(mut manager) = compaction.try_write() {
            manager.notify_message_added();
        };
    }

    fn replace_provider_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages;
        self.last_injected_memory_signature = None;
        self.rebuild_tool_result_index();
        self.reseed_compaction_from_provider_messages();
    }

    fn clear_provider_messages(&mut self) {
        self.messages.clear();
        self.last_injected_memory_signature = None;
        self.tool_result_ids.clear();
        self.reseed_compaction_from_provider_messages();
    }

    fn rebuild_tool_result_index(&mut self) {
        self.tool_result_ids.clear();
        for msg in &self.messages {
            if let Role::User = msg.role {
                for block in &msg.content {
                    if let ContentBlock::ToolResult { tool_use_id, .. } = block {
                        self.tool_result_ids.insert(tool_use_id.clone());
                    }
                }
            }
        }
    }

    fn reseed_compaction_from_provider_messages(&mut self) {
        if self.is_remote || !self.provider.supports_compaction() {
            return;
        }
        let compaction = self.registry.compaction();
        if let Ok(mut manager) = compaction.try_write() {
            manager.reset();
            manager.set_budget(self.context_limit as usize);
            for _ in &self.messages {
                manager.notify_message_added();
            }
        };
    }

    fn messages_for_provider(&mut self) -> (Vec<Message>, Option<CompactionEvent>) {
        if self.is_remote || !self.provider.supports_compaction() {
            return (self.messages.clone(), None);
        }
        let compaction = self.registry.compaction();
        let result = match compaction.try_write() {
            Ok(mut manager) => {
                let action = manager.ensure_context_fits(&self.messages, self.provider.clone());
                match action {
                    crate::compaction::CompactionAction::BackgroundStarted => {
                        self.push_display_message(DisplayMessage::system(
                            "📦 **Compaction started** — context above 80%, summarizing older messages in background..."
                                .to_string(),
                        ));
                        self.set_status_notice("Compacting context...");
                    }
                    crate::compaction::CompactionAction::HardCompacted(dropped) => {
                        self.push_display_message(DisplayMessage::system(format!(
                            "📦 **Emergency compaction** — context critically full, dropped {} old messages to fit.",
                            dropped,
                        )));
                        self.set_status_notice(format!(
                            "Emergency compaction: {} msgs dropped",
                            dropped
                        ));
                    }
                    crate::compaction::CompactionAction::None => {}
                }
                let messages = manager.messages_for_api_with(&self.messages);
                let event = manager.take_compaction_event();
                (messages, event)
            }
            Err(_) => (self.messages.clone(), None),
        };
        result
    }

    fn poll_compaction_completion(&mut self) {
        if self.is_remote || !self.provider.supports_compaction() {
            return;
        }
        let compaction = self.registry.compaction();
        if let Ok(mut manager) = compaction.try_write() {
            if let Some(event) = manager.poll_compaction_event() {
                self.handle_compaction_event(event);
            }
        };
    }

    fn handle_compaction_event(&mut self, event: CompactionEvent) {
        self.provider_session_id = None;
        self.session.provider_session_id = None;
        self.context_warning_shown = false;
        let tokens_str = event
            .pre_tokens
            .map(|t| format!(" (was {} tokens)", t))
            .unwrap_or_default();
        self.push_display_message(DisplayMessage::system(format!(
            "📦 **Compaction complete** — context summarized ({}){}",
            event.trigger, tokens_str
        )));
        self.set_status_notice("Context compacted");
    }

    fn set_status_notice(&mut self, text: impl Into<String>) {
        self.status_notice = Some((text.into(), Instant::now()));
    }

    fn set_memory_feature_enabled(&mut self, enabled: bool) {
        self.memory_enabled = enabled;
        if !enabled {
            crate::memory::clear_pending_memory(&self.session.id);
            crate::memory::clear_activity();
            crate::memory_agent::reset();
            self.last_injected_memory_signature = None;
        }
    }

    fn memory_prompt_signature(prompt: &str) -> String {
        prompt
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_lowercase)
            .collect::<Vec<String>>()
            .join("\n")
    }

    fn should_inject_memory_context(&mut self, prompt: &str) -> bool {
        let signature = Self::memory_prompt_signature(prompt);
        let now = Instant::now();
        if let Some((last_signature, last_injected_at)) =
            self.last_injected_memory_signature.as_ref()
        {
            if *last_signature == signature
                && now.duration_since(*last_injected_at).as_secs()
                    < MEMORY_INJECTION_SUPPRESSION_SECS
            {
                return false;
            }
        }
        self.last_injected_memory_signature = Some((signature, now));
        true
    }

    fn set_swarm_feature_enabled(&mut self, enabled: bool) {
        self.swarm_enabled = enabled;
        if !enabled {
            self.remote_swarm_members.clear();
        }
    }

    fn extract_thought_line(text: &str) -> Option<String> {
        let trimmed = text.trim();
        if trimmed.starts_with("Thought for ") && trimmed.ends_with('s') {
            Some(trimmed.to_string())
        } else {
            None
        }
    }

    /// Handle quit request (Ctrl+C/Ctrl+D). Returns true if should actually quit.
    fn handle_quit_request(&mut self) -> bool {
        const QUIT_TIMEOUT: Duration = Duration::from_secs(2);

        if let Some(pending_time) = self.quit_pending {
            if pending_time.elapsed() < QUIT_TIMEOUT {
                // Second press within timeout - actually quit
                // Mark session as closed and save
                self.session.provider_session_id = self.provider_session_id.clone();
                self.session.mark_closed();
                let _ = self.session.save();
                self.should_quit = true;
                return true;
            }
        }

        // First press or timeout expired - show warning
        self.quit_pending = Some(Instant::now());
        self.set_status_notice("Press Ctrl+C again to quit");
        false
    }

    fn missing_tool_result_ids(&self) -> Vec<String> {
        let mut tool_calls = HashSet::new();
        let mut tool_results = HashSet::new();

        for msg in &self.messages {
            match msg.role {
                Role::Assistant => {
                    for block in &msg.content {
                        if let ContentBlock::ToolUse { id, .. } = block {
                            tool_calls.insert(id.clone());
                        }
                    }
                }
                Role::User => {
                    for block in &msg.content {
                        if let ContentBlock::ToolResult { tool_use_id, .. } = block {
                            tool_results.insert(tool_use_id.clone());
                        }
                    }
                }
            }
        }

        tool_calls
            .difference(&tool_results)
            .cloned()
            .collect::<Vec<_>>()
    }

    fn summarize_tool_results_missing(&self) -> Option<String> {
        let missing = self.missing_tool_result_ids();
        if missing.is_empty() {
            return None;
        }
        let sample = missing
            .iter()
            .take(3)
            .map(|id| format!("`{}`", id))
            .collect::<Vec<_>>()
            .join(", ");
        let count = missing.len();
        let suffix = if count > 3 { "..." } else { "" };
        Some(format!(
            "Missing tool outputs for {} call(s): {}{}",
            count, sample, suffix
        ))
    }

    fn repair_missing_tool_outputs(&mut self) -> usize {
        let mut known_results = HashSet::new();
        for msg in &self.messages {
            if let Role::User = msg.role {
                for block in &msg.content {
                    if let ContentBlock::ToolResult { tool_use_id, .. } = block {
                        known_results.insert(tool_use_id.clone());
                    }
                }
            }
        }

        let mut repaired = 0usize;
        let mut index = 0usize;
        while index < self.messages.len() {
            let mut missing_for_message: Vec<String> = Vec::new();
            if let Role::Assistant = self.messages[index].role {
                for block in &self.messages[index].content {
                    if let ContentBlock::ToolUse { id, .. } = block {
                        if !known_results.contains(id) {
                            known_results.insert(id.clone());
                            missing_for_message.push(id.clone());
                        }
                    }
                }
            }

            if !missing_for_message.is_empty() {
                for (offset, id) in missing_for_message.iter().enumerate() {
                    let tool_block = ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: TOOL_OUTPUT_MISSING_TEXT.to_string(),
                        is_error: Some(true),
                    };
                    let inserted_message = Message {
                        role: Role::User,
                        content: vec![tool_block.clone()],
                        timestamp: None,
                    };
                    let stored_message = crate::session::StoredMessage {
                        id: id::new_id("message"),
                        role: Role::User,
                        content: vec![tool_block],
                        timestamp: Some(chrono::Utc::now()),
                        tool_duration_ms: None,
                        token_usage: None,
                    };
                    self.messages.insert(index + 1 + offset, inserted_message);
                    self.session
                        .messages
                        .insert(index + 1 + offset, stored_message);
                    self.tool_result_ids.insert(id.clone());
                    repaired += 1;
                }
                index += missing_for_message.len();
            }

            index += 1;
        }

        if repaired > 0 {
            self.reseed_compaction_from_provider_messages();
            let _ = self.session.save();
        }

        repaired
    }

    /// Rebuild current session into a new one without tool calls
    fn recover_session_without_tools(&mut self) {
        let old_session = self.session.clone();
        let old_messages = old_session.messages.clone();

        let new_session_id = format!("session_recovery_{}", id::new_id("rec"));
        let mut new_session =
            Session::create_with_id(new_session_id, Some(old_session.id.clone()), None);
        new_session.title = old_session.title.clone();
        new_session.provider_session_id = old_session.provider_session_id.clone();
        new_session.model = old_session.model.clone();
        new_session.is_canary = old_session.is_canary;
        new_session.testing_build = old_session.testing_build.clone();
        new_session.is_debug = old_session.is_debug;
        new_session.saved = old_session.saved;
        new_session.save_label = old_session.save_label.clone();
        new_session.working_dir = old_session.working_dir.clone();

        self.clear_provider_messages();
        self.clear_display_messages();
        self.queued_messages.clear();
        self.pasted_contents.clear();
        self.pending_images.clear();
        self.active_skill = None;
        self.provider_session_id = None;
        self.session = new_session;

        for msg in old_messages {
            let role = msg.role.clone();
            let kept_blocks: Vec<ContentBlock> = msg
                .content
                .into_iter()
                .filter(|block| matches!(block, ContentBlock::Text { .. }))
                .collect();
            if kept_blocks.is_empty() {
                continue;
            }
            self.add_provider_message(Message {
                role: role.clone(),
                content: kept_blocks.clone(),
                timestamp: None,
            });
            self.push_display_message(DisplayMessage {
                role: match role {
                    Role::User => "user".to_string(),
                    Role::Assistant => "assistant".to_string(),
                },
                content: kept_blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text, .. } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                tool_calls: vec![],
                duration_secs: None,
                title: None,
                tool_data: None,
            });
            let _ = self.session.add_message(role, kept_blocks);
        }
        let _ = self.session.save();

        self.push_display_message(DisplayMessage::system(format!(
            "Recovery complete. New session: {}. Tool calls stripped; context preserved.",
            self.session.id
        )));
        self.set_status_notice("Recovered session");
    }
    // Getters for UI
    pub fn display_messages(&self) -> &[DisplayMessage] {
        &self.display_messages
    }

    fn bump_display_messages_version(&mut self) {
        self.display_messages_version = self.display_messages_version.wrapping_add(1);
    }

    fn push_display_message(&mut self, message: DisplayMessage) {
        let is_tool = message.role == "tool";
        self.display_messages.push(message);
        self.bump_display_messages_version();
        if is_tool && self.diff_mode.has_side_pane() && self.diff_pane_auto_scroll {
            self.diff_pane_scroll = usize::MAX;
        }
    }

    fn append_reload_message(&mut self, line: &str) {
        if let Some(idx) = self
            .display_messages
            .iter()
            .rposition(Self::is_reload_message)
        {
            let msg = &mut self.display_messages[idx];
            if !msg.content.is_empty() {
                msg.content.push('\n');
            }
            msg.content.push_str(line);
            msg.title = Some("Reload".to_string());
            self.bump_display_messages_version();
        } else {
            self.push_display_message(
                DisplayMessage::system(line.to_string()).with_title("Reload"),
            );
        }
    }

    fn is_reload_message(message: &DisplayMessage) -> bool {
        message.role == "system"
            && message
                .title
                .as_deref()
                .is_some_and(|title| title == "Reload" || title.starts_with("Reload: "))
    }

    fn clear_display_messages(&mut self) {
        if !self.display_messages.is_empty() {
            self.display_messages.clear();
            self.bump_display_messages_version();
        }
    }

    /// Find word boundary going backward (for Ctrl+W, Alt+B)
    fn find_word_boundary_back(&self) -> usize {
        if self.cursor_pos == 0 {
            return 0;
        }
        let mut pos = self.cursor_pos;

        // Move back one char
        pos = super::core::prev_char_boundary(&self.input, pos);

        // Skip trailing whitespace
        while pos > 0 {
            let ch = self.input[pos..].chars().next().unwrap_or(' ');
            if !ch.is_whitespace() {
                break;
            }
            pos = super::core::prev_char_boundary(&self.input, pos);
        }

        // Skip word characters
        while pos > 0 {
            let prev = super::core::prev_char_boundary(&self.input, pos);
            let ch = self.input[prev..].chars().next().unwrap_or(' ');
            if ch.is_whitespace() {
                break;
            }
            pos = prev;
        }

        pos
    }

    /// Find word boundary going forward (for Alt+F, Alt+D)
    fn find_word_boundary_forward(&self) -> usize {
        let len = self.input.len();
        if self.cursor_pos >= len {
            return len;
        }
        let mut pos = self.cursor_pos;

        // Skip current word
        while pos < len {
            let ch = self.input[pos..].chars().next().unwrap_or(' ');
            if ch.is_whitespace() {
                break;
            }
            pos = super::core::next_char_boundary(&self.input, pos);
        }

        // Skip whitespace
        while pos < len {
            let ch = self.input[pos..].chars().next().unwrap_or(' ');
            if !ch.is_whitespace() {
                break;
            }
            pos = super::core::next_char_boundary(&self.input, pos);
        }

        pos
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    fn fuzzy_score(needle: &str, haystack: &str) -> Option<usize> {
        if needle.is_empty() {
            return Some(0);
        }
        // Both needle and haystack should start with '/', match from char 1 onward
        let n = needle.strip_prefix('/').unwrap_or(needle);
        let h = haystack.strip_prefix('/').unwrap_or(haystack);
        if n.is_empty() {
            return Some(0);
        }
        // First char of the command (after /) must match
        if !h.starts_with(&n[..n.chars().next().unwrap().len_utf8()]) {
            return None;
        }
        let mut score = 0usize;
        let mut pos = 0usize;
        for ch in n.chars() {
            let Some(idx) = h[pos..].find(ch) else {
                return None;
            };
            score += idx;
            pos += idx + ch.len_utf8();
        }
        // Penalize large gaps - reject if average gap is too big
        if n.len() > 1 && score > n.len() * 3 {
            return None;
        }
        Some(score)
    }

    fn rank_suggestions(
        &self,
        needle: &str,
        candidates: Vec<(String, &'static str)>,
    ) -> Vec<(String, &'static str)> {
        let needle = needle.to_lowercase();
        let mut scored: Vec<(bool, usize, String, &'static str)> = Vec::new();
        for (cmd, help) in candidates {
            let lower = cmd.to_lowercase();
            if lower.starts_with(&needle) {
                scored.push((true, 0, cmd, help));
            } else if let Some(score) = Self::fuzzy_score(&needle, &lower) {
                scored.push((false, score, cmd, help));
            }
        }
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.len().cmp(&b.2.len()))
                .then_with(|| a.2.cmp(&b.2))
        });
        scored
            .into_iter()
            .map(|(_, _, cmd, help)| (cmd, help))
            .collect()
    }

    /// Get command suggestions based on current input (or base input for cycling)
    fn get_suggestions_for(&self, input: &str) -> Vec<(String, &'static str)> {
        let input = input.trim();

        // Only show suggestions when input starts with /
        if !input.starts_with('/') {
            return vec![];
        }

        let prefix = input.to_lowercase();

        // /model opens the interactive picker — don't list individual models in autocomplete
        if prefix == "/model" || prefix.starts_with("/model ") || prefix.starts_with("/models") {
            return vec![("/model".into(), "Open model picker")];
        }

        if prefix.starts_with("/effort ") {
            let efforts = ["none", "low", "medium", "high", "xhigh"];
            return efforts
                .iter()
                .map(|e| (format!("/effort {}", e), effort_display_label(e)))
                .collect();
        }

        if prefix.starts_with("/login ") || prefix.starts_with("/auth ") {
            return crate::provider_catalog::tui_login_providers()
                .iter()
                .map(|provider| (format!("/login {}", provider.id), provider.menu_detail))
                .collect();
        }

        if prefix.starts_with("/account ") || prefix.starts_with("/accounts ") {
            let mut suggestions = vec![
                ("/account list".into(), "List all Anthropic accounts"),
                (
                    "/account add".into(),
                    "Add a new account (start OAuth login)",
                ),
                ("/account switch".into(), "Switch active account"),
                ("/account remove".into(), "Remove an account"),
            ];
            if let Ok(accounts) = crate::auth::claude::list_accounts() {
                for account in accounts {
                    suggestions.push((
                        format!("/account switch {}", account.label),
                        "Switch to this account",
                    ));
                }
            }
            return suggestions;
        }

        // Built-in commands
        let mut commands: Vec<(String, &'static str)> = vec![
            ("/help".into(), "Show help and keyboard shortcuts"),
            ("/commands".into(), "Alias for /help"),
            ("/model".into(), "List or switch models"),
            ("/effort".into(), "Show/change reasoning effort (Alt+←/→)"),
            ("/clear".into(), "Clear conversation history"),
            ("/rewind".into(), "Rewind conversation to previous message"),
            ("/poke".into(), "Poke model to resume with incomplete todos"),
            (
                "/compact".into(),
                "Compact context (summarize old messages)",
            ),
            ("/fix".into(), "Recover when the model cannot continue"),
            (
                "/remember".into(),
                "Extract and save memories from conversation",
            ),
            ("/memory".into(), "Toggle memory feature (on/off/status)"),
            ("/swarm".into(), "Toggle swarm feature (on/off/status)"),
            ("/version".into(), "Show current version"),
            ("/changelog".into(), "Show recent changes in this build"),
            ("/info".into(), "Show session info and tokens"),
            ("/usage".into(), "Show subscription usage limits"),
            ("/reload".into(), "Smart reload (if newer binary exists)"),
            ("/rebuild".into(), "Full rebuild (git pull + build + tests)"),
            ("/update".into(), "Check for and install latest release"),
            ("/resume".into(), "Open session picker"),
            ("/save".into(), "Bookmark session for easy access"),
            ("/unsave".into(), "Remove bookmark from session"),
            ("/split".into(), "Split session into a new window"),
            ("/quit".into(), "Exit jcode"),
            ("/auth".into(), "Show authentication status"),
            ("/cache".into(), "Toggle cache TTL between 5min and 1h"),
            (
                "/login".into(),
                "Login to a provider (use `/login <provider>` for the full list)",
            ),
            (
                "/account".into(),
                "Manage Anthropic accounts (list/add/switch/remove)",
            ),
        ];

        // Add client-reload and server-reload commands in remote mode
        if self.is_remote {
            commands.push(("/client-reload".into(), "Force reload client binary"));
            commands.push(("/server-reload".into(), "Force reload server binary"));
        }

        // Add skills as commands
        let skills = self.skills.list();
        for skill in skills {
            commands.push((format!("/{}", skill.name), "Activate skill"));
        }

        // Filter by prefix match
        self.rank_suggestions(&prefix, commands)
    }

    /// Get command suggestions based on current input
    pub fn command_suggestions(&self) -> Vec<(String, &'static str)> {
        self.get_suggestions_for(&self.input)
    }

    /// Get suggestion prompts for new users on the initial empty screen.
    /// Returns (label, prompt_text) pairs. Empty once user is experienced or not authenticated.
    pub fn suggestion_prompts(&self) -> Vec<(String, String)> {
        let is_canary = if self.is_remote {
            self.remote_is_canary.unwrap_or(self.session.is_canary)
        } else {
            self.session.is_canary
        };
        if is_canary {
            return Vec::new();
        }

        let auth = crate::auth::AuthStatus::check();
        if !auth.has_any_available() {
            return vec![("Log in to get started".to_string(), "/login".to_string())];
        }

        if !self.display_messages.is_empty() || self.is_processing {
            return Vec::new();
        }

        let is_new_user = crate::storage::jcode_dir()
            .ok()
            .and_then(|dir| {
                let path = dir.join("setup_hints.json");
                std::fs::read_to_string(&path).ok()
            })
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .and_then(|v| v.get("launch_count")?.as_u64())
            .map(|count| count <= 5)
            .unwrap_or(true);

        if !is_new_user {
            return Vec::new();
        }

        vec![
            (
                "Customize my terminal theme".to_string(),
                "Find what terminal I'm using, then change its background color to pitch black and make it slightly transparent. Apply the changes for me.".to_string(),
            ),
            (
                "Review something I've been working on".to_string(),
                "Find a recent file or project I've been working on, read through it, and give me concrete suggestions on how I could improve it.".to_string(),
            ),
            (
                "Find my social media and roast me".to_string(),
                "Find a social media platform I use, look around at my profile and posts, then give me a brutally honest roast based on what you see.".to_string(),
            ),
        ]
    }

    /// Autocomplete current input - cycles through suggestions on repeated Tab
    pub fn autocomplete(&mut self) -> bool {
        // Get suggestions for current input
        let current_suggestions = self.get_suggestions_for(&self.input);

        // Check if we're continuing a tab cycle from a previous base
        if let Some((ref base, idx)) = self.tab_completion_state.clone() {
            let base_suggestions = self.get_suggestions_for(&base);

            // If current input is in base suggestions AND there are multiple options, continue cycling
            if base_suggestions.len() > 1
                && base_suggestions.iter().any(|(cmd, _)| cmd == &self.input)
            {
                let next_index = (idx + 1) % base_suggestions.len();
                let (cmd, _) = &base_suggestions[next_index];
                self.input = cmd.clone();
                self.cursor_pos = self.input.len();
                self.tab_completion_state = Some((base.clone(), next_index));
                return true;
            }
            // Otherwise, fall through to start a new cycle with current input
        }

        // Start fresh cycle with current input
        if current_suggestions.is_empty() {
            self.tab_completion_state = None;
            return false;
        }

        // If only one suggestion and it matches exactly, add trailing space for commands
        // that accept arguments, then we're done
        if current_suggestions.len() == 1 && current_suggestions[0].0 == self.input {
            if !self.input.ends_with(' ') && Self::command_accepts_args(&self.input) {
                self.input.push(' ');
                self.cursor_pos = self.input.len();
                return true;
            }
            self.tab_completion_state = None;
            return false;
        }

        // Apply first suggestion and start tracking the cycle
        let (cmd, _) = &current_suggestions[0];
        let base = self.input.clone();
        self.input = cmd.clone();
        // If unique match, add trailing space for arg-accepting commands
        if current_suggestions.len() == 1 && Self::command_accepts_args(&self.input) {
            self.input.push(' ');
        }
        self.cursor_pos = self.input.len();
        self.tab_completion_state = Some((base, 0));
        true
    }

    /// Reset tab completion state (call when user types/modifies input)
    pub fn reset_tab_completion(&mut self) {
        self.tab_completion_state = None;
    }

    fn command_accepts_args(cmd: &str) -> bool {
        matches!(
            cmd.trim(),
            "/help"
                | "/model"
                | "/effort"
                | "/login"
                | "/auth"
                | "/account"
                | "/memory"
                | "/swarm"
                | "/rewind"
                | "/config"
                | "/save"
                | "/cache"
        )
    }

    pub fn cursor_pos(&self) -> usize {
        self.cursor_pos
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    pub fn is_processing(&self) -> bool {
        self.is_processing
    }

    pub fn streaming_text(&self) -> &str {
        &self.streaming_text
    }

    pub fn active_skill(&self) -> Option<&str> {
        self.active_skill.as_deref()
    }

    pub fn available_skills(&self) -> Vec<&str> {
        self.skills.list().iter().map(|s| s.name.as_str()).collect()
    }

    pub fn queued_count(&self) -> usize {
        self.queued_messages.len()
    }

    pub fn queued_messages(&self) -> &[String] {
        &self.queued_messages
    }

    pub fn streaming_tokens(&self) -> (u64, u64) {
        (self.streaming_input_tokens, self.streaming_output_tokens)
    }

    fn build_turn_footer(&self, duration: Option<f32>) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(secs) = duration {
            parts.push(format!("{:.1}s", secs));
        }
        if let Some(tps) = self.compute_streaming_tps() {
            parts.push(format!("{:.1} tps", tps));
        }
        if self.streaming_input_tokens > 0 || self.streaming_output_tokens > 0 {
            parts.push(format!(
                "↑{} ↓{}",
                format_tokens(self.streaming_input_tokens),
                format_tokens(self.streaming_output_tokens)
            ));
        }
        if let Some(cache) = format_cache_footer(
            self.streaming_cache_read_tokens,
            self.streaming_cache_creation_tokens,
        ) {
            parts.push(cache);
        }

        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" · "))
        }
    }

    fn push_turn_footer(&mut self, duration: Option<f32>) {
        self.log_cache_miss_if_unexpected();

        self.last_api_completed = Some(Instant::now());
        self.last_turn_input_tokens = {
            let input = self.streaming_input_tokens;
            if input > 0 {
                Some(input)
            } else {
                None
            }
        };

        if let Some(footer) = self.build_turn_footer(duration) {
            self.push_display_message(DisplayMessage {
                role: "meta".to_string(),
                content: footer,
                tool_calls: vec![],
                duration_secs: None,
                title: None,
                tool_data: None,
            });
        }
    }

    /// Log detailed info when an unexpected cache miss occurs (cache write on turn 3+)
    fn log_cache_miss_if_unexpected(&self) {
        let user_turn_count = self
            .display_messages
            .iter()
            .filter(|m| m.role == "user")
            .count();

        // Unexpected cache miss: on turn 3+, we should no longer be in cache warm-up
        let is_unexpected = super::is_unexpected_cache_miss(
            user_turn_count,
            self.streaming_cache_read_tokens,
            self.streaming_cache_creation_tokens,
        );

        if is_unexpected {
            // Collect context for debugging
            let session_id = self.session_id().to_string();
            let provider = self.provider.name().to_string();
            let model = self.provider.model();
            let input_tokens = self.streaming_input_tokens;
            let output_tokens = self.streaming_output_tokens;

            // Format as Option to distinguish None vs Some(0)
            let cache_creation_dbg = format!("{:?}", self.streaming_cache_creation_tokens);
            let cache_read_dbg = format!("{:?}", self.streaming_cache_read_tokens);

            // Count message types in conversation
            let mut user_msgs = 0;
            let mut assistant_msgs = 0;
            let mut tool_msgs = 0;
            let mut other_msgs = 0;
            for msg in &self.display_messages {
                match msg.role.as_str() {
                    "user" => user_msgs += 1,
                    "assistant" => assistant_msgs += 1,
                    "tool_result" | "tool_use" => tool_msgs += 1,
                    _ => other_msgs += 1,
                }
            }

            crate::logging::warn(&format!(
                "CACHE_MISS: unexpected cache miss on turn {} | \
                 cache_creation={} cache_read={} | \
                 input={} output={} | \
                 session={} provider={} model={} | \
                 msgs: user={} assistant={} tool={} other={}",
                user_turn_count,
                cache_creation_dbg,
                cache_read_dbg,
                input_tokens,
                output_tokens,
                session_id,
                provider,
                model,
                user_msgs,
                assistant_msgs,
                tool_msgs,
                other_msgs
            ));
        }
    }

    /// Check if approaching context limit and show warning
    fn check_context_warning(&mut self, input_tokens: u64) {
        let usage_percent = (input_tokens as f64 / self.context_limit as f64) * 100.0;

        // Warn at 70%, 80%, 90%
        if !self.context_warning_shown && usage_percent >= 70.0 {
            let warning = format!(
                "\n⚠️  Context usage: {:.0}% ({}/{}k tokens) - compaction approaching\n\n",
                usage_percent,
                input_tokens / 1000,
                self.context_limit / 1000
            );
            self.streaming_text.push_str(&warning);
            self.context_warning_shown = true;
        } else if self.context_warning_shown && usage_percent >= 80.0 {
            // Reset to show 80% warning
            if usage_percent < 85.0 {
                let warning = format!(
                    "\n⚠️  Context usage: {:.0}% - compaction imminent\n\n",
                    usage_percent
                );
                self.streaming_text.push_str(&warning);
            }
        }
    }

    /// Get context usage as percentage
    pub fn context_usage_percent(&self) -> f64 {
        self.current_stream_context_tokens()
            .map(|tokens| (tokens as f64 / self.context_limit as f64) * 100.0)
            .unwrap_or(0.0)
    }

    /// Time since last streaming event (for detecting stale connections)
    pub fn time_since_activity(&self) -> Option<Duration> {
        self.last_stream_activity.map(|t| t.elapsed())
    }

    pub fn streaming_tool_calls(&self) -> &[ToolCall] {
        &self.streaming_tool_calls
    }

    pub fn status(&self) -> &ProcessingStatus {
        &self.status
    }

    pub fn subagent_status(&self) -> Option<&str> {
        self.subagent_status.as_deref()
    }

    pub fn elapsed(&self) -> Option<Duration> {
        if let Some(d) = self.replay_elapsed_override {
            return Some(d);
        }
        self.processing_started.map(|t| t.elapsed())
    }

    pub fn provider_name(&self) -> &str {
        self.provider.name()
    }

    pub fn provider_model(&self) -> String {
        self.provider.model()
    }

    /// Get the upstream provider (e.g., which provider OpenRouter routed to)
    pub fn upstream_provider(&self) -> Option<&str> {
        self.upstream_provider.as_deref()
    }

    pub fn mcp_servers(&self) -> Vec<(String, usize)> {
        self.mcp_server_names.clone()
    }

    /// Scroll to the previous user prompt (scroll up - earlier in conversation)
    pub fn scroll_to_prev_prompt(&mut self) {
        let positions = super::ui::last_user_prompt_positions();
        if positions.is_empty() {
            return;
        }

        let current = self.scroll_offset;

        // positions are in document order (top to bottom).
        // Find the last position that is strictly less than current (i.e. earlier/above).
        // If we're at the bottom (!auto_scroll_paused), treat current as past-the-end.
        if !self.auto_scroll_paused {
            // Jump to the most recent (last) prompt
            if let Some(&pos) = positions.last() {
                self.scroll_offset = pos;
                self.auto_scroll_paused = true;
            }
            return;
        }

        let mut target = None;
        for &pos in positions.iter().rev() {
            if pos < current {
                target = Some(pos);
                break;
            }
        }

        if let Some(pos) = target {
            self.scroll_offset = pos;
        }
        // If no prompt above, stay where we are
    }

    /// Scroll to the next user prompt (scroll down - later in conversation)
    pub fn scroll_to_next_prompt(&mut self) {
        let positions = super::ui::last_user_prompt_positions();
        if positions.is_empty() || !self.auto_scroll_paused {
            return;
        }

        let current = self.scroll_offset;

        // Find the first position strictly greater than current (i.e. later/below).
        for &pos in &positions {
            if pos > current {
                self.scroll_offset = pos;
                return;
            }
        }

        // No more prompts below - go to bottom
        self.follow_chat_bottom();
    }

    /// Scroll to Nth most-recent user prompt (1 = most recent, 2 = second most recent, etc.).
    /// Uses actual wrapped line positions from the last render frame for accurate placement,
    /// positioning the prompt at the top of the viewport.
    fn scroll_to_recent_prompt_rank(&mut self, rank: usize) {
        let rank = rank.max(1);
        let positions = super::ui::last_user_prompt_positions();
        let max_scroll = super::ui::last_max_scroll();

        if positions.is_empty() {
            return;
        }

        // positions are in document order (top to bottom), we want most-recent first
        let target_idx = positions.len().saturating_sub(rank);
        let target_line = positions[target_idx];
        self.set_status_notice(format!(
            "Ctrl+{}: idx={}/{} line={} max={}",
            rank,
            target_idx,
            positions.len(),
            target_line,
            max_scroll
        ));
        self.scroll_offset = target_line;
        self.auto_scroll_paused = true;
    }

    fn toggle_input_stash(&mut self) {
        if let Some((stashed, stashed_cursor)) = self.stashed_input.take() {
            let current_input = std::mem::replace(&mut self.input, stashed);
            let current_cursor = std::mem::replace(&mut self.cursor_pos, stashed_cursor);
            if current_input.is_empty() {
                self.set_status_notice("📋 Input restored from stash");
            } else {
                self.stashed_input = Some((current_input, current_cursor));
                self.set_status_notice("📋 Swapped input with stash");
            }
        } else if !self.input.is_empty() {
            let input = std::mem::take(&mut self.input);
            let cursor = std::mem::replace(&mut self.cursor_pos, 0);
            self.stashed_input = Some((input, cursor));
            self.set_status_notice("📋 Input stashed");
        }
    }

    fn save_input_for_reload(&self, session_id: &str) {
        if self.input.is_empty() {
            return;
        }
        if let Ok(jcode_dir) = crate::storage::jcode_dir() {
            let path = jcode_dir.join(format!("client-input-{}", session_id));
            let data = format!("{}\n{}", self.cursor_pos, self.input);
            let _ = std::fs::write(&path, &data);
        }
    }

    fn restore_input_from_reload(session_id: &str) -> Option<(String, usize)> {
        let jcode_dir = crate::storage::jcode_dir().ok()?;
        let path = jcode_dir.join(format!("client-input-{}", session_id));
        if !path.exists() {
            return None;
        }
        let data = std::fs::read_to_string(&path).ok()?;
        let _ = std::fs::remove_file(&path);
        let (cursor_str, input) = data.split_once('\n')?;
        let cursor = cursor_str.parse::<usize>().unwrap_or(0);
        let cursor = cursor.min(input.len());
        Some((input.to_string(), cursor))
    }

    /// Toggle scroll bookmark: stash current position and jump to bottom,
    /// or restore stashed position if already at bottom.
    fn toggle_scroll_bookmark(&mut self) {
        if let Some(saved) = self.scroll_bookmark.take() {
            // We have a bookmark — teleport back to it
            self.scroll_offset = saved;
            self.auto_scroll_paused = saved > 0;
            self.set_status_notice("📌 Returned to bookmark");
        } else if self.auto_scroll_paused && self.scroll_offset > 0 {
            // We're scrolled up — save position and jump to bottom
            self.scroll_bookmark = Some(self.scroll_offset);
            self.follow_chat_bottom();
            self.set_status_notice("📌 Bookmark set — press again to return");
        }
        // If already at bottom with no bookmark, do nothing
    }

    fn toggle_centered_mode(&mut self) {
        self.centered = !self.centered;
        let mode = if self.centered {
            "Centered"
        } else {
            "Left-aligned"
        };
        self.set_status_notice(format!("Layout: {}", mode));
    }

    pub fn set_centered(&mut self, centered: bool) {
        self.centered = centered;
    }

    // ==================== Debug Socket Methods ====================

    /// Enable debug socket and return the broadcast receiver
    /// Call this before run() to enable debug event broadcasting
    pub fn enable_debug_socket(
        &mut self,
    ) -> tokio::sync::broadcast::Receiver<super::backend::DebugEvent> {
        let (tx, rx) = tokio::sync::broadcast::channel(256);
        self.debug_tx = Some(tx);
        rx
    }

    /// Broadcast a debug event to connected clients (if debug socket enabled)
    fn broadcast_debug(&self, event: super::backend::DebugEvent) {
        if let Some(ref tx) = self.debug_tx {
            let _ = tx.send(event); // Ignore errors (no receivers)
        }
    }

    /// Create a full state snapshot for debug socket
    pub fn create_debug_snapshot(&self) -> super::backend::DebugEvent {
        use super::backend::{DebugEvent, DebugMessage};

        DebugEvent::StateSnapshot {
            display_messages: self
                .display_messages
                .iter()
                .map(|m| DebugMessage {
                    role: m.role.clone(),
                    content: m.content.clone(),
                    tool_calls: m.tool_calls.clone(),
                    duration_secs: m.duration_secs,
                    title: m.title.clone(),
                    tool_data: m.tool_data.clone(),
                })
                .collect(),
            streaming_text: self.streaming_text.clone(),
            streaming_tool_calls: self.streaming_tool_calls.clone(),
            input: self.input.clone(),
            cursor_pos: self.cursor_pos,
            is_processing: self.is_processing,
            scroll_offset: self.scroll_offset,
            status: format!("{:?}", self.status),
            provider_name: self.provider.name().to_string(),
            provider_model: self.provider.model().to_string(),
            mcp_servers: self
                .mcp_server_names
                .iter()
                .map(|(name, _)| name.clone())
                .collect(),
            skills: self.skills.list().iter().map(|s| s.name.clone()).collect(),
            session_id: self.provider_session_id.clone(),
            input_tokens: self.streaming_input_tokens,
            output_tokens: self.streaming_output_tokens,
            cache_read_input_tokens: self.streaming_cache_read_tokens,
            cache_creation_input_tokens: self.streaming_cache_creation_tokens,
            queued_messages: self.queued_messages.clone(),
        }
    }

    /// Start debug socket listener task
    /// Returns a JoinHandle for the listener task
    pub fn start_debug_socket_listener(
        &self,
        mut rx: tokio::sync::broadcast::Receiver<super::backend::DebugEvent>,
    ) -> tokio::task::JoinHandle<()> {
        use crate::transport::Listener;
        use tokio::io::AsyncWriteExt;

        let socket_path = Self::debug_socket_path();
        let initial_snapshot = self.create_debug_snapshot();

        tokio::spawn(async move {
            // Clean up old socket
            let _ = std::fs::remove_file(&socket_path);

            let mut listener = match Listener::bind(&socket_path) {
                Ok(l) => l,
                Err(e) => {
                    crate::logging::error(&format!("Failed to bind debug socket: {}", e));
                    return;
                }
            };

            // Restrict TUI debug socket to owner-only.
            let _ = crate::platform::set_permissions_owner_only(&socket_path);

            // Accept connections and forward events
            let clients: std::sync::Arc<tokio::sync::Mutex<Vec<crate::transport::WriteHalf>>> =
                std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));

            let clients_clone = clients.clone();

            // Spawn event broadcaster
            let broadcast_handle = tokio::spawn(async move {
                while let Ok(event) = rx.recv().await {
                    let json = match serde_json::to_string(&event) {
                        Ok(j) => j + "\n",
                        Err(_) => continue,
                    };
                    let bytes = json.as_bytes();

                    let mut clients = clients_clone.lock().await;
                    let mut to_remove = Vec::new();

                    for (i, writer) in clients.iter_mut().enumerate() {
                        if writer.write_all(bytes).await.is_err() {
                            to_remove.push(i);
                        }
                    }

                    // Remove disconnected clients (reverse order to preserve indices)
                    for i in to_remove.into_iter().rev() {
                        clients.swap_remove(i);
                    }
                }
            });

            // Accept new connections
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let (_, writer) = stream.into_split();
                        let mut writer = writer;

                        // Send initial snapshot
                        let snapshot_json =
                            serde_json::to_string(&initial_snapshot).unwrap_or_default() + "\n";
                        if writer.write_all(snapshot_json.as_bytes()).await.is_ok() {
                            clients.lock().await.push(writer);
                        }
                    }
                    Err(_) => break,
                }
            }

            broadcast_handle.abort();
            let _ = std::fs::remove_file(&socket_path);
        })
    }

    /// Get the debug socket path
    pub fn debug_socket_path() -> std::path::PathBuf {
        crate::storage::runtime_dir().join("jcode-debug.sock")
    }
}

/// Update cost calculation based on token usage (for API-key providers)
impl App {
    fn update_cost_impl(&mut self) {
        let provider_name = self.provider.name().to_lowercase();

        // Only calculate cost for API-key providers
        if !provider_name.contains("openrouter")
            && !provider_name.contains("anthropic")
            && !provider_name.contains("openai")
        {
            return;
        }

        // For OAuth providers, cost is already tracked in subscription
        let is_oauth = (provider_name.contains("anthropic") || provider_name.contains("claude"))
            && std::env::var("ANTHROPIC_API_KEY").is_err();
        if is_oauth {
            return;
        }

        // Default pricing (will be cached after first turn)
        let prompt_price = *self.cached_prompt_price.get_or_insert(15.0); // $15/1M tokens default
        let completion_price = *self.cached_completion_price.get_or_insert(60.0); // $60/1M tokens default

        // Calculate cost for this turn
        let prompt_cost = (self.streaming_input_tokens as f32 * prompt_price) / 1_000_000.0;
        let completion_cost =
            (self.streaming_output_tokens as f32 * completion_price) / 1_000_000.0;
        self.total_cost += prompt_cost + completion_cost;
    }

    fn compute_streaming_tps(&self) -> Option<f32> {
        let mut elapsed = self.streaming_tps_elapsed;
        let total_tokens = self.streaming_total_output_tokens;
        if let Some(start) = self.streaming_tps_start {
            elapsed += start.elapsed();
        }
        let elapsed_secs = elapsed.as_secs_f32();
        if elapsed_secs > 0.1 && total_tokens > 0 {
            Some(total_tokens as f32 / elapsed_secs)
        } else {
            None
        }
    }
    fn handle_changelog_key(&mut self, code: KeyCode) -> Result<()> {
        let scroll = self.changelog_scroll.unwrap_or(0);
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.changelog_scroll = None;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.changelog_scroll = Some(scroll.saturating_add(1));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.changelog_scroll = Some(scroll.saturating_sub(1));
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.changelog_scroll = Some(scroll.saturating_add(20));
            }
            KeyCode::PageUp => {
                self.changelog_scroll = Some(scroll.saturating_sub(20));
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.changelog_scroll = Some(0);
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.changelog_scroll = Some(usize::MAX);
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_help_key(&mut self, code: KeyCode) -> Result<()> {
        let scroll = self.help_scroll.unwrap_or(0);
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.help_scroll = None;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.help_scroll = Some(scroll.saturating_add(1));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.help_scroll = Some(scroll.saturating_sub(1));
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.help_scroll = Some(scroll.saturating_add(20));
            }
            KeyCode::PageUp => {
                self.help_scroll = Some(scroll.saturating_sub(20));
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.help_scroll = Some(0);
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.help_scroll = Some(usize::MAX);
            }
            _ => {}
        }
        Ok(())
    }
}



#[cfg(test)]
mod tests;
