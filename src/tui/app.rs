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
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::interval;

mod auth;
mod commands;
mod conversation_state;
mod debug;
mod event_wrappers;
mod helpers;
mod input;
mod local;
mod misc_ui;
mod tui_lifecycle;
mod model_context;
mod navigation;
mod picker;
mod remote;
mod replay;
mod state_ui;
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

}

#[cfg(test)]
mod tests;
