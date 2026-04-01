use crate::storage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const TELEMETRY_ENDPOINT: &str = "https://jcode-telemetry.jeremyhuang55555.workers.dev/v1/event";
const ASYNC_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const BLOCKING_INSTALL_TIMEOUT: Duration = Duration::from_millis(1200);
const BLOCKING_LIFECYCLE_TIMEOUT: Duration = Duration::from_millis(800);

static SESSION_STATE: Mutex<Option<SessionTelemetry>> = Mutex::new(None);

static ERROR_PROVIDER_TIMEOUT: AtomicU32 = AtomicU32::new(0);
static ERROR_AUTH_FAILED: AtomicU32 = AtomicU32::new(0);
static ERROR_TOOL_ERROR: AtomicU32 = AtomicU32::new(0);
static ERROR_MCP_ERROR: AtomicU32 = AtomicU32::new(0);
static ERROR_RATE_LIMITED: AtomicU32 = AtomicU32::new(0);
static PROVIDER_SWITCHES: AtomicU32 = AtomicU32::new(0);
static MODEL_SWITCHES: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstallEvent {
    id: String,
    event: &'static str,
    version: String,
    os: &'static str,
    arch: &'static str,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpgradeEvent {
    id: String,
    event: &'static str,
    version: String,
    os: &'static str,
    arch: &'static str,
    from_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthEvent {
    id: String,
    event: &'static str,
    version: String,
    os: &'static str,
    arch: &'static str,
    auth_provider: String,
    auth_method: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionStartEvent {
    id: String,
    event: &'static str,
    version: String,
    os: &'static str,
    arch: &'static str,
    provider_start: String,
    model_start: String,
    resumed_session: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionLifecycleEvent {
    id: String,
    event: &'static str,
    version: String,
    os: &'static str,
    arch: &'static str,
    provider_start: String,
    provider_end: String,
    model_start: String,
    model_end: String,
    provider_switches: u32,
    model_switches: u32,
    duration_mins: u64,
    duration_secs: u64,
    turns: u32,
    had_user_prompt: bool,
    had_assistant_response: bool,
    assistant_responses: u32,
    first_assistant_response_ms: Option<u64>,
    first_tool_call_ms: Option<u64>,
    first_tool_success_ms: Option<u64>,
    tool_calls: u32,
    tool_failures: u32,
    executed_tool_calls: u32,
    executed_tool_successes: u32,
    executed_tool_failures: u32,
    tool_latency_total_ms: u64,
    tool_latency_max_ms: u64,
    file_write_calls: u32,
    tests_run: u32,
    tests_passed: u32,
    feature_memory_used: bool,
    feature_swarm_used: bool,
    feature_web_used: bool,
    feature_email_used: bool,
    feature_mcp_used: bool,
    feature_side_panel_used: bool,
    feature_goal_used: bool,
    feature_selfdev_used: bool,
    feature_background_used: bool,
    feature_subagent_used: bool,
    unique_mcp_servers: u32,
    session_success: bool,
    abandoned_before_response: bool,
    transport_https: u32,
    transport_persistent_ws_fresh: u32,
    transport_persistent_ws_reuse: u32,
    transport_cli_subprocess: u32,
    transport_native_http2: u32,
    transport_other: u32,
    resumed_session: bool,
    end_reason: &'static str,
    errors: ErrorCounts,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ErrorCounts {
    provider_timeout: u32,
    auth_failed: u32,
    tool_error: u32,
    mcp_error: u32,
    rate_limited: u32,
}

struct SessionTelemetry {
    started_at: Instant,
    provider_start: String,
    model_start: String,
    turns: u32,
    had_user_prompt: bool,
    had_assistant_response: bool,
    assistant_responses: u32,
    first_assistant_response_ms: Option<u64>,
    first_tool_call_ms: Option<u64>,
    first_tool_success_ms: Option<u64>,
    tool_calls: u32,
    tool_failures: u32,
    executed_tool_calls: u32,
    executed_tool_successes: u32,
    executed_tool_failures: u32,
    tool_latency_total_ms: u64,
    tool_latency_max_ms: u64,
    file_write_calls: u32,
    tests_run: u32,
    tests_passed: u32,
    feature_memory_used: bool,
    feature_swarm_used: bool,
    feature_web_used: bool,
    feature_email_used: bool,
    feature_mcp_used: bool,
    feature_side_panel_used: bool,
    feature_goal_used: bool,
    feature_selfdev_used: bool,
    feature_background_used: bool,
    feature_subagent_used: bool,
    unique_mcp_servers: HashSet<String>,
    transport_https: u32,
    transport_persistent_ws_fresh: u32,
    transport_persistent_ws_reuse: u32,
    transport_cli_subprocess: u32,
    transport_native_http2: u32,
    transport_other: u32,
    resumed_session: bool,
    start_event_sent: bool,
}

#[derive(Debug, Clone, Copy)]
enum DeliveryMode {
    Background,
    Blocking(Duration),
}

#[derive(Debug, Clone, Copy)]
pub enum SessionEndReason {
    NormalExit,
    Panic,
    Signal,
    Disconnect,
    Reload,
    Unknown,
}

impl SessionEndReason {
    fn as_str(self) -> &'static str {
        match self {
            SessionEndReason::NormalExit => "normal_exit",
            SessionEndReason::Panic => "panic",
            SessionEndReason::Signal => "signal",
            SessionEndReason::Disconnect => "disconnect",
            SessionEndReason::Reload => "reload",
            SessionEndReason::Unknown => "unknown",
        }
    }
}

pub fn is_enabled() -> bool {
    if std::env::var("JCODE_NO_TELEMETRY").is_ok() || std::env::var("DO_NOT_TRACK").is_ok() {
        return false;
    }
    if let Ok(dir) = storage::jcode_dir() {
        if dir.join("no_telemetry").exists() {
            return false;
        }
    }
    true
}

fn telemetry_id_path() -> Option<PathBuf> {
    storage::jcode_dir().ok().map(|d| d.join("telemetry_id"))
}

fn install_recorded_path() -> Option<PathBuf> {
    storage::jcode_dir()
        .ok()
        .map(|d| d.join("telemetry_install_sent"))
}

fn version_recorded_path() -> Option<PathBuf> {
    storage::jcode_dir().ok().map(|d| d.join("telemetry_version_sent"))
}

fn write_private_file(path: &PathBuf, value: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, value);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

fn get_or_create_id() -> Option<String> {
    let path = telemetry_id_path()?;
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim().to_string();
        if !id.is_empty() {
            return Some(id);
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    write_private_file(&path, &id);
    Some(id)
}

fn is_first_run() -> bool {
    telemetry_id_path().map(|p| !p.exists()).unwrap_or(false)
}

fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

fn install_recorded_for_id(id: &str) -> bool {
    install_recorded_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|stored| stored.trim() == id)
        .unwrap_or(false)
}

fn mark_install_recorded(id: &str) {
    if let Some(path) = install_recorded_path() {
        write_private_file(&path, id);
    }
}

fn previously_recorded_version() -> Option<String> {
    version_recorded_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn mark_current_version_recorded() {
    if let Some(path) = version_recorded_path() {
        write_private_file(&path, &version());
    }
}

fn now_ms_since(started_at: Instant) -> u64 {
    started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn mark_tool_feature_usage(state: &mut SessionTelemetry, name: &str, input: &Value) {
    match name {
        "memory" => state.feature_memory_used = true,
        "communicate" => state.feature_swarm_used = true,
        "webfetch" | "websearch" | "codesearch" => state.feature_web_used = true,
        "gmail" => state.feature_email_used = true,
        "side_panel" => state.feature_side_panel_used = true,
        "goal" => state.feature_goal_used = true,
        "selfdev" => state.feature_selfdev_used = true,
        "bg" | "schedule" => state.feature_background_used = true,
        "subagent" => state.feature_subagent_used = true,
        _ => {}
    }

    if matches!(name, "write" | "edit" | "multiedit" | "patch" | "apply_patch") {
        state.file_write_calls += 1;
    }

    if name == "mcp" || name.starts_with("mcp__") {
        state.feature_mcp_used = true;
        if let Some(server) = mcp_server_name(name, input) {
            state.unique_mcp_servers.insert(server);
        }
    }

    if looks_like_test_run(name, input) {
        state.tests_run += 1;
    }
}

fn mark_tool_success_side_effects(state: &mut SessionTelemetry, name: &str, input: &Value) {
    if looks_like_test_run(name, input) {
        state.tests_passed += 1;
    }

    if state.first_tool_success_ms.is_none() {
        state.first_tool_success_ms = Some(now_ms_since(state.started_at));
    }

    if name == "memory" {
        state.feature_memory_used = true;
    }
}

fn looks_like_test_run(name: &str, input: &Value) -> bool {
    let mut haystacks = Vec::new();
    haystacks.push(name.to_ascii_lowercase());

    if let Some(command) = input.get("command").and_then(Value::as_str) {
        haystacks.push(command.to_ascii_lowercase());
    }
    if let Some(description) = input.get("description").and_then(Value::as_str) {
        haystacks.push(description.to_ascii_lowercase());
    }
    if let Some(task) = input.get("task").and_then(Value::as_str) {
        haystacks.push(task.to_ascii_lowercase());
    }

    haystacks.into_iter().any(|value| {
        value.contains("cargo test")
            || value.contains("npm test")
            || value.contains("pnpm test")
            || value.contains("pytest")
            || value.contains("jest")
            || value.contains("vitest")
            || value.contains("go test")
            || value.contains("rspec")
            || value.contains("bun test")
            || value.contains(" test")
    })
}

fn mcp_server_name(name: &str, input: &Value) -> Option<String> {
    if let Some(rest) = name.strip_prefix("mcp__") {
        return rest.split("__").next().map(|value| value.to_string());
    }
    if name == "mcp" {
        return input
            .get("server")
            .and_then(Value::as_str)
            .map(sanitize_telemetry_label)
            .filter(|value| !value.is_empty());
    }
    None
}

fn post_payload(payload: serde_json::Value, timeout: Duration) -> bool {
    let client = match reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
    {
        Ok(client) => client,
        Err(_) => return false,
    };
    match client.post(TELEMETRY_ENDPOINT).json(&payload).send() {
        Ok(response) => response.error_for_status().is_ok(),
        Err(_) => false,
    }
}

fn send_payload(payload: serde_json::Value, mode: DeliveryMode) -> bool {
    match mode {
        DeliveryMode::Background => {
            std::thread::spawn(move || {
                let _ = post_payload(payload, ASYNC_SEND_TIMEOUT);
            });
            true
        }
        DeliveryMode::Blocking(timeout) => post_payload(payload, timeout),
    }
}

fn reset_counters() {
    ERROR_PROVIDER_TIMEOUT.store(0, Ordering::Relaxed);
    ERROR_AUTH_FAILED.store(0, Ordering::Relaxed);
    ERROR_TOOL_ERROR.store(0, Ordering::Relaxed);
    ERROR_MCP_ERROR.store(0, Ordering::Relaxed);
    ERROR_RATE_LIMITED.store(0, Ordering::Relaxed);
    PROVIDER_SWITCHES.store(0, Ordering::Relaxed);
    MODEL_SWITCHES.store(0, Ordering::Relaxed);
}

fn current_error_counts() -> ErrorCounts {
    ErrorCounts {
        provider_timeout: ERROR_PROVIDER_TIMEOUT.load(Ordering::Relaxed),
        auth_failed: ERROR_AUTH_FAILED.load(Ordering::Relaxed),
        tool_error: ERROR_TOOL_ERROR.load(Ordering::Relaxed),
        mcp_error: ERROR_MCP_ERROR.load(Ordering::Relaxed),
        rate_limited: ERROR_RATE_LIMITED.load(Ordering::Relaxed),
    }
}

fn sanitize_telemetry_label(value: &str) -> String {
    let mut cleaned = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if matches!(chars.peek(), Some('[')) {
                let _ = chars.next();
                while let Some(next) = chars.next() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
                continue;
            }
            continue;
        }
        if ch.is_control() {
            continue;
        }
        cleaned.push(ch);
    }
    cleaned.trim().to_string()
}

fn has_any_errors(errors: &ErrorCounts) -> bool {
    errors.provider_timeout > 0
        || errors.auth_failed > 0
        || errors.tool_error > 0
        || errors.mcp_error > 0
        || errors.rate_limited > 0
}

fn session_has_meaningful_activity(state: &SessionTelemetry, errors: &ErrorCounts) -> bool {
    state.had_user_prompt
        || state.had_assistant_response
        || state.assistant_responses > 0
        || state.tool_calls > 0
        || state.tool_failures > 0
        || state.executed_tool_calls > 0
        || state.feature_memory_used
        || state.feature_swarm_used
        || state.feature_web_used
        || state.feature_email_used
        || state.feature_mcp_used
        || state.feature_side_panel_used
        || state.feature_goal_used
        || state.feature_selfdev_used
        || state.feature_background_used
        || state.feature_subagent_used
        || PROVIDER_SWITCHES.load(Ordering::Relaxed) > 0
        || MODEL_SWITCHES.load(Ordering::Relaxed) > 0
        || has_any_errors(errors)
}

fn maybe_emit_session_start() {
    if !is_enabled() {
        return;
    }
    let event = {
        let mut guard = match SESSION_STATE.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let state = match guard.as_mut() {
            Some(state) => state,
            None => return,
        };
        if state.start_event_sent {
            return;
        }
        state.start_event_sent = true;
        SessionStartEvent {
            id: match get_or_create_id() {
                Some(id) => id,
                None => return,
            },
            event: "session_start",
            version: version(),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            provider_start: state.provider_start.clone(),
            model_start: state.model_start.clone(),
            resumed_session: state.resumed_session,
        }
    };
    if let Ok(payload) = serde_json::to_value(&event) {
        let _ = send_payload(payload, DeliveryMode::Background);
    }
}

fn emit_session_start_for_state(id: String, state: &SessionTelemetry, mode: DeliveryMode) -> bool {
    let event = SessionStartEvent {
        id,
        event: "session_start",
        version: version(),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        provider_start: state.provider_start.clone(),
        model_start: state.model_start.clone(),
        resumed_session: state.resumed_session,
    };
    if let Ok(payload) = serde_json::to_value(&event) {
        return send_payload(payload, mode);
    }
    false
}

pub fn record_install_if_first_run() {
    if !is_enabled() {
        return;
    }
    let first_run = is_first_run();
    let id = match get_or_create_id() {
        Some(id) => id,
        None => return,
    };
    if install_recorded_for_id(&id) {
        return;
    }
    let event = InstallEvent {
        id: id.clone(),
        event: "install",
        version: version(),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
    };
    if let Ok(payload) = serde_json::to_value(&event) {
        if send_payload(payload, DeliveryMode::Blocking(BLOCKING_INSTALL_TIMEOUT)) {
            mark_install_recorded(&id);
        }
    }
    if first_run {
        show_first_run_notice();
    }
    mark_current_version_recorded();
}

pub fn record_upgrade_if_needed() {
    if !is_enabled() {
        return;
    }
    let current = version();
    let Some(previous) = previously_recorded_version() else {
        mark_current_version_recorded();
        return;
    };
    if previous == current {
        return;
    }
    let Some(id) = get_or_create_id() else {
        return;
    };
    let event = UpgradeEvent {
        id,
        event: "upgrade",
        version: current,
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        from_version: previous,
    };
    if let Ok(payload) = serde_json::to_value(&event) {
        let _ = send_payload(payload, DeliveryMode::Background);
    }
    mark_current_version_recorded();
}

pub fn record_auth_success(provider: &str, method: &str) {
    if !is_enabled() {
        return;
    }
    let Some(id) = get_or_create_id() else {
        return;
    };
    let event = AuthEvent {
        id,
        event: "auth_success",
        version: version(),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        auth_provider: sanitize_telemetry_label(provider),
        auth_method: sanitize_telemetry_label(method),
    };
    if let Ok(payload) = serde_json::to_value(&event) {
        let _ = send_payload(payload, DeliveryMode::Background);
    }
}

pub fn begin_session(provider: &str, model: &str) {
    begin_session_with_mode(provider, model, false);
}

pub fn begin_resumed_session(provider: &str, model: &str) {
    begin_session_with_mode(provider, model, true);
}

fn begin_session_with_mode(provider: &str, model: &str, resumed_session: bool) {
    if !is_enabled() {
        return;
    }
    let state = SessionTelemetry {
        started_at: Instant::now(),
        provider_start: sanitize_telemetry_label(provider),
        model_start: sanitize_telemetry_label(model),
        turns: 0,
        had_user_prompt: false,
        had_assistant_response: false,
        assistant_responses: 0,
        first_assistant_response_ms: None,
        first_tool_call_ms: None,
        first_tool_success_ms: None,
        tool_calls: 0,
        tool_failures: 0,
        executed_tool_calls: 0,
        executed_tool_successes: 0,
        executed_tool_failures: 0,
        tool_latency_total_ms: 0,
        tool_latency_max_ms: 0,
        file_write_calls: 0,
        tests_run: 0,
        tests_passed: 0,
        feature_memory_used: false,
        feature_swarm_used: false,
        feature_web_used: false,
        feature_email_used: false,
        feature_mcp_used: false,
        feature_side_panel_used: false,
        feature_goal_used: false,
        feature_selfdev_used: false,
        feature_background_used: false,
        feature_subagent_used: false,
        unique_mcp_servers: HashSet::new(),
        transport_https: 0,
        transport_persistent_ws_fresh: 0,
        transport_persistent_ws_reuse: 0,
        transport_cli_subprocess: 0,
        transport_native_http2: 0,
        transport_other: 0,
        resumed_session,
        start_event_sent: false,
    };
    if let Ok(mut guard) = SESSION_STATE.lock() {
        *guard = Some(state);
    }
    reset_counters();
}

pub fn record_turn() {
    if let Ok(mut guard) = SESSION_STATE.lock() {
        if let Some(ref mut state) = *guard {
            state.turns += 1;
            state.had_user_prompt = true;
        }
    }
    maybe_emit_session_start();
}

pub fn record_assistant_response() {
    if let Ok(mut guard) = SESSION_STATE.lock() {
        if let Some(ref mut state) = *guard {
            if state.first_assistant_response_ms.is_none() {
                state.first_assistant_response_ms = Some(now_ms_since(state.started_at));
            }
            state.had_assistant_response = true;
            state.assistant_responses += 1;
        }
    }
    maybe_emit_session_start();
}

pub fn record_memory_injected(_count: usize, _age_ms: u64) {
    if let Ok(mut guard) = SESSION_STATE.lock() {
        if let Some(ref mut state) = *guard {
            state.feature_memory_used = true;
        }
    }
    maybe_emit_session_start();
}

pub fn record_tool_call() {
    if let Ok(mut guard) = SESSION_STATE.lock() {
        if let Some(ref mut state) = *guard {
            state.tool_calls += 1;
            if state.first_tool_call_ms.is_none() {
                state.first_tool_call_ms = Some(now_ms_since(state.started_at));
            }
        }
    }
    maybe_emit_session_start();
}

pub fn record_tool_failure() {
    if let Ok(mut guard) = SESSION_STATE.lock() {
        if let Some(ref mut state) = *guard {
            state.tool_failures += 1;
        }
    }
    maybe_emit_session_start();
}

pub fn record_connection_type(connection: &str) {
    if let Ok(mut guard) = SESSION_STATE.lock() {
        if let Some(ref mut state) = *guard {
            let normalized = sanitize_telemetry_label(connection).to_ascii_lowercase();
            if normalized.contains("websocket/persistent-reuse") {
                state.transport_persistent_ws_reuse += 1;
            } else if normalized.contains("websocket/persistent-fresh")
                || normalized.contains("websocket/persistent")
            {
                state.transport_persistent_ws_fresh += 1;
            } else if normalized.contains("native http2") {
                state.transport_native_http2 += 1;
            } else if normalized.contains("cli subprocess") {
                state.transport_cli_subprocess += 1;
            } else if normalized.starts_with("https") {
                state.transport_https += 1;
            } else {
                state.transport_other += 1;
            }
        }
    }
    maybe_emit_session_start();
}

pub fn record_error(category: ErrorCategory) {
    match category {
        ErrorCategory::ProviderTimeout => {
            ERROR_PROVIDER_TIMEOUT.fetch_add(1, Ordering::Relaxed);
        }
        ErrorCategory::AuthFailed => {
            ERROR_AUTH_FAILED.fetch_add(1, Ordering::Relaxed);
        }
        ErrorCategory::ToolError => {
            ERROR_TOOL_ERROR.fetch_add(1, Ordering::Relaxed);
        }
        ErrorCategory::McpError => {
            ERROR_MCP_ERROR.fetch_add(1, Ordering::Relaxed);
        }
        ErrorCategory::RateLimited => {
            ERROR_RATE_LIMITED.fetch_add(1, Ordering::Relaxed);
        }
    }
    maybe_emit_session_start();
}

pub fn record_provider_switch() {
    PROVIDER_SWITCHES.fetch_add(1, Ordering::Relaxed);
    maybe_emit_session_start();
}

pub fn record_model_switch() {
    MODEL_SWITCHES.fetch_add(1, Ordering::Relaxed);
    maybe_emit_session_start();
}

pub fn record_tool_execution(name: &str, input: &Value, succeeded: bool, latency_ms: u64) {
    if let Ok(mut guard) = SESSION_STATE.lock() {
        if let Some(ref mut state) = *guard {
            state.executed_tool_calls += 1;
            state.tool_latency_total_ms = state.tool_latency_total_ms.saturating_add(latency_ms);
            state.tool_latency_max_ms = state.tool_latency_max_ms.max(latency_ms);
            mark_tool_feature_usage(state, name, input);
            if succeeded {
                state.executed_tool_successes += 1;
                mark_tool_success_side_effects(state, name, input);
            } else {
                state.executed_tool_failures += 1;
            }
        }
    }
    maybe_emit_session_start();
}

pub fn end_session(provider_end: &str, model_end: &str) {
    end_session_with_reason(provider_end, model_end, SessionEndReason::NormalExit);
}

pub fn end_session_with_reason(provider_end: &str, model_end: &str, reason: SessionEndReason) {
    emit_lifecycle_event("session_end", provider_end, model_end, reason, true);
}

pub fn record_crash(provider_end: &str, model_end: &str, reason: SessionEndReason) {
    emit_lifecycle_event("session_crash", provider_end, model_end, reason, true);
}

pub fn current_provider_model() -> Option<(String, String)> {
    SESSION_STATE.lock().ok().and_then(|guard| {
        guard
            .as_ref()
            .map(|state| (state.provider_start.clone(), state.model_start.clone()))
    })
}

fn emit_lifecycle_event(
    event_name: &'static str,
    provider_end: &str,
    model_end: &str,
    reason: SessionEndReason,
    clear_state: bool,
) {
    if !is_enabled() {
        return;
    }
    let id = match get_or_create_id() {
        Some(id) => id,
        None => return,
    };
    let state = {
        let mut guard = match SESSION_STATE.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let state = match guard.as_ref() {
            Some(s) => SessionTelemetry {
                started_at: s.started_at,
                provider_start: s.provider_start.clone(),
                model_start: s.model_start.clone(),
                turns: s.turns,
                had_user_prompt: s.had_user_prompt,
                had_assistant_response: s.had_assistant_response,
                assistant_responses: s.assistant_responses,
                first_assistant_response_ms: s.first_assistant_response_ms,
                first_tool_call_ms: s.first_tool_call_ms,
                first_tool_success_ms: s.first_tool_success_ms,
                tool_calls: s.tool_calls,
                tool_failures: s.tool_failures,
                executed_tool_calls: s.executed_tool_calls,
                executed_tool_successes: s.executed_tool_successes,
                executed_tool_failures: s.executed_tool_failures,
                tool_latency_total_ms: s.tool_latency_total_ms,
                tool_latency_max_ms: s.tool_latency_max_ms,
                file_write_calls: s.file_write_calls,
                tests_run: s.tests_run,
                tests_passed: s.tests_passed,
                feature_memory_used: s.feature_memory_used,
                feature_swarm_used: s.feature_swarm_used,
                feature_web_used: s.feature_web_used,
                feature_email_used: s.feature_email_used,
                feature_mcp_used: s.feature_mcp_used,
                feature_side_panel_used: s.feature_side_panel_used,
                feature_goal_used: s.feature_goal_used,
                feature_selfdev_used: s.feature_selfdev_used,
                feature_background_used: s.feature_background_used,
                feature_subagent_used: s.feature_subagent_used,
                unique_mcp_servers: s.unique_mcp_servers.clone(),
                transport_https: s.transport_https,
                transport_persistent_ws_fresh: s.transport_persistent_ws_fresh,
                transport_persistent_ws_reuse: s.transport_persistent_ws_reuse,
                transport_cli_subprocess: s.transport_cli_subprocess,
                transport_native_http2: s.transport_native_http2,
                transport_other: s.transport_other,
                resumed_session: s.resumed_session,
                start_event_sent: s.start_event_sent,
            },
            None => return,
        };
        if clear_state {
            *guard = None;
        }
        state
    };
    let errors = current_error_counts();
    if !session_has_meaningful_activity(&state, &errors) {
        reset_counters();
        return;
    }
    if !state.start_event_sent {
        let _ = emit_session_start_for_state(
            id.clone(),
            &state,
            DeliveryMode::Blocking(BLOCKING_LIFECYCLE_TIMEOUT),
        );
    }
    let duration = state.started_at.elapsed();
    let session_success = state.had_assistant_response
        || state.executed_tool_successes > 0
        || state.tests_passed > 0
        || state.file_write_calls > 0;
    let abandoned_before_response = state.had_user_prompt
        && !state.had_assistant_response
        && state.executed_tool_successes == 0;
    let event = SessionLifecycleEvent {
        id,
        event: event_name,
        version: version(),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        provider_start: state.provider_start,
        provider_end: sanitize_telemetry_label(provider_end),
        model_start: state.model_start,
        model_end: sanitize_telemetry_label(model_end),
        provider_switches: PROVIDER_SWITCHES.load(Ordering::Relaxed),
        model_switches: MODEL_SWITCHES.load(Ordering::Relaxed),
        duration_mins: duration.as_secs() / 60,
        duration_secs: duration.as_secs(),
        turns: state.turns,
        had_user_prompt: state.had_user_prompt,
        had_assistant_response: state.had_assistant_response,
        assistant_responses: state.assistant_responses,
        first_assistant_response_ms: state.first_assistant_response_ms,
        first_tool_call_ms: state.first_tool_call_ms,
        first_tool_success_ms: state.first_tool_success_ms,
        tool_calls: state.tool_calls,
        tool_failures: state.tool_failures,
        executed_tool_calls: state.executed_tool_calls,
        executed_tool_successes: state.executed_tool_successes,
        executed_tool_failures: state.executed_tool_failures,
        tool_latency_total_ms: state.tool_latency_total_ms,
        tool_latency_max_ms: state.tool_latency_max_ms,
        file_write_calls: state.file_write_calls,
        tests_run: state.tests_run,
        tests_passed: state.tests_passed,
        feature_memory_used: state.feature_memory_used,
        feature_swarm_used: state.feature_swarm_used,
        feature_web_used: state.feature_web_used,
        feature_email_used: state.feature_email_used,
        feature_mcp_used: state.feature_mcp_used,
        feature_side_panel_used: state.feature_side_panel_used,
        feature_goal_used: state.feature_goal_used,
        feature_selfdev_used: state.feature_selfdev_used,
        feature_background_used: state.feature_background_used,
        feature_subagent_used: state.feature_subagent_used,
        unique_mcp_servers: state.unique_mcp_servers.len() as u32,
        session_success,
        abandoned_before_response,
        transport_https: state.transport_https,
        transport_persistent_ws_fresh: state.transport_persistent_ws_fresh,
        transport_persistent_ws_reuse: state.transport_persistent_ws_reuse,
        transport_cli_subprocess: state.transport_cli_subprocess,
        transport_native_http2: state.transport_native_http2,
        transport_other: state.transport_other,
        resumed_session: state.resumed_session,
        end_reason: reason.as_str(),
        errors,
    };
    if let Ok(payload) = serde_json::to_value(&event) {
        let _ = send_payload(payload, DeliveryMode::Blocking(BLOCKING_LIFECYCLE_TIMEOUT));
    }
    reset_counters();
}

#[derive(Debug, Clone, Copy)]
pub enum ErrorCategory {
    ProviderTimeout,
    AuthFailed,
    ToolError,
    McpError,
    RateLimited,
}

fn show_first_run_notice() {
    eprintln!("\x1b[90m");
    eprintln!("  jcode collects anonymous usage statistics (install count, version, OS,");
    eprintln!("  session activity, tool counts, and crash/exit reasons). No code, filenames,");
    eprintln!("  prompts, or personal data is sent.");
    eprintln!("  To opt out: export JCODE_NO_TELEMETRY=1");
    eprintln!("  Details: https://github.com/1jehuang/jcode/blob/master/TELEMETRY.md");
    eprintln!("\x1b[0m");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::lock_test_env;
    use std::sync::{Mutex, OnceLock};

    fn lock_telemetry_test_state() -> std::sync::MutexGuard<'static, ()> {
        static TELEMETRY_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        TELEMETRY_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn test_opt_out_env_var() {
        let _guard = lock_test_env();
        crate::env::set_var("JCODE_NO_TELEMETRY", "1");
        assert!(!is_enabled());
        crate::env::remove_var("JCODE_NO_TELEMETRY");
    }

    #[test]
    fn test_do_not_track() {
        let _guard = lock_test_env();
        crate::env::set_var("DO_NOT_TRACK", "1");
        assert!(!is_enabled());
        crate::env::remove_var("DO_NOT_TRACK");
    }

    #[test]
    fn test_error_counters() {
        let _guard = lock_telemetry_test_state();
        reset_counters();
        record_error(ErrorCategory::ProviderTimeout);
        record_error(ErrorCategory::ProviderTimeout);
        record_error(ErrorCategory::ToolError);
        assert_eq!(ERROR_PROVIDER_TIMEOUT.load(Ordering::Relaxed), 2);
        assert_eq!(ERROR_TOOL_ERROR.load(Ordering::Relaxed), 1);
        reset_counters();
    }

    #[test]
    fn test_session_reason_labels() {
        assert_eq!(SessionEndReason::NormalExit.as_str(), "normal_exit");
        assert_eq!(SessionEndReason::Disconnect.as_str(), "disconnect");
    }

    #[test]
    fn test_session_start_event_serialization() {
        let event = SessionStartEvent {
            id: "test-uuid".to_string(),
            event: "session_start",
            version: "0.6.1".to_string(),
            os: "linux",
            arch: "x86_64",
            provider_start: "claude".to_string(),
            model_start: "claude-sonnet-4".to_string(),
            resumed_session: true,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["event"], "session_start");
        assert_eq!(json["resumed_session"], true);
    }

    #[test]
    fn test_session_end_event_serialization() {
        let event = SessionLifecycleEvent {
            id: "test-uuid".to_string(),
            event: "session_end",
            version: "0.6.1".to_string(),
            os: "linux",
            arch: "x86_64",
            provider_start: "claude".to_string(),
            provider_end: "openrouter".to_string(),
            model_start: "claude-sonnet-4-20250514".to_string(),
            model_end: "anthropic/claude-sonnet-4".to_string(),
            provider_switches: 1,
            model_switches: 2,
            duration_mins: 45,
            duration_secs: 2700,
            turns: 23,
            had_user_prompt: true,
            had_assistant_response: true,
            assistant_responses: 3,
            first_assistant_response_ms: Some(1200),
            first_tool_call_ms: Some(900),
            first_tool_success_ms: Some(1500),
            tool_calls: 4,
            tool_failures: 1,
            executed_tool_calls: 5,
            executed_tool_successes: 4,
            executed_tool_failures: 1,
            tool_latency_total_ms: 3200,
            tool_latency_max_ms: 1400,
            file_write_calls: 2,
            tests_run: 1,
            tests_passed: 1,
            feature_memory_used: true,
            feature_swarm_used: false,
            feature_web_used: true,
            feature_email_used: false,
            feature_mcp_used: true,
            feature_side_panel_used: true,
            feature_goal_used: false,
            feature_selfdev_used: false,
            feature_background_used: false,
            feature_subagent_used: true,
            unique_mcp_servers: 2,
            session_success: true,
            abandoned_before_response: false,
            transport_https: 2,
            transport_persistent_ws_fresh: 1,
            transport_persistent_ws_reuse: 5,
            transport_cli_subprocess: 0,
            transport_native_http2: 0,
            transport_other: 0,
            resumed_session: false,
            end_reason: "normal_exit",
            errors: ErrorCounts {
                provider_timeout: 2,
                auth_failed: 0,
                tool_error: 1,
                mcp_error: 0,
                rate_limited: 0,
            },
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["event"], "session_end");
        assert_eq!(json["assistant_responses"], 3);
        assert_eq!(json["duration_secs"], 2700);
        assert_eq!(json["executed_tool_calls"], 5);
        assert_eq!(json["transport_https"], 2);
        assert_eq!(json["transport_persistent_ws_reuse"], 5);
        assert_eq!(json["end_reason"], "normal_exit");
        assert_eq!(json["errors"]["provider_timeout"], 2);
    }

    #[test]
    fn test_record_connection_type_buckets_transport() {
        let _guard = lock_telemetry_test_state();
        reset_counters();
        if let Ok(mut session) = SESSION_STATE.lock() {
            *session = None;
        }
        begin_session_with_mode("openai", "gpt-5.4", false);
        record_connection_type("websocket/persistent-fresh");
        record_connection_type("websocket/persistent-reuse");
        record_connection_type("https/sse");
        record_connection_type("native http2");
        record_connection_type("cli subprocess");
        record_connection_type("weird-transport");

        let guard = SESSION_STATE.lock().unwrap();
        let state = guard.as_ref().expect("session telemetry state");
        assert_eq!(state.transport_persistent_ws_fresh, 1);
        assert_eq!(state.transport_persistent_ws_reuse, 1);
        assert_eq!(state.transport_https, 1);
        assert_eq!(state.transport_native_http2, 1);
        assert_eq!(state.transport_cli_subprocess, 1);
        assert_eq!(state.transport_other, 1);
        if let Ok(mut session) = SESSION_STATE.lock() {
            *session = None;
        }
        reset_counters();
    }

    #[test]
    fn test_sanitize_telemetry_label_strips_ansi_and_controls() {
        assert_eq!(
            sanitize_telemetry_label("\u{1b}[1mclaude-opus-4-6\u{1b}[0m\n"),
            "claude-opus-4-6"
        );
    }

    #[test]
    fn test_install_marker_tracks_current_telemetry_id() {
        let _guard = lock_test_env();
        let prev_home = std::env::var_os("JCODE_HOME");
        let temp = tempfile::TempDir::new().expect("create temp dir");
        crate::env::set_var("JCODE_HOME", temp.path());

        assert!(!install_recorded_for_id("id-a"));
        mark_install_recorded("id-a");
        assert!(install_recorded_for_id("id-a"));
        assert!(!install_recorded_for_id("id-b"));

        if let Some(prev_home) = prev_home {
            crate::env::set_var("JCODE_HOME", prev_home);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
    }
}
