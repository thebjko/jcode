#![allow(dead_code)]

mod debug;
mod headless;

use self::debug::{handle_debug_client, ClientConnectionInfo, ClientDebugState, DebugJob};
use self::headless::create_headless_session;
use crate::agent::{Agent, StreamError};
use crate::ambient_runner::AmbientRunnerHandle;
use crate::build;
use crate::bus::{Bus, BusEvent, FileOp};
use crate::id;
use crate::plan::PlanItem;
use crate::protocol::{
    decode_request, encode_event, AgentInfo, ContextEntry, FeatureToggle, HistoryMessage,
    NotificationType, Request, ServerEvent,
};
use crate::provider::Provider;
use crate::session::Session;
use crate::tool::Registry;
use crate::transport::{Listener, ReadHalf, Stream, WriteHalf};
use anyhow::Result;
use futures::future::try_join_all;
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};

/// Record of a file access by an agent
#[derive(Clone, Debug)]
pub struct FileAccess {
    pub session_id: String,
    pub op: FileOp,
    pub timestamp: Instant,
    pub absolute_time: std::time::SystemTime,
    pub summary: Option<String>,
}

/// Information about a session in a swarm
#[derive(Clone, Debug)]
pub struct SwarmMember {
    pub session_id: String,
    /// Channel to send events to this session
    pub event_tx: mpsc::UnboundedSender<ServerEvent>,
    /// Working directory (used to derive swarm id)
    pub working_dir: Option<PathBuf>,
    /// Swarm identifier (shared across worktrees)
    pub swarm_id: Option<String>,
    /// Whether swarm coordination is enabled for this member
    pub swarm_enabled: bool,
    /// Lifecycle status (ready, running, completed, failed, stopped, etc.)
    pub status: String,
    /// Optional detail (current task, error, etc.)
    pub detail: Option<String>,
    /// Friendly name like "fox"
    pub friendly_name: Option<String>,
    /// Role: "agent", "coordinator", "worktree_manager"
    pub role: String,
    /// When this member joined the swarm
    pub joined_at: Instant,
    /// When status was last changed
    pub last_status_change: Instant,
}

/// A versioned plan for a swarm.
#[derive(Clone, Debug)]
pub struct VersionedPlan {
    pub items: Vec<PlanItem>,
    pub version: u64,
    /// Session ids that should receive this plan's updates.
    pub participants: HashSet<String>,
}

impl VersionedPlan {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            version: 0,
            participants: HashSet::new(),
        }
    }
}

/// A shared context entry stored by the server
#[derive(Clone, Debug)]
pub struct SharedContext {
    pub key: String,
    pub value: String,
    pub from_session: String,
    pub from_name: Option<String>,
    /// When this context was created
    pub created_at: Instant,
    /// When this context was last updated
    pub updated_at: Instant,
}

/// Event types for real-time event subscription
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SwarmEventType {
    /// A file was touched (read/write/edit)
    FileTouch {
        path: String,
        op: String,
        summary: Option<String>,
    },
    /// A notification was broadcast
    Notification {
        notification_type: String,
        message: String,
    },
    /// A swarm plan was updated
    PlanUpdate { swarm_id: String, item_count: usize },
    /// A plan proposal was submitted
    PlanProposal {
        swarm_id: String,
        proposer_session: String,
        item_count: usize,
    },
    /// Shared context was updated
    ContextUpdate { swarm_id: String, key: String },
    /// Session status changed
    StatusChange {
        old_status: String,
        new_status: String,
    },
    /// Session joined/left swarm
    MemberChange {
        action: String, // "joined" or "left"
    },
}

/// A swarm event with metadata
#[derive(Clone, Debug)]
pub struct SwarmEvent {
    pub id: u64,
    pub session_id: String,
    pub session_name: Option<String>,
    pub swarm_id: Option<String>,
    pub event: SwarmEventType,
    pub timestamp: Instant,
    pub absolute_time: std::time::SystemTime,
}

/// Ring buffer for recent swarm events
const MAX_EVENT_HISTORY: usize = 5000;

/// Socket path for main communication
/// Can be overridden via JCODE_SOCKET env var
pub fn socket_path() -> PathBuf {
    if let Ok(custom) = std::env::var("JCODE_SOCKET") {
        return PathBuf::from(custom);
    }
    crate::storage::runtime_dir().join("jcode.sock")
}

/// Debug socket path for testing/introspection
/// Derived from main socket path
pub fn debug_socket_path() -> PathBuf {
    let main_path = socket_path();
    let filename = main_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("jcode.sock");
    let debug_filename = filename.replace(".sock", "-debug.sock");
    main_path.with_file_name(debug_filename)
}

fn sibling_socket_path(path: &std::path::Path) -> Option<PathBuf> {
    let filename = path.file_name()?.to_str()?;

    if let Some(base) = filename.strip_suffix("-debug.sock") {
        return Some(path.with_file_name(format!("{}.sock", base)));
    }

    if let Some(base) = filename.strip_suffix(".sock") {
        return Some(path.with_file_name(format!("{}-debug.sock", base)));
    }

    None
}

/// Remove a socket file and its sibling (main/debug) if present.
pub fn cleanup_socket_pair(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    if let Some(sibling) = sibling_socket_path(path) {
        let _ = std::fs::remove_file(sibling);
    }
}

/// Connect to a Unix socket, cleaning up stale socket files on connection-refused.
pub async fn connect_socket(path: &std::path::Path) -> Result<Stream> {
    match Stream::connect(path).await {
        Ok(stream) => Ok(stream),
        Err(err) => {
            let is_stale = err.kind() == std::io::ErrorKind::ConnectionRefused && path.exists();
            if is_stale {
                cleanup_socket_pair(path);
                anyhow::bail!(
                    "Stale socket removed at {}. Start/restart jcode and retry.",
                    path.display()
                );
            }
            Err(err.into())
        }
    }
}

#[cfg(test)]
mod socket_tests {
    use super::{cleanup_socket_pair, sibling_socket_path};

    #[test]
    fn sibling_socket_path_roundtrip() {
        let main = std::path::PathBuf::from("/tmp/jcode.sock");
        let debug = std::path::PathBuf::from("/tmp/jcode-debug.sock");

        assert_eq!(sibling_socket_path(&main), Some(debug.clone()));
        assert_eq!(sibling_socket_path(&debug), Some(main));
    }

    #[test]
    fn cleanup_socket_pair_removes_main_and_debug_files() {
        let stamp = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let dir = std::env::temp_dir();
        let main = dir.join(format!("jcode-test-{}.sock", stamp));
        let debug = dir.join(format!("jcode-test-{}-debug.sock", stamp));

        std::fs::write(&main, b"").expect("create main socket placeholder");
        std::fs::write(&debug, b"").expect("create debug socket placeholder");

        cleanup_socket_pair(&main);

        assert!(!main.exists(), "main socket file should be removed");
        assert!(!debug.exists(), "debug socket file should be removed");
    }
}

/// Set custom socket path (sets JCODE_SOCKET env var)
pub fn set_socket_path(path: &str) {
    std::env::set_var("JCODE_SOCKET", path);
}

/// Idle timeout for self-dev server (5 minutes)
const IDLE_TIMEOUT_SECS: u64 = 300;

/// How often to check whether the embedding model can be unloaded.
const EMBEDDING_IDLE_CHECK_SECS: u64 = 30;

/// Default embedding idle unload threshold (15 minutes).
const EMBEDDING_IDLE_UNLOAD_DEFAULT_SECS: u64 = 15 * 60;

/// Self-dev socket path (used for detection when env var isn't set)
const SELFDEV_SOCKET: &str = "/tmp/jcode-selfdev.sock";

fn is_selfdev_env() -> bool {
    if std::env::var("JCODE_SELFDEV_MODE").is_ok() {
        return true;
    }
    if std::env::var("JCODE_SOCKET").ok().as_deref() == Some(SELFDEV_SOCKET) {
        return true;
    }
    std::env::current_dir()
        .ok()
        .map(|p| crate::build::is_jcode_repo(&p))
        .unwrap_or(false)
}

/// Reload signal payload sent via in-process channel (replaces filesystem-based rebuild-signal)
#[derive(Clone, Debug)]
pub struct ReloadSignal {
    pub hash: String,
    pub triggering_session: Option<String>,
}

/// Global reload signal channel. The selfdev tool and debug commands fire this;
/// the server awaits it instead of polling the filesystem.
static RELOAD_SIGNAL: std::sync::OnceLock<(
    tokio::sync::watch::Sender<Option<ReloadSignal>>,
    tokio::sync::watch::Receiver<Option<ReloadSignal>>,
)> = std::sync::OnceLock::new();

fn reload_signal() -> &'static (
    tokio::sync::watch::Sender<Option<ReloadSignal>>,
    tokio::sync::watch::Receiver<Option<ReloadSignal>>,
) {
    RELOAD_SIGNAL.get_or_init(|| tokio::sync::watch::channel(None))
}

/// Send a reload signal to the server (called by selfdev tool / debug commands).
pub fn send_reload_signal(hash: String, triggering_session: Option<String>) {
    let (tx, _) = reload_signal();
    let _ = tx.send(Some(ReloadSignal {
        hash,
        triggering_session,
    }));
}

fn is_jcode_repo_or_parent(path: &std::path::Path) -> bool {
    let mut current = Some(path);
    while let Some(dir) = current {
        if crate::build::is_jcode_repo(dir) {
            return true;
        }
        current = dir.parent();
    }
    false
}

fn debug_control_allowed() -> bool {
    if is_selfdev_env() {
        return true;
    }
    // Check config file setting
    if crate::config::config().display.debug_socket {
        return true;
    }
    if std::env::var("JCODE_DEBUG_CONTROL")
        .ok()
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
    {
        return true;
    }
    // Check for file-based toggle (allows enabling without restart)
    if let Ok(jcode_dir) = crate::storage::jcode_dir() {
        if jcode_dir.join("debug_control").exists() {
            return true;
        }
    }
    false
}

fn embedding_idle_unload_secs() -> u64 {
    std::env::var("JCODE_EMBEDDING_IDLE_UNLOAD_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(EMBEDDING_IDLE_UNLOAD_DEFAULT_SECS)
}

fn server_update_candidate() -> Option<(PathBuf, &'static str)> {
    build::client_update_candidate(is_selfdev_env())
}

fn canonicalize_or(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn git_common_dir_for(path: &std::path::Path) -> Option<PathBuf> {
    let mut current = Some(path);
    while let Some(dir) = current {
        let dotgit = dir.join(".git");
        if dotgit.is_dir() {
            return Some(canonicalize_or(dotgit));
        }
        if dotgit.is_file() {
            let content = std::fs::read_to_string(&dotgit).ok()?;
            let gitdir_line = content
                .lines()
                .find(|line| line.trim_start().starts_with("gitdir:"))?;
            let raw = gitdir_line
                .trim_start()
                .trim_start_matches("gitdir:")
                .trim();
            if raw.is_empty() {
                return None;
            }
            let gitdir = if std::path::Path::new(raw).is_absolute() {
                PathBuf::from(raw)
            } else {
                dir.join(raw)
            };
            let gitdir = canonicalize_or(gitdir);
            // Worktree gitdir looks like: <repo>/.git/worktrees/<name>
            if let Some(parent) = gitdir.parent() {
                if parent.file_name().and_then(|s| s.to_str()) == Some("worktrees") {
                    if let Some(common) = parent.parent() {
                        return Some(canonicalize_or(common.to_path_buf()));
                    }
                }
            }
            return Some(gitdir);
        }
        current = dir.parent();
    }
    None
}

fn swarm_id_for_dir(dir: Option<PathBuf>) -> Option<String> {
    if let Ok(sw_id) = std::env::var("JCODE_SWARM_ID") {
        let trimmed = sw_id.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let dir = dir?;
    if let Some(git_common) = git_common_dir_for(&dir) {
        return Some(git_common.to_string_lossy().to_string());
    }
    Some(dir.to_string_lossy().to_string())
}

fn server_has_newer_binary() -> bool {
    let current_exe = std::env::current_exe().ok();
    let Some((candidate, _label)) = server_update_candidate() else {
        return false;
    };

    let current_mtime = current_exe
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok());
    let candidate_mtime = std::fs::metadata(&candidate)
        .ok()
        .and_then(|m| m.modified().ok());

    match (current_mtime, candidate_mtime) {
        (Some(current), Some(candidate)) => candidate > current,
        _ => {
            if let Some(current_exe) = current_exe {
                let current = canonicalize_or(current_exe);
                let candidate_path = canonicalize_or(candidate);
                current != candidate_path
            } else {
                false
            }
        }
    }
}

/// Exit code when server shuts down due to idle timeout
pub const EXIT_IDLE_TIMEOUT: i32 = 44;

/// Server identity for multi-server support
#[derive(Debug, Clone)]
pub struct ServerIdentity {
    /// Full server ID (e.g., "server_blazing_1705012345678")
    pub id: String,
    /// Short name (e.g., "blazing")
    pub name: String,
    /// Icon for display (e.g., "🔥")
    pub icon: String,
    /// Git hash of the binary
    pub git_hash: String,
    /// Version string (e.g., "v0.1.123")
    pub version: String,
}

impl ServerIdentity {
    /// Display name with icon (e.g., "🔥 blazing")
    pub fn display_name(&self) -> String {
        format!("{} {}", self.icon, self.name)
    }
}

/// Server state
pub struct Server {
    provider: Arc<dyn Provider>,
    socket_path: PathBuf,
    debug_socket_path: PathBuf,
    /// Server identity for multi-server support
    identity: ServerIdentity,
    /// Broadcast channel for streaming events to all subscribers
    event_tx: broadcast::Sender<ServerEvent>,
    /// Active sessions (session_id -> Agent)
    sessions: Arc<RwLock<HashMap<String, Arc<Mutex<Agent>>>>>,
    /// Current processing state
    is_processing: Arc<RwLock<bool>>,
    /// Session ID for the default session
    session_id: Arc<RwLock<String>>,
    /// Number of connected clients
    client_count: Arc<RwLock<usize>>,
    /// Connected client mapping (client_id -> session_id)
    client_connections: Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
    /// Track file touches: path -> list of accesses
    file_touches: Arc<RwLock<HashMap<PathBuf, Vec<FileAccess>>>>,
    /// Swarm members: session_id -> SwarmMember info
    swarm_members: Arc<RwLock<HashMap<String, SwarmMember>>>,
    /// Swarm groupings by swarm id -> set of session_ids
    swarms_by_id: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    /// Shared context by swarm (swarm_id -> key -> SharedContext)
    shared_context: Arc<RwLock<HashMap<String, HashMap<String, SharedContext>>>>,
    /// Shared plans by swarm (swarm_id -> plan)
    swarm_plans: Arc<RwLock<HashMap<String, VersionedPlan>>>,
    /// Coordinator per swarm (swarm_id -> session_id)
    swarm_coordinators: Arc<RwLock<HashMap<String, String>>>,
    /// Active and available TUI debug channels (request_id, command)
    client_debug_state: Arc<RwLock<ClientDebugState>>,
    /// Channel to receive client debug responses from TUI (request_id, response)
    client_debug_response_tx: broadcast::Sender<(u64, String)>,
    /// Background debug jobs (async debug commands)
    debug_jobs: Arc<RwLock<HashMap<String, DebugJob>>>,
    /// Channel subscriptions (swarm_id -> channel -> session_ids)
    channel_subscriptions: Arc<RwLock<HashMap<String, HashMap<String, HashSet<String>>>>>,
    /// Event history for real-time event subscription (ring buffer)
    event_history: Arc<RwLock<Vec<SwarmEvent>>>,
    /// Counter for event IDs
    event_counter: Arc<std::sync::atomic::AtomicU64>,
    /// Broadcast channel for swarm event subscriptions (debug socket subscribers)
    swarm_event_tx: broadcast::Sender<SwarmEvent>,
    /// Ambient mode runner handle (None if ambient is disabled)
    ambient_runner: Option<AmbientRunnerHandle>,
    /// Shared MCP server pool (processes shared across sessions)
    mcp_pool: Arc<crate::mcp::SharedMcpPool>,
    /// Graceful shutdown signals by session_id (stored outside agent mutex so they
    /// can be signaled without locking the agent during active tool execution)
    shutdown_signals: Arc<RwLock<HashMap<String, crate::agent::InterruptSignal>>>,
}

impl Server {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        use crate::id::{new_memorable_server_id, server_icon};

        let (event_tx, _) = broadcast::channel(1024);
        let (client_debug_response_tx, _) = broadcast::channel(64);

        // Generate a memorable server name
        let (id, name) = new_memorable_server_id();
        let icon = server_icon(&name).to_string();
        let identity = ServerIdentity {
            id,
            name,
            icon,
            git_hash: env!("JCODE_GIT_HASH").to_string(),
            version: env!("JCODE_VERSION").to_string(),
        };

        // Initialize ambient runner if enabled
        let ambient_runner = if crate::config::config().ambient.enabled {
            let safety = Arc::new(crate::safety::SafetySystem::new());
            Some(AmbientRunnerHandle::new(safety))
        } else {
            None
        };

        Self {
            provider,
            socket_path: socket_path(),
            debug_socket_path: debug_socket_path(),
            identity,
            event_tx,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            is_processing: Arc::new(RwLock::new(false)),
            session_id: Arc::new(RwLock::new(String::new())),
            client_count: Arc::new(RwLock::new(0)),
            client_connections: Arc::new(RwLock::new(HashMap::new())),
            file_touches: Arc::new(RwLock::new(HashMap::new())),
            swarm_members: Arc::new(RwLock::new(HashMap::new())),
            swarms_by_id: Arc::new(RwLock::new(HashMap::new())),
            shared_context: Arc::new(RwLock::new(HashMap::new())),
            swarm_plans: Arc::new(RwLock::new(HashMap::new())),
            swarm_coordinators: Arc::new(RwLock::new(HashMap::new())),
            client_debug_state: Arc::new(RwLock::new(ClientDebugState::default())),
            client_debug_response_tx,
            debug_jobs: Arc::new(RwLock::new(HashMap::new())),
            channel_subscriptions: Arc::new(RwLock::new(HashMap::new())),
            event_history: Arc::new(RwLock::new(Vec::new())),
            event_counter: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            swarm_event_tx: broadcast::channel(256).0,
            ambient_runner,
            mcp_pool: Arc::new(crate::mcp::SharedMcpPool::from_default_config()),
            shutdown_signals: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn new_with_paths(
        provider: Arc<dyn Provider>,
        socket_path: PathBuf,
        debug_socket_path: PathBuf,
    ) -> Self {
        let mut server = Self::new(provider);
        server.socket_path = socket_path;
        server.debug_socket_path = debug_socket_path;
        server
    }

    /// Get the server identity
    pub fn identity(&self) -> &ServerIdentity {
        &self.identity
    }

    /// Monitor the global Bus for FileTouch events and detect conflicts
    async fn monitor_bus(
        file_touches: Arc<RwLock<HashMap<PathBuf, Vec<FileAccess>>>>,
        swarm_members: Arc<RwLock<HashMap<String, SwarmMember>>>,
        swarms_by_id: Arc<RwLock<HashMap<String, HashSet<String>>>>,
        _swarm_plans: Arc<RwLock<HashMap<String, VersionedPlan>>>,
        _swarm_coordinators: Arc<RwLock<HashMap<String, String>>>,
        _shared_context: Arc<RwLock<HashMap<String, HashMap<String, SharedContext>>>>,
        sessions: Arc<RwLock<HashMap<String, Arc<Mutex<Agent>>>>>,
        event_history: Arc<RwLock<Vec<SwarmEvent>>>,
        event_counter: Arc<std::sync::atomic::AtomicU64>,
        swarm_event_tx: broadcast::Sender<SwarmEvent>,
    ) {
        let mut receiver = Bus::global().subscribe();
        let mut last_cleanup = Instant::now();
        const TOUCH_EXPIRY: Duration = Duration::from_secs(30 * 60); // 30 min
        const CLEANUP_INTERVAL: Duration = Duration::from_secs(5 * 60); // 5 min

        loop {
            // Periodic cleanup of expired file touches
            if last_cleanup.elapsed() > CLEANUP_INTERVAL {
                let mut touches = file_touches.write().await;
                let now = Instant::now();
                touches.retain(|_, accesses| {
                    accesses.retain(|a| now.duration_since(a.timestamp) < TOUCH_EXPIRY);
                    !accesses.is_empty()
                });
                last_cleanup = Instant::now();
            }

            match receiver.recv().await {
                Ok(BusEvent::FileTouch(touch)) => {
                    let path = touch.path.clone();
                    let session_id = touch.session_id.clone();

                    // Record this touch
                    {
                        let mut touches = file_touches.write().await;
                        let accesses = touches.entry(path.clone()).or_insert_with(Vec::new);
                        accesses.push(FileAccess {
                            session_id: session_id.clone(),
                            op: touch.op.clone(),
                            timestamp: Instant::now(),
                            absolute_time: std::time::SystemTime::now(),
                            summary: touch.summary.clone(),
                        });
                    }

                    // Record event for subscription
                    {
                        let members = swarm_members.read().await;
                        let member = members.get(&session_id);
                        let session_name = member.and_then(|m| m.friendly_name.clone());
                        let swarm_id = member.and_then(|m| m.swarm_id.clone());

                        let event = SwarmEvent {
                            id: event_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                            session_id: session_id.clone(),
                            session_name,
                            swarm_id,
                            event: SwarmEventType::FileTouch {
                                path: path.to_string_lossy().to_string(),
                                op: touch.op.as_str().to_string(),
                                summary: touch.summary.clone(),
                            },
                            timestamp: Instant::now(),
                            absolute_time: std::time::SystemTime::now(),
                        };

                        let mut history = event_history.write().await;
                        history.push(event.clone());
                        if history.len() > MAX_EVENT_HISTORY {
                            history.remove(0);
                        }
                        let _ = swarm_event_tx.send(event);
                    }

                    // Find the swarm this session belongs to
                    let swarm_session_ids: Vec<String> = {
                        let members = swarm_members.read().await;
                        if let Some(member) = members.get(&session_id) {
                            if let Some(ref swarm_id) = member.swarm_id {
                                let swarms = swarms_by_id.read().await;
                                if let Some(swarm) = swarms.get(swarm_id) {
                                    swarm.iter().cloned().collect()
                                } else {
                                    vec![]
                                }
                            } else {
                                vec![]
                            }
                        } else {
                            vec![]
                        }
                    };

                    // Only check for conflicts when someone writes/edits (reads never conflict)
                    let is_write = matches!(touch.op, FileOp::Write | FileOp::Edit);
                    if is_write {
                        crate::logging::info(&format!(
                            "[conflict-check] WRITE by {} on {}, swarm_peers: {:?}",
                            &session_id[..8.min(session_id.len())],
                            path.display(),
                            swarm_session_ids
                                .iter()
                                .map(|s| &s[..8.min(s.len())])
                                .collect::<Vec<_>>()
                        ));
                    }
                    let previous_touches: Vec<FileAccess> = if is_write {
                        let touches = file_touches.read().await;
                        if let Some(accesses) = touches.get(&path) {
                            let result: Vec<FileAccess> = accesses
                                .iter()
                                .filter(|a| {
                                    a.session_id != session_id
                                        && swarm_session_ids.contains(&a.session_id)
                                        && matches!(a.op, FileOp::Write | FileOp::Edit)
                                })
                                .cloned()
                                .collect();
                            crate::logging::info(&format!(
                                "[conflict-check] {} prev write-touches from peers ({} total accesses)",
                                result.len(), accesses.len()
                            ));
                            result
                        } else {
                            crate::logging::info("[conflict-check] no touches for this path yet");
                            vec![]
                        }
                    } else {
                        vec![]
                    };

                    // If there are previous write conflicts from swarm members, send alerts
                    if !previous_touches.is_empty() {
                        crate::logging::info(&format!(
                            "[conflict-check] CONFLICT on {} — sending alerts",
                            path.display()
                        ));
                        let members = swarm_members.read().await;
                        let current_member = members.get(&session_id);
                        let current_name = current_member.and_then(|m| m.friendly_name.clone());
                        let agent_sessions = sessions.read().await;

                        // Deduplicate previous touches by session (keep latest per agent)
                        let mut unique_by_session: std::collections::HashMap<&str, &FileAccess> =
                            std::collections::HashMap::new();
                        for prev in &previous_touches {
                            unique_by_session
                                .entry(&prev.session_id)
                                .and_modify(|existing| {
                                    if prev.timestamp > existing.timestamp {
                                        *existing = prev;
                                    }
                                })
                                .or_insert(prev);
                        }

                        // Alert the current agent about previous writers (one per agent)
                        if let Some(member) = current_member {
                            for prev in unique_by_session.values() {
                                let prev_member = members.get(&prev.session_id);
                                let prev_name = prev_member.and_then(|m| m.friendly_name.clone());
                                let alert_msg = format!(
                                    "⚠️ File conflict: {} — {} previously {} this file{}",
                                    path.display(),
                                    prev_name.as_deref().unwrap_or(&prev.session_id[..8]),
                                    prev.op.as_str(),
                                    prev.summary
                                        .as_ref()
                                        .map(|s| format!(": {}", s))
                                        .unwrap_or_default()
                                );
                                let notification = ServerEvent::Notification {
                                    from_session: prev.session_id.clone(),
                                    from_name: prev_name,
                                    notification_type: NotificationType::FileConflict {
                                        path: path.display().to_string(),
                                        operation: prev.op.as_str().to_string(),
                                    },
                                    message: alert_msg.clone(),
                                };
                                let _ = member.event_tx.send(notification);

                                if let Some(agent) = agent_sessions.get(&session_id) {
                                    if let Ok(agent) = agent.try_lock() {
                                        agent.queue_soft_interrupt(alert_msg.clone(), false);
                                    }
                                }
                            }
                        }

                        // Alert previous agents about the current touch (one per agent)
                        let mut notified_sessions: std::collections::HashSet<&str> =
                            std::collections::HashSet::new();
                        for prev in &previous_touches {
                            if !notified_sessions.insert(&prev.session_id) {
                                continue;
                            }
                            if let Some(prev_member) = members.get(&prev.session_id) {
                                let alert_msg = format!(
                                    "⚠️ File conflict: {} — {} just {} this file you previously worked with{}",
                                    path.display(),
                                    current_name.as_deref().unwrap_or(&session_id[..8.min(session_id.len())]),
                                    touch.op.as_str(),
                                    touch
                                        .summary
                                        .as_ref()
                                        .map(|s| format!(": {}", s))
                                        .unwrap_or_default()
                                );
                                let notification = ServerEvent::Notification {
                                    from_session: session_id.clone(),
                                    from_name: current_name.clone(),
                                    notification_type: NotificationType::FileConflict {
                                        path: path.display().to_string(),
                                        operation: touch.op.as_str().to_string(),
                                    },
                                    message: alert_msg.clone(),
                                };
                                let _ = prev_member.event_tx.send(notification);

                                if let Some(agent) = agent_sessions.get(&prev.session_id) {
                                    if let Ok(agent) = agent.try_lock() {
                                        agent.queue_soft_interrupt(alert_msg.clone(), false);
                                    }
                                }
                            }
                        }
                    }
                }
                // Session todos are private. Swarm plans are updated via explicit
                // communication actions (comm_propose_plan / comm_approve_plan), not
                // todowrite broadcasts.
                Ok(BusEvent::TodoUpdated(_)) => {}
                Ok(_) => {
                    // Ignore other events
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    crate::logging::info(&format!("Bus monitor lagged by {} events", n));
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    }

    /// Start the server (both main and debug sockets)
    pub async fn run(&self) -> Result<()> {
        // Ensure socket directory exists (for named sockets like /run/user/1000/jcode/)
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Remove existing sockets (uses transport abstraction for cross-platform cleanup)
        crate::transport::remove_socket(&self.socket_path);
        crate::transport::remove_socket(&self.debug_socket_path);

        let mut main_listener = Listener::bind(&self.socket_path)?;
        let mut debug_listener = Listener::bind(&self.debug_socket_path)?;

        // Restrict socket files to owner-only so other local users cannot connect.
        let _ = crate::platform::set_permissions_owner_only(&self.socket_path);
        let _ = crate::platform::set_permissions_owner_only(&self.debug_socket_path);

        // Set logging context for this server
        crate::logging::set_server(&self.identity.name);

        // Log server identity
        crate::logging::info(&format!(
            "Server {} starting ({})",
            self.identity.display_name(),
            self.identity.version
        ));
        crate::logging::info(&format!("Server listening on {:?}", self.socket_path));
        crate::logging::info(&format!("Debug socket on {:?}", self.debug_socket_path));

        // Write server git hash next to socket so clients can detect version mismatches
        let hash_path = format!("{}.hash", self.socket_path.display());
        let _ = std::fs::write(&hash_path, env!("JCODE_GIT_HASH"));

        // Register server in the registry so session picker and clients can discover it.
        // Do registration inline (fast) but defer cleanup to background (probes sockets).
        {
            let mut registry = crate::registry::ServerRegistry::load()
                .await
                .unwrap_or_default();
            let info = crate::registry::ServerInfo {
                id: self.identity.id.clone(),
                name: self.identity.name.clone(),
                icon: self.identity.icon.clone(),
                socket: self.socket_path.clone(),
                debug_socket: self.debug_socket_path.clone(),
                git_hash: self.identity.git_hash.clone(),
                version: self.identity.version.clone(),
                pid: std::process::id(),
                started_at: chrono::Utc::now().to_rfc3339(),
                sessions: Vec::new(),
            };
            registry.register(info);
            let _ = registry.save().await;
            crate::logging::info(&format!(
                "Registered as {} in server registry",
                self.identity.display_name(),
            ));
        }

        // Cleanup stale registry entries in background (probes sockets, can be slow)
        tokio::spawn(async {
            if let Ok(mut registry) = crate::registry::ServerRegistry::load().await {
                let _ = registry.cleanup_stale().await;
                let _ = registry.save().await;
            }
        });

        // Preload embedding model in background so first memory recall is fast
        tokio::task::spawn_blocking(|| {
            let start = std::time::Instant::now();
            match crate::embedding::get_embedder() {
                Ok(_) => {
                    crate::logging::info(&format!(
                        "Embedding model preloaded in {}ms",
                        start.elapsed().as_millis()
                    ));
                }
                Err(e) => {
                    crate::logging::info(&format!(
                        "Embedding model preload failed (non-fatal): {}",
                        e
                    ));
                }
            }
        });

        // Spawn selfdev reload monitor (event-driven via in-process channel)
        // Only run on selfdev servers to avoid non-selfdev servers picking up
        // reload signals, which would kill all their client connections
        if is_selfdev_env() {
            let signal_sessions = Arc::clone(&self.sessions);
            let signal_swarm_members = Arc::clone(&self.swarm_members);
            let signal_shutdown_signals = Arc::clone(&self.shutdown_signals);
            tokio::spawn(async move {
                await_reload_signal(
                    signal_sessions,
                    signal_swarm_members,
                    signal_shutdown_signals,
                )
                .await;
            });
        }

        // Log when we receive SIGTERM for debugging
        #[cfg(unix)]
        {
            let sigterm_server_name = self.identity.name.clone();
            tokio::spawn(async move {
                use tokio::signal::unix::{signal, SignalKind};
                if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
                    sigterm.recv().await;
                    crate::logging::info("Server received SIGTERM, shutting down gracefully");
                    let _ = crate::registry::unregister_server(&sigterm_server_name).await;
                    std::process::exit(0);
                }
            });
        }

        // Spawn the bus monitor for swarm coordination
        let monitor_file_touches = Arc::clone(&self.file_touches);
        let monitor_swarm_members = Arc::clone(&self.swarm_members);
        let monitor_swarms_by_id = Arc::clone(&self.swarms_by_id);
        let monitor_swarm_plans = Arc::clone(&self.swarm_plans);
        let monitor_swarm_coordinators = Arc::clone(&self.swarm_coordinators);
        let monitor_shared_context = Arc::clone(&self.shared_context);
        let monitor_sessions = Arc::clone(&self.sessions);
        let monitor_event_history = Arc::clone(&self.event_history);
        let monitor_event_counter = Arc::clone(&self.event_counter);
        let monitor_swarm_event_tx = self.swarm_event_tx.clone();
        tokio::spawn(async move {
            Self::monitor_bus(
                monitor_file_touches,
                monitor_swarm_members,
                monitor_swarms_by_id,
                monitor_swarm_plans,
                monitor_swarm_coordinators,
                monitor_shared_context,
                monitor_sessions,
                monitor_event_history,
                monitor_event_counter,
                monitor_swarm_event_tx,
            )
            .await;
        });

        // Note: No default session created here - each client creates its own session

        // Initialize the memory agent early so it's ready for all sessions
        if crate::config::config().features.memory {
            tokio::spawn(async {
                let _ = crate::memory_agent::init().await;
            });
        }

        // Spawn ambient mode background loop if enabled
        if let Some(ref runner) = self.ambient_runner {
            let ambient_handle = runner.clone();
            let ambient_provider = Arc::clone(&self.provider);
            crate::logging::info("Ambient mode enabled - starting background loop");
            tokio::spawn(async move {
                ambient_handle.run_loop(ambient_provider).await;
            });
        }

        // Spawn embedding idle monitor so the model can be unloaded when this
        // server has been quiet for a while.
        let embedding_idle_secs = embedding_idle_unload_secs();
        tokio::spawn(async move {
            let idle_for = std::time::Duration::from_secs(embedding_idle_secs);
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(EMBEDDING_IDLE_CHECK_SECS));
            loop {
                interval.tick().await;
                let unloaded = crate::embedding::maybe_unload_if_idle(idle_for);
                if unloaded {
                    let stats = crate::embedding::stats();
                    crate::logging::info(&format!(
                        "Embedding idle monitor: model unloaded (loads={}, unloads={}, calls={}, avg_ms={})",
                        stats.load_count,
                        stats.unload_count,
                        stats.embed_calls,
                        stats
                            .avg_embed_ms
                            .map(|v| format!("{:.1}", v))
                            .unwrap_or_else(|| "n/a".to_string())
                    ));
                }
            }
        });

        if debug_control_allowed() {
            crate::logging::info("Debug control enabled; idle timeout monitor disabled.");
        } else {
            let idle_client_count = Arc::clone(&self.client_count);
            let idle_server_name = self.identity.name.clone();
            tokio::spawn(async move {
                let mut idle_since: Option<std::time::Instant> = None;
                let mut check_interval = tokio::time::interval(std::time::Duration::from_secs(10));

                loop {
                    check_interval.tick().await;

                    let count = *idle_client_count.read().await;

                    if count == 0 {
                        // No clients connected
                        if idle_since.is_none() {
                            idle_since = Some(std::time::Instant::now());
                            crate::logging::info(&format!(
                                "No clients connected. Server will exit after {} minutes of idle.",
                                IDLE_TIMEOUT_SECS / 60
                            ));
                        }

                        if let Some(since) = idle_since {
                            let idle_duration = since.elapsed().as_secs();
                            if idle_duration >= IDLE_TIMEOUT_SECS {
                                crate::logging::info(&format!(
                                    "Server idle for {} minutes with no clients. Shutting down.",
                                    idle_duration / 60
                                ));
                                let _ = crate::registry::unregister_server(&idle_server_name).await;
                                std::process::exit(EXIT_IDLE_TIMEOUT);
                            }
                        }
                    } else {
                        // Clients connected - reset idle timer
                        if idle_since.is_some() {
                            crate::logging::info("Client connected. Idle timer cancelled.");
                        }
                        idle_since = None;
                    }
                }
            });
        }

        // Spawn main socket handler
        let main_sessions = Arc::clone(&self.sessions);
        let main_event_tx = self.event_tx.clone();
        let main_provider = Arc::clone(&self.provider);
        let main_is_processing = Arc::clone(&self.is_processing);
        let main_session_id = Arc::clone(&self.session_id);
        let main_client_count = Arc::clone(&self.client_count);
        let main_client_connections = Arc::clone(&self.client_connections);
        let main_swarm_members = Arc::clone(&self.swarm_members);
        let main_swarms_by_id = Arc::clone(&self.swarms_by_id);
        let main_shared_context = Arc::clone(&self.shared_context);
        let main_swarm_plans = Arc::clone(&self.swarm_plans);
        let main_swarm_coordinators = Arc::clone(&self.swarm_coordinators);
        let main_file_touches = Arc::clone(&self.file_touches);
        let main_channel_subscriptions = Arc::clone(&self.channel_subscriptions);
        let main_client_debug_state = Arc::clone(&self.client_debug_state);
        let main_client_debug_response_tx = self.client_debug_response_tx.clone();
        let main_event_history = Arc::clone(&self.event_history);
        let main_event_counter = Arc::clone(&self.event_counter);
        let main_swarm_event_tx = self.swarm_event_tx.clone();
        let main_server_name = self.identity.name.clone();
        let main_server_icon = self.identity.icon.clone();
        let main_ambient_runner = self.ambient_runner.clone();
        let main_mcp_pool = Arc::clone(&self.mcp_pool);
        let main_shutdown_signals = Arc::clone(&self.shutdown_signals);

        let main_handle = tokio::spawn(async move {
            loop {
                match main_listener.accept().await {
                    Ok((stream, _)) => {
                        let sessions = Arc::clone(&main_sessions);
                        let event_tx = main_event_tx.clone();
                        let provider = Arc::clone(&main_provider);
                        let is_processing = Arc::clone(&main_is_processing);
                        let session_id = Arc::clone(&main_session_id);
                        let client_count = Arc::clone(&main_client_count);
                        let client_connections = Arc::clone(&main_client_connections);
                        let swarm_members = Arc::clone(&main_swarm_members);
                        let swarms_by_id = Arc::clone(&main_swarms_by_id);
                        let shared_context = Arc::clone(&main_shared_context);
                        let swarm_plans = Arc::clone(&main_swarm_plans);
                        let swarm_coordinators = Arc::clone(&main_swarm_coordinators);
                        let file_touches = Arc::clone(&main_file_touches);
                        let channel_subscriptions = Arc::clone(&main_channel_subscriptions);
                        let client_debug_state = Arc::clone(&main_client_debug_state);
                        let client_debug_response_tx = main_client_debug_response_tx.clone();
                        let event_history = Arc::clone(&main_event_history);
                        let event_counter = Arc::clone(&main_event_counter);
                        let swarm_event_tx = main_swarm_event_tx.clone();
                        let server_name = main_server_name.clone();
                        let server_icon = main_server_icon.clone();
                        let ambient_runner = main_ambient_runner.clone();
                        let mcp_pool = Arc::clone(&main_mcp_pool);
                        let shutdown_signals = Arc::clone(&main_shutdown_signals);

                        // Increment client count
                        *client_count.write().await += 1;

                        tokio::spawn(async move {
                            let result = handle_client(
                                stream,
                                sessions,
                                event_tx,
                                provider,
                                is_processing,
                                session_id,
                                Arc::clone(&client_count),
                                client_connections,
                                swarm_members,
                                swarms_by_id,
                                shared_context,
                                swarm_plans,
                                swarm_coordinators,
                                file_touches,
                                channel_subscriptions,
                                client_debug_state,
                                client_debug_response_tx,
                                event_history,
                                event_counter,
                                swarm_event_tx,
                                server_name,
                                server_icon,
                                mcp_pool,
                                shutdown_signals,
                            )
                            .await;

                            // Decrement client count when done
                            *client_count.write().await -= 1;

                            // Nudge ambient runner on session close
                            if let Some(ref runner) = ambient_runner {
                                runner.nudge();
                            }

                            if let Err(e) = result {
                                crate::logging::error(&format!("Client error: {}", e));
                            }
                        });
                    }
                    Err(e) => {
                        crate::logging::error(&format!("Main accept error: {}", e));
                    }
                }
            }
        });

        // Spawn debug socket handler
        let debug_sessions = Arc::clone(&self.sessions);
        let debug_is_processing = Arc::clone(&self.is_processing);
        let debug_session_id = Arc::clone(&self.session_id);
        let debug_provider = Arc::clone(&self.provider);
        let debug_client_debug_state = Arc::clone(&self.client_debug_state);
        let debug_client_connections = Arc::clone(&self.client_connections);
        let debug_swarm_members = Arc::clone(&self.swarm_members);
        let debug_swarms_by_id = Arc::clone(&self.swarms_by_id);
        let debug_shared_context = Arc::clone(&self.shared_context);
        let debug_swarm_plans = Arc::clone(&self.swarm_plans);
        let debug_swarm_coordinators = Arc::clone(&self.swarm_coordinators);
        let debug_file_touches = Arc::clone(&self.file_touches);
        let debug_channel_subscriptions = Arc::clone(&self.channel_subscriptions);
        let debug_client_debug_response_tx = self.client_debug_response_tx.clone();
        let debug_jobs = Arc::clone(&self.debug_jobs);
        let debug_event_history = Arc::clone(&self.event_history);
        let debug_event_counter = Arc::clone(&self.event_counter);
        let debug_swarm_event_tx = self.swarm_event_tx.clone();
        let debug_server_identity = self.identity.clone();
        let debug_start_time = std::time::Instant::now();
        let debug_ambient_runner = self.ambient_runner.clone();
        let debug_mcp_pool = Arc::clone(&self.mcp_pool);

        let debug_handle = tokio::spawn(async move {
            loop {
                match debug_listener.accept().await {
                    Ok((stream, _)) => {
                        let sessions = Arc::clone(&debug_sessions);
                        let is_processing = Arc::clone(&debug_is_processing);
                        let session_id = Arc::clone(&debug_session_id);
                        let provider = Arc::clone(&debug_provider);
                        let client_debug_state = Arc::clone(&debug_client_debug_state);
                        let client_connections = Arc::clone(&debug_client_connections);
                        let swarm_members = Arc::clone(&debug_swarm_members);
                        let swarms_by_id = Arc::clone(&debug_swarms_by_id);
                        let shared_context = Arc::clone(&debug_shared_context);
                        let swarm_plans = Arc::clone(&debug_swarm_plans);
                        let swarm_coordinators = Arc::clone(&debug_swarm_coordinators);
                        let file_touches = Arc::clone(&debug_file_touches);
                        let channel_subscriptions = Arc::clone(&debug_channel_subscriptions);
                        let client_debug_response_tx = debug_client_debug_response_tx.clone();
                        let debug_jobs = Arc::clone(&debug_jobs);
                        let event_history = Arc::clone(&debug_event_history);
                        let event_counter = Arc::clone(&debug_event_counter);
                        let swarm_event_tx = debug_swarm_event_tx.clone();
                        let server_identity = debug_server_identity.clone();
                        let server_start_time = debug_start_time;
                        let ambient_runner = debug_ambient_runner.clone();
                        let mcp_pool = Some(debug_mcp_pool.clone());

                        tokio::spawn(async move {
                            if let Err(e) = handle_debug_client(
                                stream,
                                sessions,
                                is_processing,
                                session_id,
                                provider,
                                client_connections,
                                swarm_members,
                                swarms_by_id,
                                shared_context,
                                swarm_plans,
                                swarm_coordinators,
                                file_touches,
                                channel_subscriptions,
                                client_debug_state,
                                client_debug_response_tx,
                                debug_jobs,
                                event_history,
                                event_counter,
                                swarm_event_tx,
                                server_identity,
                                server_start_time,
                                ambient_runner,
                                mcp_pool,
                            )
                            .await
                            {
                                crate::logging::error(&format!("Debug client error: {}", e));
                            }
                        });
                    }
                    Err(e) => {
                        crate::logging::error(&format!("Debug accept error: {}", e));
                    }
                }
            }
        });

        // Spawn WebSocket gateway for iOS/web clients (if enabled)
        let _gateway_handle = self.spawn_gateway();

        // Wait for both to complete (they won't normally)
        let _ = tokio::join!(main_handle, debug_handle);
        Ok(())
    }

    /// Spawn the WebSocket gateway if enabled in config.
    /// Returns a task handle that accepts gateway clients and feeds them
    /// into handle_client just like Unix socket connections.
    fn spawn_gateway(&self) -> Option<tokio::task::JoinHandle<()>> {
        let gw_config = &crate::config::config().gateway;
        if !gw_config.enabled {
            return None;
        }

        let config = crate::gateway::GatewayConfig {
            port: gw_config.port,
            bind_addr: gw_config.bind_addr.clone(),
            enabled: true,
        };

        let (client_tx, mut client_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::gateway::GatewayClient>();

        // Spawn the TCP/WebSocket listener
        tokio::spawn(async move {
            if let Err(e) = crate::gateway::run_gateway(config, client_tx).await {
                crate::logging::error(&format!("Gateway error: {}", e));
            }
        });

        // Spawn a task that receives gateway clients and plugs them into handle_client
        let gw_sessions = Arc::clone(&self.sessions);
        let gw_event_tx = self.event_tx.clone();
        let gw_provider = Arc::clone(&self.provider);
        let gw_is_processing = Arc::clone(&self.is_processing);
        let gw_session_id = Arc::clone(&self.session_id);
        let gw_client_count = Arc::clone(&self.client_count);
        let gw_client_connections = Arc::clone(&self.client_connections);
        let gw_swarm_members = Arc::clone(&self.swarm_members);
        let gw_swarms_by_id = Arc::clone(&self.swarms_by_id);
        let gw_shared_context = Arc::clone(&self.shared_context);
        let gw_swarm_plans = Arc::clone(&self.swarm_plans);
        let gw_swarm_coordinators = Arc::clone(&self.swarm_coordinators);
        let gw_file_touches = Arc::clone(&self.file_touches);
        let gw_channel_subscriptions = Arc::clone(&self.channel_subscriptions);
        let gw_client_debug_state = Arc::clone(&self.client_debug_state);
        let gw_client_debug_response_tx = self.client_debug_response_tx.clone();
        let gw_event_history = Arc::clone(&self.event_history);
        let gw_event_counter = Arc::clone(&self.event_counter);
        let gw_swarm_event_tx = self.swarm_event_tx.clone();
        let gw_server_name = self.identity.name.clone();
        let gw_server_icon = self.identity.icon.clone();
        let gw_ambient_runner = self.ambient_runner.clone();
        let gw_mcp_pool = Arc::clone(&self.mcp_pool);
        let gw_shutdown_signals = Arc::clone(&self.shutdown_signals);

        let handle = tokio::spawn(async move {
            while let Some(gw_client) = client_rx.recv().await {
                let sessions = Arc::clone(&gw_sessions);
                let event_tx = gw_event_tx.clone();
                let provider = Arc::clone(&gw_provider);
                let is_processing = Arc::clone(&gw_is_processing);
                let session_id = Arc::clone(&gw_session_id);
                let client_count = Arc::clone(&gw_client_count);
                let client_connections = Arc::clone(&gw_client_connections);
                let swarm_members = Arc::clone(&gw_swarm_members);
                let swarms_by_id = Arc::clone(&gw_swarms_by_id);
                let shared_context = Arc::clone(&gw_shared_context);
                let swarm_plans = Arc::clone(&gw_swarm_plans);
                let swarm_coordinators = Arc::clone(&gw_swarm_coordinators);
                let file_touches = Arc::clone(&gw_file_touches);
                let channel_subscriptions = Arc::clone(&gw_channel_subscriptions);
                let client_debug_state = Arc::clone(&gw_client_debug_state);
                let client_debug_response_tx = gw_client_debug_response_tx.clone();
                let event_history = Arc::clone(&gw_event_history);
                let event_counter = Arc::clone(&gw_event_counter);
                let swarm_event_tx = gw_swarm_event_tx.clone();
                let server_name = gw_server_name.clone();
                let server_icon = gw_server_icon.clone();
                let ambient_runner = gw_ambient_runner.clone();
                let mcp_pool = Arc::clone(&gw_mcp_pool);
                let shutdown_signals = Arc::clone(&gw_shutdown_signals);

                *client_count.write().await += 1;

                crate::logging::info(&format!(
                    "Gateway client connected: {} ({})",
                    gw_client.device_name, gw_client.device_id
                ));

                tokio::spawn(async move {
                    let result = handle_client(
                        gw_client.stream,
                        sessions,
                        event_tx,
                        provider,
                        is_processing,
                        session_id,
                        Arc::clone(&client_count),
                        client_connections,
                        swarm_members,
                        swarms_by_id,
                        shared_context,
                        swarm_plans,
                        swarm_coordinators,
                        file_touches,
                        channel_subscriptions,
                        client_debug_state,
                        client_debug_response_tx,
                        event_history,
                        event_counter,
                        swarm_event_tx,
                        server_name,
                        server_icon,
                        mcp_pool,
                        shutdown_signals,
                    )
                    .await;

                    *client_count.write().await -= 1;

                    if let Err(e) = result {
                        crate::logging::error(&format!("Gateway client error: {}", e));
                    }
                });
            }
        });

        Some(handle)
    }
}

async fn handle_client(
    stream: Stream,
    sessions: Arc<RwLock<HashMap<String, Arc<Mutex<Agent>>>>>,
    _global_event_tx: broadcast::Sender<ServerEvent>,
    provider_template: Arc<dyn Provider>,
    _global_is_processing: Arc<RwLock<bool>>,
    global_session_id: Arc<RwLock<String>>,
    client_count: Arc<RwLock<usize>>,
    client_connections: Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
    swarm_members: Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    shared_context: Arc<RwLock<HashMap<String, HashMap<String, SharedContext>>>>,
    swarm_plans: Arc<RwLock<HashMap<String, VersionedPlan>>>,
    swarm_coordinators: Arc<RwLock<HashMap<String, String>>>,
    file_touches: Arc<RwLock<HashMap<PathBuf, Vec<FileAccess>>>>,
    channel_subscriptions: Arc<RwLock<HashMap<String, HashMap<String, HashSet<String>>>>>,
    client_debug_state: Arc<RwLock<ClientDebugState>>,
    client_debug_response_tx: broadcast::Sender<(u64, String)>,
    event_history: Arc<RwLock<Vec<SwarmEvent>>>,
    event_counter: Arc<std::sync::atomic::AtomicU64>,
    swarm_event_tx: broadcast::Sender<SwarmEvent>,
    server_name: String,
    server_icon: String,
    mcp_pool: Arc<crate::mcp::SharedMcpPool>,
    shutdown_signals: Arc<RwLock<HashMap<String, crate::agent::InterruptSignal>>>,
) -> Result<()> {
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let writer = Arc::new(Mutex::new(writer));
    let mut line = String::new();

    // Per-client state
    let mut client_is_processing = false;
    let (processing_done_tx, mut processing_done_rx) =
        mpsc::unbounded_channel::<(u64, Result<()>)>();
    let mut processing_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut processing_message_id: Option<u64> = None;
    let mut processing_session_id: Option<String> = None;
    // Client selfdev status is determined by Subscribe request, not server's env
    let mut client_selfdev = false;

    let provider = provider_template.fork();
    let registry = Registry::new(provider.clone()).await;
    let mut swarm_enabled = crate::config::config().features.swarm;

    // Create a new session for this client
    let mut new_agent = Agent::new(Arc::clone(&provider), registry.clone());
    new_agent.set_memory_enabled(crate::config::config().features.memory);
    let mut client_session_id = new_agent.session_id().to_string();
    let friendly_name = new_agent.session_short_name().map(|s| s.to_string());
    let client_connection_id = id::new_id("conn");
    let connected_at = Instant::now();

    {
        let mut connections = client_connections.write().await;
        connections.insert(
            client_connection_id.clone(),
            ClientConnectionInfo {
                client_id: client_connection_id.clone(),
                session_id: client_session_id.clone(),
                connected_at,
                last_seen: connected_at,
            },
        );
    }

    {
        let mut current = global_session_id.write().await;
        if current.is_empty() || *current != client_session_id {
            *current = client_session_id.clone();
        }
    }

    // Get a handle to the soft interrupt queue BEFORE wrapping in Mutex
    // This allows queueing interrupts while the agent is processing
    let soft_interrupt_queue = new_agent.soft_interrupt_queue();

    // Get a handle to the background tool signal BEFORE wrapping in Mutex
    // This allows signaling "move to background" while the agent is processing
    let background_tool_signal = new_agent.background_tool_signal();

    // Get a handle to the graceful shutdown signal BEFORE wrapping in Mutex
    // This allows signaling cancel (checkpoint partial response) without needing the lock
    let cancel_signal = new_agent.graceful_shutdown_signal();

    // Register the shutdown signal in the server-level map so
    // graceful_shutdown_sessions can signal it without locking the agent mutex
    {
        let mut signals = shutdown_signals.write().await;
        signals.insert(client_session_id.clone(), cancel_signal.clone());
    }

    let agent = Arc::new(Mutex::new(new_agent));
    {
        let mut sessions_guard = sessions.write().await;
        sessions_guard.insert(client_session_id.clone(), Arc::clone(&agent));
    }

    // Per-client event channel (not shared with other clients)
    let (client_event_tx, mut client_event_rx) =
        tokio::sync::mpsc::unbounded_channel::<ServerEvent>();

    // Get the working directory and swarm id (shared across worktrees)
    let working_dir = std::env::current_dir().ok();
    let swarm_id = if swarm_enabled {
        swarm_id_for_dir(working_dir.clone())
    } else {
        None
    };

    // Register this client as a swarm member
    {
        let mut members = swarm_members.write().await;
        let now = Instant::now();
        members.insert(
            client_session_id.clone(),
            SwarmMember {
                session_id: client_session_id.clone(),
                event_tx: client_event_tx.clone(),
                working_dir: working_dir.clone(),
                swarm_id: swarm_id.clone(),
                swarm_enabled,
                status: "spawned".to_string(),
                detail: None,
                friendly_name: friendly_name.clone(),
                role: "agent".to_string(),
                joined_at: now,
                last_status_change: now,
            },
        );
    }
    // Add to swarm by swarm_id (separate scope to avoid holding swarm_members lock)
    if let Some(ref id) = swarm_id {
        let mut swarms = swarms_by_id.write().await;
        swarms
            .entry(id.clone())
            .or_insert_with(HashSet::new)
            .insert(client_session_id.clone());
        record_swarm_event(
            &event_history,
            &event_counter,
            &swarm_event_tx,
            client_session_id.clone(),
            friendly_name.clone(),
            Some(id.clone()),
            SwarmEventType::MemberChange {
                action: "joined".to_string(),
            },
        )
        .await;
    }
    let mut is_new_coordinator = false;
    if let Some(ref id) = swarm_id {
        let mut coordinators = swarm_coordinators.write().await;
        if coordinators.get(id).is_none() {
            coordinators.insert(id.clone(), client_session_id.clone());
            is_new_coordinator = true;
        }
    }
    // Update role separately to avoid nested lock
    if is_new_coordinator {
        let mut members = swarm_members.write().await;
        if let Some(m) = members.get_mut(&client_session_id) {
            m.role = "coordinator".to_string();
        }
    }
    if let Some(ref id) = swarm_id {
        broadcast_swarm_status(id, &swarm_members, &swarms_by_id).await;
    }
    update_member_status(
        &client_session_id,
        "ready",
        None,
        &swarm_members,
        &swarms_by_id,
        Some(&event_history),
        Some(&event_counter),
        Some(&swarm_event_tx),
    )
    .await;
    if is_new_coordinator {
        let msg = "You are the coordinator for this swarm.".to_string();
        let _ = client_event_tx.send(ServerEvent::Notification {
            from_session: client_session_id.clone(),
            from_name: friendly_name.clone(),
            notification_type: NotificationType::Message {
                scope: Some("swarm".to_string()),
                channel: None,
            },
            message: msg.clone(),
        });
    }

    // Spawn event forwarder for this client only
    let writer_clone = Arc::clone(&writer);
    let event_handle = tokio::spawn(async move {
        while let Some(event) = client_event_rx.recv().await {
            let json = encode_event(&event);
            let mut w = writer_clone.lock().await;
            if w.write_all(json.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    // Note: Don't send initial SessionId here - it's sent by the Subscribe handler
    // Sending it via the channel causes race conditions where it can arrive after
    // other events (like History) that are written directly to the socket.

    // Set up client debug command channel
    // This client becomes the "active" debug client that receives client: commands
    let (debug_cmd_tx, mut debug_cmd_rx) = mpsc::unbounded_channel::<(u64, String)>();
    let client_debug_id = id::new_id("client");
    {
        let mut debug_state = client_debug_state.write().await;
        debug_state.register(client_debug_id.clone(), debug_cmd_tx);
    }

    let stdin_responses: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<String>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Subscribe to bus events so we can forward ModelsUpdated to this client
    // (e.g. when Copilot finishes async init after the initial History was sent)
    let mut bus_rx = Bus::global().subscribe();

    // Set up stdin request forwarding: tools send StdinInputRequest, we forward to TUI
    let (stdin_req_tx, mut stdin_req_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::tool::StdinInputRequest>();
    {
        let mut agent_guard = agent.lock().await;
        agent_guard.set_stdin_request_tx(stdin_req_tx);
    }
    let stdin_forwarder = {
        let client_event_tx = client_event_tx.clone();
        let stdin_responses = stdin_responses.clone();
        let tool_call_id = String::new();
        tokio::spawn(async move {
            while let Some(req) = stdin_req_rx.recv().await {
                let request_id = req.request_id.clone();
                stdin_responses
                    .lock()
                    .await
                    .insert(request_id.clone(), req.response_tx);
                let _ = client_event_tx.send(ServerEvent::StdinRequest {
                    request_id,
                    prompt: req.prompt,
                    is_password: req.is_password,
                    tool_call_id: tool_call_id.clone(),
                });
            }
        })
    };

    loop {
        line.clear();
        tokio::select! {
            // Forward ModelsUpdated bus events to this client
            // (fired when Copilot/OpenAI async init completes after History was already sent)
            bus_event = bus_rx.recv() => {
                if matches!(bus_event, Ok(BusEvent::ModelsUpdated)) {
                    let (models, model_routes) = {
                        let agent_guard = agent.lock().await;
                        (
                            agent_guard.available_models_display(),
                            agent_guard.model_routes(),
                        )
                    };
                    let _ = client_event_tx.send(ServerEvent::AvailableModelsUpdated {
                        available_models: models,
                        available_model_routes: model_routes,
                    });
                }
                continue;
            }
            // Handle client debug commands from debug socket
            debug_cmd = debug_cmd_rx.recv() => {
                if let Some((request_id, command)) = debug_cmd {
                    if client_event_tx
                        .send(ServerEvent::ClientDebugRequest {
                            id: request_id,
                            command,
                        })
                        .is_err()
                    {
                        let _ = client_debug_response_tx.send((
                            request_id,
                            "No TUI client connected".to_string(),
                        ));
                    }
                }
                continue;
            }
            done = processing_done_rx.recv() => {
                if let Some((done_id, result)) = done {
                    if Some(done_id) != processing_message_id {
                        crate::logging::warn(&format!(
                            "Done event id={} doesn't match processing_message_id={:?}, dropping",
                            done_id, processing_message_id
                        ));
                        continue;
                    }
                    crate::logging::info(&format!(
                        "Processing done for message id={}, result={}",
                        done_id,
                        if result.is_ok() { "ok" } else { "err" }
                    ));
                    processing_message_id = None;
                    processing_task = None;
                    client_is_processing = false;

                    let done_session = processing_session_id.take();
                    match result {
                        Ok(()) => {
                            if let Some(session_id) = done_session.as_deref() {
                                update_member_status(
                                    session_id,
                                    "ready",
                                    None,
                                    &swarm_members,
                                    &swarms_by_id,
                                    Some(&event_history),
                                    Some(&event_counter),
                                    Some(&swarm_event_tx),
                                )
                                .await;
                            }
                            let _ = client_event_tx.send(ServerEvent::Done { id: done_id });
                        }
                        Err(e) => {
                            if let Some(session_id) = done_session.as_deref() {
                                update_member_status(
                                    session_id,
                                    "failed",
                                    Some(truncate_detail(&e.to_string(), 120)),
                                    &swarm_members,
                                    &swarms_by_id,
                                    Some(&event_history),
                                    Some(&event_counter),
                                    Some(&swarm_event_tx),
                                )
                                .await;
                            }
                            let retry_after_secs = e.downcast_ref::<StreamError>().and_then(|se| se.retry_after_secs);
                            let _ = client_event_tx.send(ServerEvent::Error {
                                id: done_id,
                                message: e.to_string(),
                                retry_after_secs,
                            });
                        }
                    }
                } else {
                    break;
                }
                continue;
            }
            n = reader.read_line(&mut line) => {
                let n = match n {
                    Ok(n) => n,
                    Err(e) => {
                        crate::logging::error(&format!("Client read error: {}", e));
                        break;
                    }
                };
                if n == 0 {
                    break; // Client disconnected
                }
                let mut connections = client_connections.write().await;
                if let Some(info) = connections.get_mut(&client_connection_id) {
                    info.last_seen = Instant::now();
                }
            }
        }

        let request = match decode_request(&line) {
            Ok(r) => r,
            Err(e) => {
                let event = ServerEvent::Error {
                    id: 0,
                    message: format!("Invalid request: {}", e),
                    retry_after_secs: None,
                };
                let json = encode_event(&event);
                let mut w = writer.lock().await;
                if w.write_all(json.as_bytes()).await.is_err() {
                    break;
                }
                continue;
            }
        };

        // Send ack
        let ack = ServerEvent::Ack { id: request.id() };
        let json = encode_event(&ack);
        {
            let mut w = writer.lock().await;
            if w.write_all(json.as_bytes()).await.is_err() {
                break;
            }
        }

        match request {
            Request::Message {
                id,
                content,
                images,
            } => {
                // Check if this client is already processing
                if client_is_processing {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: "Already processing a message".to_string(),
                        retry_after_secs: None,
                    });
                    continue;
                }

                // Set processing flag for this client
                client_is_processing = true;
                processing_message_id = Some(id);
                processing_session_id = Some(client_session_id.clone());

                update_member_status(
                    &client_session_id,
                    "running",
                    Some(truncate_detail(&content, 120)),
                    &swarm_members,
                    &swarms_by_id,
                    Some(&event_history),
                    Some(&event_counter),
                    Some(&swarm_event_tx),
                )
                .await;

                let agent = Arc::clone(&agent);
                let tx = client_event_tx.clone();
                let done_tx = processing_done_tx.clone();
                crate::logging::info(&format!("Processing message id={} spawning task", id));
                processing_task = Some(tokio::spawn(async move {
                    let result = match std::panic::AssertUnwindSafe(process_message_streaming_mpsc(
                        agent, &content, images, tx,
                    ))
                    .catch_unwind()
                    .await
                    {
                        Ok(r) => r,
                        Err(panic_payload) => {
                            let msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                                s.to_string()
                            } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                                s.clone()
                            } else {
                                "unknown panic".to_string()
                            };
                            crate::logging::error(&format!(
                                "Processing task PANICKED for message id={}: {}",
                                id, msg
                            ));
                            Err(anyhow::anyhow!("Processing task panicked: {}", msg))
                        }
                    };
                    match &result {
                        Ok(()) => crate::logging::info(&format!(
                            "Processing task completed OK for message id={}",
                            id
                        )),
                        Err(e) => crate::logging::warn(&format!(
                            "Processing task completed with error for message id={}: {}",
                            id, e
                        )),
                    }
                    let _ = done_tx.send((id, result));
                }));
            }

            Request::Cancel { id } => {
                let _ = id; // cancel request id (not the message id)
                if let Some(mut handle) = processing_task.take() {
                    if handle.is_finished() {
                        processing_task = Some(handle);
                        continue;
                    }
                    // Signal graceful shutdown so the agent checkpoints partial content
                    // before exiting, then wait briefly for it to finish.
                    cancel_signal.fire();
                    match tokio::time::timeout(std::time::Duration::from_millis(500), &mut handle)
                        .await
                    {
                        Ok(_) => {
                            // Task exited gracefully within timeout
                        }
                        Err(_) => {
                            // Timed out waiting for graceful exit, force abort and wait
                            // for the task to actually release resources (e.g. agent mutex)
                            handle.abort();
                            match tokio::time::timeout(
                                std::time::Duration::from_millis(2000),
                                handle,
                            )
                            .await
                            {
                                Ok(_) => {
                                    crate::logging::info(
                                        "Aborted processing task released resources",
                                    );
                                }
                                Err(_) => {
                                    crate::logging::warn(
                                        "Aborted processing task did not release resources within 2s",
                                    );
                                }
                            }
                        }
                    }
                    // Reset the signal for future turns
                    cancel_signal.reset();
                    processing_task = None;
                    client_is_processing = false;
                    if let Some(session_id) = processing_session_id.take() {
                        update_member_status(
                            &session_id,
                            "stopped",
                            Some("cancelled".to_string()),
                            &swarm_members,
                            &swarms_by_id,
                            Some(&event_history),
                            Some(&event_counter),
                            Some(&swarm_event_tx),
                        )
                        .await;
                    }
                    if let Some(message_id) = processing_message_id.take() {
                        let _ = client_event_tx.send(ServerEvent::Interrupted);
                        let _ = client_event_tx.send(ServerEvent::Done { id: message_id });
                    }
                }
            }

            Request::SoftInterrupt {
                id,
                content,
                urgent,
            } => {
                // Queue a soft interrupt message to be injected at the next safe point
                // Uses the pre-extracted queue handle so we don't need the agent lock
                if let Ok(mut q) = soft_interrupt_queue.lock() {
                    q.push(crate::agent::SoftInterruptMessage { content, urgent });
                }
                let _ = client_event_tx.send(ServerEvent::Ack { id });
            }

            Request::CancelSoftInterrupts { id } => {
                // Cancel all pending soft interrupts (drain the queue)
                if let Ok(mut q) = soft_interrupt_queue.lock() {
                    q.clear();
                }
                let _ = client_event_tx.send(ServerEvent::Ack { id });
            }

            Request::BackgroundTool { id } => {
                // Signal the agent to move the currently executing tool to background
                background_tool_signal.fire();
                let _ = client_event_tx.send(ServerEvent::Ack { id });
            }

            Request::Clear { id } => {
                // Clear this client's session (create new agent)
                let preserve_debug = {
                    let agent_guard = agent.lock().await;
                    agent_guard.is_debug()
                };

                // Mark the old session as closed before replacing
                {
                    let mut agent_guard = agent.lock().await;
                    agent_guard.mark_closed();
                }

                let mut new_agent = Agent::new(Arc::clone(&provider), registry.clone());
                let new_id = new_agent.session_id().to_string();

                // Enable self-dev mode when running in a self-dev environment
                if client_selfdev {
                    new_agent.set_canary("self-dev");
                    // selfdev tools should already be registered from initial connection
                }
                if preserve_debug {
                    new_agent.set_debug(true);
                }

                // Replace the agent in place
                let mut agent_guard = agent.lock().await;
                *agent_guard = new_agent;
                drop(agent_guard);

                // Update sessions map
                {
                    let mut sessions_guard = sessions.write().await;
                    sessions_guard.remove(&client_session_id);
                    sessions_guard.insert(new_id.clone(), Arc::clone(&agent));
                }

                // Update swarm membership to the new session id
                let swarm_id_for_update = {
                    let mut members = swarm_members.write().await;
                    if let Some(mut member) = members.remove(&client_session_id) {
                        let sid = member.swarm_id.clone();
                        member.session_id = new_id.clone();
                        member.status = "ready".to_string();
                        member.detail = None;
                        members.insert(new_id.clone(), member);
                        sid
                    } else {
                        None
                    }
                };
                if let Some(ref swarm_id) = swarm_id_for_update {
                    let mut swarms = swarms_by_id.write().await;
                    if let Some(swarm) = swarms.get_mut(swarm_id) {
                        swarm.remove(&client_session_id);
                        swarm.insert(new_id.clone());
                    }
                }
                update_member_status(
                    &new_id,
                    "ready",
                    None,
                    &swarm_members,
                    &swarms_by_id,
                    Some(&event_history),
                    Some(&event_counter),
                    Some(&swarm_event_tx),
                )
                .await;
                if let Some(swarm_id) = swarm_id_for_update {
                    rename_plan_participant(&swarm_id, &client_session_id, &new_id, &swarm_plans)
                        .await;
                }

                client_session_id = new_id.clone();
                {
                    let mut connections = client_connections.write().await;
                    if let Some(info) = connections.get_mut(&client_connection_id) {
                        info.session_id = new_id.clone();
                        info.last_seen = Instant::now();
                    }
                }
                let _ = client_event_tx.send(ServerEvent::SessionId { session_id: new_id });
                let _ = client_event_tx.send(ServerEvent::Done { id });
            }

            Request::Ping { id } => {
                let json = encode_event(&ServerEvent::Pong { id });
                let mut w = writer.lock().await;
                if w.write_all(json.as_bytes()).await.is_err() {
                    break;
                }
            }

            Request::GetState { id } => {
                let sessions_guard = sessions.read().await;
                let all_sessions: Vec<String> = sessions_guard.keys().cloned().collect();
                let session_count = all_sessions.len();
                drop(sessions_guard);

                let event = ServerEvent::State {
                    id,
                    session_id: client_session_id.clone(),
                    message_count: session_count,
                    is_processing: client_is_processing,
                };
                let json = encode_event(&event);
                let mut w = writer.lock().await;
                if w.write_all(json.as_bytes()).await.is_err() {
                    break;
                }
            }

            Request::Subscribe {
                id,
                working_dir: subscribe_working_dir,
                selfdev,
            } => {
                // Update session working directory from client's cwd
                if let Some(ref dir) = subscribe_working_dir {
                    let mut agent_guard = agent.lock().await;
                    agent_guard.set_working_dir(dir);
                    drop(agent_guard);

                    // Update swarm member's working directory + swarm id
                    let new_path = PathBuf::from(dir);
                    let new_swarm_id = swarm_id_for_dir(Some(new_path.clone()));
                    let mut old_swarm_id: Option<String> = None;
                    let mut updated_swarm_id: Option<String> = None;
                    // Update member fields (separate scope to avoid nested locks)
                    {
                        let mut members = swarm_members.write().await;
                        if let Some(member) = members.get_mut(&client_session_id) {
                            old_swarm_id = member.swarm_id.clone();
                            member.working_dir = Some(new_path);
                            member.swarm_id = if member.swarm_enabled {
                                new_swarm_id.clone()
                            } else {
                                None
                            };
                            updated_swarm_id = member.swarm_id.clone();
                        }
                    }
                    // Remove from old swarm group
                    if let Some(ref old_id) = old_swarm_id {
                        let mut swarms = swarms_by_id.write().await;
                        if let Some(swarm) = swarms.get_mut(old_id) {
                            swarm.remove(&client_session_id);
                            if swarm.is_empty() {
                                swarms.remove(old_id);
                            }
                        }
                    }
                    // Add to new swarm group
                    if let Some(ref new_id) = updated_swarm_id {
                        let mut swarms = swarms_by_id.write().await;
                        swarms
                            .entry(new_id.clone())
                            .or_insert_with(HashSet::new)
                            .insert(client_session_id.clone());
                    }
                    if let Some(old_id) = old_swarm_id.clone() {
                        let was_coordinator = {
                            let coordinators = swarm_coordinators.read().await;
                            coordinators
                                .get(&old_id)
                                .map(|id| id == &client_session_id)
                                .unwrap_or(false)
                        };
                        if was_coordinator {
                            let mut new_coordinator: Option<String> = None;
                            {
                                let swarms = swarms_by_id.read().await;
                                if let Some(swarm) = swarms.get(&old_id) {
                                    new_coordinator = swarm.iter().min().cloned();
                                }
                            }
                            {
                                let mut coordinators = swarm_coordinators.write().await;
                                coordinators.remove(&old_id);
                                if let Some(ref new_id) = new_coordinator {
                                    coordinators.insert(old_id.clone(), new_id.clone());
                                }
                            }
                            // Notify new coordinator (separate scope to avoid nested lock)
                            if let Some(new_id) = new_coordinator.clone() {
                                let members = swarm_members.read().await;
                                if let Some(member) = members.get(&new_id) {
                                    let msg =
                                        "You are now the coordinator for this swarm.".to_string();
                                    let _ = member.event_tx.send(ServerEvent::Notification {
                                        from_session: new_id.clone(),
                                        from_name: member.friendly_name.clone(),
                                        notification_type: NotificationType::Message {
                                            scope: Some("swarm".to_string()),
                                            channel: None,
                                        },
                                        message: msg.clone(),
                                    });
                                }
                            }
                        }
                    }
                    if let Some(new_id) = updated_swarm_id.clone() {
                        let mut coordinators = swarm_coordinators.write().await;
                        if coordinators.get(&new_id).is_none() {
                            coordinators.insert(new_id.clone(), client_session_id.clone());
                            let msg = "You are the coordinator for this swarm.".to_string();
                            let _ = client_event_tx.send(ServerEvent::Notification {
                                from_session: client_session_id.clone(),
                                from_name: friendly_name.clone(),
                                notification_type: NotificationType::Message {
                                    scope: Some("swarm".to_string()),
                                    channel: None,
                                },
                                message: msg.clone(),
                            });
                        }
                    }
                    if let Some(old_id) = old_swarm_id.clone() {
                        if updated_swarm_id.as_ref() != Some(&old_id) {
                            remove_plan_participant(&old_id, &client_session_id, &swarm_plans)
                                .await;
                        }
                        broadcast_swarm_status(&old_id, &swarm_members, &swarms_by_id).await;
                    }
                    if let Some(new_id) = updated_swarm_id {
                        if old_swarm_id.as_ref() != Some(&new_id) {
                            broadcast_swarm_status(&new_id, &swarm_members, &swarms_by_id).await;
                        }
                    }
                }

                let mut should_selfdev = client_selfdev;
                if matches!(selfdev, Some(true)) {
                    should_selfdev = true;
                }

                if !should_selfdev {
                    if let Some(ref dir) = subscribe_working_dir {
                        let path = PathBuf::from(dir);
                        if is_jcode_repo_or_parent(&path) {
                            should_selfdev = true;
                        }
                    }
                }

                if should_selfdev {
                    client_selfdev = true;
                    let mut agent_guard = agent.lock().await;
                    if !agent_guard.is_canary() {
                        agent_guard.set_canary("self-dev");
                    }
                    drop(agent_guard);
                    registry.register_selfdev_tools().await;
                }

                // Register MCP tools (management tool + server tool proxies)
                // Shared pool means shared servers only spawn once across sessions
                registry
                    .register_mcp_tools(
                        Some(client_event_tx.clone()),
                        Some(Arc::clone(&mcp_pool)),
                        Some(client_session_id.clone()),
                    )
                    .await;

                // Note: Don't send SessionId here - it's included in the History response
                // from GetHistory. Sending it here causes race conditions when ResumeSession
                // is also called, as client_session_id may not yet be updated.
                let _ = client_event_tx.send(ServerEvent::Done { id });
            }

            Request::GetHistory { id } => {
                let _ = provider.prefetch_models().await;
                let (
                    messages,
                    is_canary,
                    provider_name,
                    provider_model,
                    available_models,
                    available_model_routes,
                    tool_names,
                    upstream_provider,
                ) = {
                    let agent_guard = agent.lock().await;
                    (
                        agent_guard.get_history(),
                        agent_guard.is_canary(),
                        agent_guard.provider_name(),
                        agent_guard.provider_model(),
                        agent_guard.available_models_display(),
                        agent_guard.model_routes(),
                        agent_guard.tool_names().await,
                        agent_guard.last_upstream_provider(),
                    )
                };

                // Build MCP server list with tool counts from registered tool names
                let mut mcp_map: std::collections::BTreeMap<String, usize> =
                    std::collections::BTreeMap::new();
                for name in &tool_names {
                    if let Some(rest) = name.strip_prefix("mcp__") {
                        if let Some((server, _tool)) = rest.split_once("__") {
                            *mcp_map.entry(server.to_string()).or_default() += 1;
                        }
                    }
                }
                let mcp_servers: Vec<String> = mcp_map
                    .into_iter()
                    .map(|(name, count)| format!("{}:{}", name, count))
                    .collect();

                // Build skills list
                let skills = crate::skill::SkillRegistry::load()
                    .map(|r| r.list().iter().map(|s| s.name.clone()).collect())
                    .unwrap_or_default();

                // Get all session IDs and client count
                let (all_sessions, current_client_count) = {
                    let sessions_guard = sessions.read().await;
                    let all: Vec<String> = sessions_guard.keys().cloned().collect();
                    let count = *client_count.read().await;
                    (all, count)
                };

                let event = ServerEvent::History {
                    id,
                    session_id: client_session_id.clone(),
                    messages,
                    provider_name: Some(provider_name),
                    provider_model: Some(provider_model),
                    available_models,
                    available_model_routes,
                    mcp_servers,
                    skills,
                    total_tokens: None,
                    all_sessions,
                    client_count: Some(current_client_count),
                    is_canary: Some(is_canary),
                    server_version: Some(env!("JCODE_VERSION").to_string()),
                    server_name: Some(server_name.clone()),
                    server_icon: Some(server_icon.clone()),
                    server_has_update: Some(server_has_newer_binary()),
                    was_interrupted: None,
                    upstream_provider,
                };
                let json = encode_event(&event);
                let mut w = writer.lock().await;
                if w.write_all(json.as_bytes()).await.is_err() {
                    break;
                }
            }

            Request::DebugCommand { id, .. } => {
                let _ = client_event_tx.send(ServerEvent::Error {
                    id,
                    message: "debug_command is only supported on the debug socket".to_string(),
                    retry_after_secs: None,
                });
            }

            Request::Reload { id } => {
                // Notify this client that server is reloading
                let _ = client_event_tx.send(ServerEvent::Reloading { new_socket: None });

                // Capture provider/model from the live session, not the stale
                // provider template created when the client connected.
                let (provider_arg, model_arg) = {
                    let agent_guard = agent.lock().await;
                    (
                        provider_cli_arg(&agent_guard.provider_name()),
                        normalize_model_arg(agent_guard.provider_model()),
                    )
                };

                // Spawn reload process with progress streaming
                let progress_tx = client_event_tx.clone();
                let socket_arg = socket_path().to_string_lossy().to_string();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    if let Err(e) = do_server_reload_with_progress(
                        progress_tx.clone(),
                        provider_arg,
                        model_arg,
                        socket_arg,
                    )
                    .await
                    {
                        let _ = progress_tx.send(ServerEvent::ReloadProgress {
                            step: "error".to_string(),
                            message: format!("Reload failed: {}", e),
                            success: Some(false),
                            output: None,
                        });
                        crate::logging::error(&format!("Reload failed: {}", e));
                    }
                });

                // Send Done after starting the reload (client will reconnect after server restarts)
                let _ = client_event_tx.send(ServerEvent::Done { id });
            }

            Request::ResumeSession { id, session_id } => {
                // Mark the current session as closed before switching
                {
                    let mut agent_guard = agent.lock().await;
                    agent_guard.mark_closed();
                }

                // Load the specified session into this client's agent
                let (result, is_canary) = {
                    let mut agent_guard = agent.lock().await;
                    let result = agent_guard.restore_session(&session_id);
                    if client_selfdev || is_selfdev_env() {
                        agent_guard.set_canary("self-dev");
                    }
                    let is_canary = agent_guard.is_canary();
                    (result, is_canary)
                };

                let was_interrupted = match &result {
                    Ok(status) => match status {
                        crate::session::SessionStatus::Crashed { .. } => true,
                        crate::session::SessionStatus::Active => {
                            let agent_guard = agent.lock().await;
                            let last_role = agent_guard.last_message_role();
                            let last_is_user = last_role
                                .as_ref()
                                .map(|r| *r == crate::message::Role::User)
                                .unwrap_or(false);
                            let last_is_reload_interrupted = last_role
                                .as_ref()
                                .map(|r| *r == crate::message::Role::Assistant)
                                .unwrap_or(false)
                                && agent_guard
                                    .last_message_text()
                                    .map(|t| {
                                        t.ends_with("[generation interrupted - server reloading]")
                                    })
                                    .unwrap_or(false);
                            if last_is_user {
                                crate::logging::info(&format!(
                                        "Session {} was Active with pending user message - treating as interrupted",
                                        session_id
                                    ));
                            }
                            if last_is_reload_interrupted {
                                crate::logging::info(&format!(
                                    "Session {} was interrupted by reload - will auto-resume",
                                    session_id
                                ));
                            }
                            last_is_user || last_is_reload_interrupted
                        }
                        _ => false,
                    },
                    Err(_) => false,
                };

                if result.is_ok() && is_canary {
                    client_selfdev = true;
                    registry.register_selfdev_tools().await;
                }

                // Register MCP tools for resumed sessions
                if result.is_ok() {
                    registry
                        .register_mcp_tools(
                            Some(client_event_tx.clone()),
                            Some(Arc::clone(&mcp_pool)),
                            Some(client_session_id.clone()),
                        )
                        .await;
                }

                match result {
                    Ok(_prev_status) => {
                        // Update client_session_id to match the restored session
                        let old_session_id = client_session_id.clone();
                        client_session_id = session_id.clone();

                        {
                            let mut sessions_guard = sessions.write().await;
                            sessions_guard.remove(&old_session_id);
                            sessions_guard.insert(session_id.clone(), Arc::clone(&agent));
                        }
                        {
                            let mut connections = client_connections.write().await;
                            if let Some(info) = connections.get_mut(&client_connection_id) {
                                info.session_id = session_id.clone();
                                info.last_seen = Instant::now();
                            }
                        }

                        {
                            let mut members = swarm_members.write().await;
                            if let Some(mut member) = members.remove(&old_session_id) {
                                if let Some(ref swarm_id) = member.swarm_id {
                                    let mut swarms = swarms_by_id.write().await;
                                    if let Some(swarm) = swarms.get_mut(swarm_id) {
                                        swarm.remove(&old_session_id);
                                        swarm.insert(session_id.clone());
                                    }
                                }
                                member.session_id = session_id.clone();
                                member.status = "ready".to_string();
                                member.detail = None;
                                members.insert(session_id.clone(), member);
                            }
                        }
                        {
                            let mut coordinators = swarm_coordinators.write().await;
                            for coord in coordinators.values_mut() {
                                if *coord == old_session_id {
                                    *coord = session_id.clone();
                                }
                            }
                        }
                        update_member_status(
                            &session_id,
                            "ready",
                            None,
                            &swarm_members,
                            &swarms_by_id,
                            Some(&event_history),
                            Some(&event_counter),
                            Some(&swarm_event_tx),
                        )
                        .await;
                        if let Some(swarm_id) = {
                            let members = swarm_members.read().await;
                            members.get(&session_id).and_then(|m| m.swarm_id.clone())
                        } {
                            rename_plan_participant(
                                &swarm_id,
                                &old_session_id,
                                &session_id,
                                &swarm_plans,
                            )
                            .await;
                        }

                        // Send updated history to client
                        let _ = provider.prefetch_models().await;
                        let (
                            messages,
                            is_canary,
                            provider_name,
                            provider_model,
                            available_models,
                            available_model_routes,
                            tool_names,
                            upstream_provider,
                        ) = {
                            let agent_guard = agent.lock().await;
                            (
                                agent_guard.get_history(),
                                agent_guard.is_canary(),
                                agent_guard.provider_name(),
                                agent_guard.provider_model(),
                                agent_guard.available_models_display(),
                                agent_guard.model_routes(),
                                agent_guard.tool_names().await,
                                agent_guard.last_upstream_provider(),
                            )
                        };

                        // Build MCP server list with tool counts
                        let mut mcp_map: std::collections::BTreeMap<String, usize> =
                            std::collections::BTreeMap::new();
                        for name in &tool_names {
                            if let Some(rest) = name.strip_prefix("mcp__") {
                                if let Some((server, _tool)) = rest.split_once("__") {
                                    *mcp_map.entry(server.to_string()).or_default() += 1;
                                }
                            }
                        }
                        let mcp_servers: Vec<String> = mcp_map
                            .into_iter()
                            .map(|(name, count)| format!("{}:{}", name, count))
                            .collect();

                        let skills = crate::skill::SkillRegistry::load()
                            .map(|r| r.list().iter().map(|s| s.name.clone()).collect())
                            .unwrap_or_default();

                        let (all_sessions, current_client_count) = {
                            let sessions_guard = sessions.read().await;
                            let all: Vec<String> = sessions_guard.keys().cloned().collect();
                            let count = *client_count.read().await;
                            (all, count)
                        };

                        let event = ServerEvent::History {
                            id,
                            session_id: session_id.clone(),
                            messages,
                            provider_name: Some(provider_name),
                            provider_model: Some(provider_model),
                            available_models,
                            available_model_routes,
                            mcp_servers,
                            skills,
                            total_tokens: None,
                            all_sessions,
                            client_count: Some(current_client_count),
                            is_canary: Some(is_canary),
                            server_version: Some(env!("JCODE_VERSION").to_string()),
                            server_name: Some(server_name.clone()),
                            server_icon: Some(server_icon.clone()),
                            server_has_update: Some(server_has_newer_binary()),
                            was_interrupted: if was_interrupted { Some(true) } else { None },
                            upstream_provider,
                        };
                        let json = encode_event(&event);
                        let mut w = writer.lock().await;
                        if w.write_all(json.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = client_event_tx.send(ServerEvent::Error {
                            id,
                            message: format!("Failed to restore session: {}", e),
                            retry_after_secs: None,
                        });
                    }
                }
            }

            Request::CycleModel { id, direction } => {
                let models = {
                    let agent_guard = agent.lock().await;
                    agent_guard.available_models()
                };
                if models.is_empty() {
                    let model = {
                        let agent_guard = agent.lock().await;
                        agent_guard.provider_model()
                    };
                    let _ = client_event_tx.send(ServerEvent::ModelChanged {
                        id,
                        model,
                        provider_name: None,
                        error: Some(
                            "Model switching is not available for this provider.".to_string(),
                        ),
                    });
                    continue;
                }

                let current = {
                    let agent_guard = agent.lock().await;
                    agent_guard.provider_model()
                };
                let current_index = models.iter().position(|m| *m == current).unwrap_or(0);
                let len = models.len();
                let next_index = if direction >= 0 {
                    (current_index + 1) % len
                } else {
                    (current_index + len - 1) % len
                };
                let next_model = models[next_index];

                let result = {
                    let mut agent_guard = agent.lock().await;
                    let result = agent_guard.set_model(next_model);
                    if result.is_ok() {
                        agent_guard.reset_provider_session();
                    }
                    result.map(|_| (agent_guard.provider_model(), agent_guard.provider_name()))
                };

                match result {
                    Ok((updated, pname)) => {
                        let _ = client_event_tx.send(ServerEvent::ModelChanged {
                            id,
                            model: updated,
                            provider_name: Some(pname),
                            error: None,
                        });
                    }
                    Err(e) => {
                        let _ = client_event_tx.send(ServerEvent::ModelChanged {
                            id,
                            model: current,
                            provider_name: None,
                            error: Some(e.to_string()),
                        });
                    }
                }
            }

            Request::SetPremiumMode { id, mode } => {
                use crate::provider::copilot::PremiumMode;
                let pm = match mode {
                    2 => PremiumMode::Zero,
                    1 => PremiumMode::OnePerSession,
                    _ => PremiumMode::Normal,
                };
                let agent_guard = agent.lock().await;
                agent_guard.set_premium_mode(pm);
                let label = match pm {
                    PremiumMode::Zero => "zero premium requests",
                    PremiumMode::OnePerSession => "one premium per session",
                    PremiumMode::Normal => "normal",
                };
                crate::logging::info(&format!("Server: premium mode set to {} ({})", mode, label));
                let _ = client_event_tx.send(ServerEvent::Ack { id });
            }

            Request::SetModel { id, model } => {
                let models = {
                    let agent_guard = agent.lock().await;
                    agent_guard.available_models()
                };
                if models.is_empty() {
                    let current = {
                        let agent_guard = agent.lock().await;
                        agent_guard.provider_model()
                    };
                    let _ = client_event_tx.send(ServerEvent::ModelChanged {
                        id,
                        model: current,
                        provider_name: None,
                        error: Some(
                            "Model switching is not available for this provider.".to_string(),
                        ),
                    });
                    continue;
                }

                let current = {
                    let agent_guard = agent.lock().await;
                    agent_guard.provider_model()
                };
                let result = {
                    let mut agent_guard = agent.lock().await;
                    let result = agent_guard.set_model(&model);
                    if result.is_ok() {
                        agent_guard.reset_provider_session();
                    }
                    result.map(|_| (agent_guard.provider_model(), agent_guard.provider_name()))
                };
                match result {
                    Ok((updated, pname)) => {
                        let _ = client_event_tx.send(ServerEvent::ModelChanged {
                            id,
                            model: updated,
                            provider_name: Some(pname),
                            error: None,
                        });
                    }
                    Err(e) => {
                        let _ = client_event_tx.send(ServerEvent::ModelChanged {
                            id,
                            model: current,
                            provider_name: None,
                            error: Some(e.to_string()),
                        });
                    }
                }
            }

            Request::NotifyAuthChanged { id } => {
                crate::auth::AuthStatus::invalidate_cache();
                let provider_clone = provider.clone();
                let client_event_tx_clone = client_event_tx.clone();
                let agent_clone = agent.clone();
                tokio::spawn(async move {
                    provider_clone.on_auth_changed();
                    // Wait for models to update via Bus event (with 10s timeout)
                    let mut bus_rx = crate::bus::Bus::global().subscribe();
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
                    loop {
                        let remaining =
                            deadline.saturating_duration_since(tokio::time::Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        tokio::select! {
                            event = bus_rx.recv() => {
                                if matches!(event, Ok(crate::bus::BusEvent::ModelsUpdated)) {
                                    break;
                                }
                            }
                            _ = tokio::time::sleep(remaining) => break,
                        }
                    }
                    let (models, model_routes) = {
                        let agent_guard = agent_clone.lock().await;
                        (
                            agent_guard.available_models_display(),
                            agent_guard.model_routes(),
                        )
                    };
                    let _ = client_event_tx_clone.send(ServerEvent::AvailableModelsUpdated {
                        available_models: models,
                        available_model_routes: model_routes,
                    });
                });
                let _ = client_event_tx.send(ServerEvent::Done { id });
            }

            Request::SwitchAnthropicAccount { id, label } => {
                // Apply account selection in server process so Anthropic provider
                // resolves credentials from the intended account.
                match crate::auth::claude::set_active_account(&label) {
                    Ok(()) => {
                        crate::auth::AuthStatus::invalidate_cache();

                        // Clear cached Anthropic credentials in this session's provider.
                        {
                            let agent_guard = agent.lock().await;
                            let provider = agent_guard.provider_handle();
                            drop(agent_guard);
                            provider.invalidate_credentials().await;
                        }

                        // Clear provider-side account/runtime availability caches.
                        crate::provider::clear_all_provider_unavailability_for_account();
                        crate::provider::clear_all_model_unavailability_for_account();

                        // Reset provider resume id so next turn starts clean on new account.
                        {
                            let mut agent_guard = agent.lock().await;
                            agent_guard.reset_provider_session();
                        }

                        // Refresh local usage snapshot asynchronously for the new active account.
                        tokio::spawn(async {
                            let _ = crate::usage::get().await;
                        });

                        let _ = client_event_tx.send(ServerEvent::Done { id });
                    }
                    Err(e) => {
                        let _ = client_event_tx.send(ServerEvent::Error {
                            id,
                            message: format!("Failed to switch Anthropic account: {}", e),
                            retry_after_secs: None,
                        });
                    }
                }
            }

            Request::SetFeature {
                id,
                feature,
                enabled,
            } => match feature {
                FeatureToggle::Memory => {
                    let mut agent_guard = agent.lock().await;
                    agent_guard.set_memory_enabled(enabled);
                    drop(agent_guard);
                    if !enabled {
                        crate::memory::clear_pending_memory(&client_session_id);
                    }
                    let _ = client_event_tx.send(ServerEvent::Done { id });
                }
                FeatureToggle::Swarm => {
                    if swarm_enabled == enabled {
                        let _ = client_event_tx.send(ServerEvent::Done { id });
                        continue;
                    }
                    swarm_enabled = enabled;

                    let (old_swarm_id, working_dir) = {
                        let mut members = swarm_members.write().await;
                        if let Some(member) = members.get_mut(&client_session_id) {
                            let old = member.swarm_id.clone();
                            let wd = member.working_dir.clone();
                            member.swarm_enabled = enabled;
                            if !enabled {
                                member.swarm_id = None;
                                member.role = "agent".to_string();
                            }
                            (old, wd)
                        } else {
                            (None, None)
                        }
                    };

                    if let Some(ref old_id) = old_swarm_id {
                        remove_session_from_swarm(
                            &client_session_id,
                            old_id,
                            &swarm_members,
                            &swarms_by_id,
                            &swarm_coordinators,
                            &swarm_plans,
                        )
                        .await;
                    }

                    if enabled {
                        let new_swarm_id = swarm_id_for_dir(working_dir);
                        if let Some(ref id) = new_swarm_id {
                            {
                                let mut swarms = swarms_by_id.write().await;
                                swarms
                                    .entry(id.clone())
                                    .or_insert_with(HashSet::new)
                                    .insert(client_session_id.clone());
                            }

                            let mut is_new_coordinator = false;
                            {
                                let mut coordinators = swarm_coordinators.write().await;
                                if coordinators.get(id).is_none() {
                                    coordinators.insert(id.clone(), client_session_id.clone());
                                    is_new_coordinator = true;
                                }
                            }

                            {
                                let mut members = swarm_members.write().await;
                                if let Some(member) = members.get_mut(&client_session_id) {
                                    member.swarm_id = Some(id.clone());
                                    member.role = if is_new_coordinator {
                                        "coordinator".to_string()
                                    } else {
                                        "agent".to_string()
                                    };
                                }
                            }

                            broadcast_swarm_status(id, &swarm_members, &swarms_by_id).await;

                            if is_new_coordinator {
                                let msg = "You are the coordinator for this swarm.".to_string();
                                let _ = client_event_tx.send(ServerEvent::Notification {
                                    from_session: client_session_id.clone(),
                                    from_name: friendly_name.clone(),
                                    notification_type: NotificationType::Message {
                                        scope: Some("swarm".to_string()),
                                        channel: None,
                                    },
                                    message: msg,
                                });
                            }
                        } else {
                            let _ = client_event_tx.send(ServerEvent::SwarmStatus {
                                members: Vec::new(),
                            });
                        }
                    } else {
                        let _ = client_event_tx.send(ServerEvent::SwarmStatus {
                            members: Vec::new(),
                        });
                    }

                    let _ = client_event_tx.send(ServerEvent::Done { id });
                }
            },

            Request::Split { id } => {
                // Clone the current session's messages into a new session
                let (new_session_id, new_session_name) = {
                    let agent_guard = agent.lock().await;
                    let parent_session_id = agent_guard.session_id().to_string();
                    let messages = agent_guard.messages().to_vec();
                    let working_dir = agent_guard.working_dir().map(|s| s.to_string());
                    let model = Some(agent_guard.provider_model());

                    // Create a new session with the same messages
                    let mut child = Session::create(Some(parent_session_id), None);
                    child.messages = messages;
                    child.working_dir = working_dir;
                    child.model = model;
                    child.status = crate::session::SessionStatus::Closed;

                    if let Err(e) = child.save() {
                        let _ = client_event_tx.send(ServerEvent::Error {
                            id,
                            message: format!("Failed to save split session: {}", e),
                            retry_after_secs: None,
                        });
                        continue;
                    }

                    let name = child.display_name().to_string();
                    (child.id.clone(), name)
                };

                let _ = client_event_tx.send(ServerEvent::SplitResponse {
                    id,
                    new_session_id,
                    new_session_name,
                });
            }

            Request::Compact { id } => {
                let agent = Arc::clone(&agent);
                let tx = client_event_tx.clone();
                tokio::spawn(async move {
                    let agent_guard = agent.lock().await;
                    let provider = agent_guard.provider_fork();
                    let compaction = agent_guard.registry().compaction();
                    let messages = agent_guard.provider_messages();
                    drop(agent_guard);

                    if !provider.supports_compaction() {
                        let _ = tx.send(ServerEvent::CompactResult {
                            id,
                            message: "Manual compaction is not available for this provider."
                                .to_string(),
                            success: false,
                        });
                        return;
                    }

                    let result = match compaction.try_write() {
                        Ok(mut manager) => {
                            let stats = manager.stats_with(&messages);
                            let status_msg = format!(
                                "**Context Status:**\n\
                                • Messages: {} (active), {} (total history)\n\
                                • Token usage: ~{}k (estimate ~{}k) / {}k ({:.1}%)\n\
                                • Has summary: {}\n\
                                • Compacting: {}",
                                stats.active_messages,
                                stats.total_turns,
                                stats.effective_tokens / 1000,
                                stats.token_estimate / 1000,
                                manager.token_budget() / 1000,
                                stats.context_usage * 100.0,
                                if stats.has_summary { "yes" } else { "no" },
                                if stats.is_compacting {
                                    "in progress..."
                                } else {
                                    "no"
                                }
                            );

                            match manager.force_compact_with(&messages, provider) {
                                Ok(()) => ServerEvent::CompactResult {
                                    id,
                                    message: format!(
                                        "{}\n\n✓ **Compaction started** — summarizing older messages in background.\n\
                                        The summary will be applied automatically when ready.",
                                        status_msg
                                    ),
                                    success: true,
                                },
                                Err(reason) => ServerEvent::CompactResult {
                                    id,
                                    message: format!(
                                        "{}\n\n⚠ **Cannot compact:** {}",
                                        status_msg, reason
                                    ),
                                    success: false,
                                },
                            }
                        }
                        Err(_) => ServerEvent::CompactResult {
                            id,
                            message: "⚠ Cannot access compaction manager (lock held)".to_string(),
                            success: false,
                        },
                    };
                    let _ = tx.send(result);
                });
            }

            // Agent-to-agent communication
            Request::AgentRegister { id, .. } => {
                let _ = client_event_tx.send(ServerEvent::Done { id });
            }

            Request::StdinResponse {
                id,
                request_id,
                input,
            } => {
                if let Some(tx) = stdin_responses.lock().await.remove(&request_id) {
                    let _ = tx.send(input);
                }
                let _ = client_event_tx.send(ServerEvent::Done { id });
            }

            Request::AgentTask { id, task, .. } => {
                // Process as a message on this client's agent
                update_member_status(
                    &client_session_id,
                    "running",
                    Some(truncate_detail(&task, 120)),
                    &swarm_members,
                    &swarms_by_id,
                    Some(&event_history),
                    Some(&event_counter),
                    Some(&swarm_event_tx),
                )
                .await;
                let result = process_message_streaming_mpsc(
                    Arc::clone(&agent),
                    &task,
                    vec![],
                    client_event_tx.clone(),
                )
                .await;
                match result {
                    Ok(()) => {
                        update_member_status(
                            &client_session_id,
                            "completed",
                            None,
                            &swarm_members,
                            &swarms_by_id,
                            Some(&event_history),
                            Some(&event_counter),
                            Some(&swarm_event_tx),
                        )
                        .await;
                        let _ = client_event_tx.send(ServerEvent::Done { id });
                    }
                    Err(e) => {
                        update_member_status(
                            &client_session_id,
                            "failed",
                            Some(truncate_detail(&e.to_string(), 120)),
                            &swarm_members,
                            &swarms_by_id,
                            Some(&event_history),
                            Some(&event_counter),
                            Some(&swarm_event_tx),
                        )
                        .await;
                        let retry_after_secs = e
                            .downcast_ref::<StreamError>()
                            .and_then(|se| se.retry_after_secs);
                        let _ = client_event_tx.send(ServerEvent::Error {
                            id,
                            message: e.to_string(),
                            retry_after_secs,
                        });
                    }
                }
            }

            Request::AgentCapabilities { id } => {
                let _ = client_event_tx.send(ServerEvent::Done { id });
            }

            Request::AgentContext { id } => {
                let _ = client_event_tx.send(ServerEvent::Done { id });
            }

            // === Agent communication ===
            Request::CommShare {
                id,
                session_id: req_session_id,
                key,
                value,
            } => {
                // Find the swarm id for this session
                let swarm_id = {
                    let members = swarm_members.read().await;
                    members
                        .get(&req_session_id)
                        .and_then(|m| m.swarm_id.clone())
                };

                if let Some(swarm_id) = swarm_id {
                    let friendly_name = {
                        let members = swarm_members.read().await;
                        members
                            .get(&req_session_id)
                            .and_then(|m| m.friendly_name.clone())
                    };

                    // Store the shared context
                    {
                        let mut ctx = shared_context.write().await;
                        let swarm_ctx = ctx.entry(swarm_id.clone()).or_insert_with(HashMap::new);
                        let now = Instant::now();
                        // Preserve created_at if updating existing context
                        let created_at = swarm_ctx.get(&key).map(|c| c.created_at).unwrap_or(now);
                        swarm_ctx.insert(
                            key.clone(),
                            SharedContext {
                                key: key.clone(),
                                value: value.clone(),
                                from_session: req_session_id.clone(),
                                from_name: friendly_name.clone(),
                                created_at,
                                updated_at: now,
                            },
                        );
                    }

                    // Notify other swarm members
                    let swarm_session_ids: Vec<String> = {
                        let swarms = swarms_by_id.read().await;
                        swarms
                            .get(&swarm_id)
                            .map(|s| s.iter().cloned().collect())
                            .unwrap_or_default()
                    };

                    let members = swarm_members.read().await;
                    for sid in &swarm_session_ids {
                        if sid != &req_session_id {
                            if let Some(member) = members.get(sid) {
                                let _ = member.event_tx.send(ServerEvent::Notification {
                                    from_session: req_session_id.clone(),
                                    from_name: friendly_name.clone(),
                                    notification_type: NotificationType::SharedContext {
                                        key: key.clone(),
                                        value: value.clone(),
                                    },
                                    message: format!("Shared context: {} = {}", key, value),
                                });
                            }
                        }
                    }
                    record_swarm_event(
                        &event_history,
                        &event_counter,
                        &swarm_event_tx,
                        req_session_id.clone(),
                        friendly_name.clone(),
                        Some(swarm_id.clone()),
                        SwarmEventType::ContextUpdate {
                            swarm_id: swarm_id.clone(),
                            key: key.clone(),
                        },
                    )
                    .await;
                    let _ = client_event_tx.send(ServerEvent::Done { id });
                } else {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: "Not in a swarm. Use a git repository to enable swarm features."
                            .to_string(),
                        retry_after_secs: None,
                    });
                }
            }

            Request::CommRead {
                id,
                session_id: req_session_id,
                key,
            } => {
                // Find the swarm id for this session
                let swarm_id = {
                    let members = swarm_members.read().await;
                    members
                        .get(&req_session_id)
                        .and_then(|m| m.swarm_id.clone())
                };

                let entries = if let Some(swarm_id) = swarm_id {
                    let ctx = shared_context.read().await;
                    if let Some(swarm_ctx) = ctx.get(&swarm_id) {
                        let entries: Vec<ContextEntry> = if let Some(k) = key {
                            // Get specific key
                            swarm_ctx
                                .get(&k)
                                .map(|c| {
                                    vec![ContextEntry {
                                        key: c.key.clone(),
                                        value: c.value.clone(),
                                        from_session: c.from_session.clone(),
                                        from_name: c.from_name.clone(),
                                    }]
                                })
                                .unwrap_or_default()
                        } else {
                            // Get all
                            swarm_ctx
                                .values()
                                .map(|c| ContextEntry {
                                    key: c.key.clone(),
                                    value: c.value.clone(),
                                    from_session: c.from_session.clone(),
                                    from_name: c.from_name.clone(),
                                })
                                .collect()
                        };
                        entries
                    } else {
                        vec![]
                    }
                } else {
                    vec![]
                };

                let _ = client_event_tx.send(ServerEvent::CommContext { id, entries });
            }

            Request::CommMessage {
                id,
                from_session,
                message,
                to_session,
                channel,
            } => {
                // Find the swarm id for this session
                let swarm_id = {
                    let members = swarm_members.read().await;
                    members.get(&from_session).and_then(|m| m.swarm_id.clone())
                };

                if let Some(swarm_id) = swarm_id {
                    let friendly_name = {
                        let members = swarm_members.read().await;
                        members
                            .get(&from_session)
                            .and_then(|m| m.friendly_name.clone())
                    };

                    let swarm_session_ids: Vec<String> = {
                        let swarms = swarms_by_id.read().await;
                        swarms
                            .get(&swarm_id)
                            .map(|s| s.iter().cloned().collect())
                            .unwrap_or_default()
                    };

                    // Validate DM recipient exists in swarm
                    if let Some(ref target) = to_session {
                        if !swarm_session_ids.contains(target) {
                            let _ = client_event_tx.send(ServerEvent::Error {
                                id,
                                message: format!("DM failed: session '{}' not in swarm", target),
                                retry_after_secs: None,
                            });
                            continue;
                        }
                    }

                    let scope = if to_session.is_some() {
                        "dm"
                    } else if channel.is_some() {
                        "channel"
                    } else {
                        "broadcast"
                    };

                    let members = swarm_members.read().await;
                    let sessions = sessions.read().await;

                    let target_sessions: Vec<String> = if let Some(target) = to_session.clone() {
                        vec![target]
                    } else if let Some(ref ch) = channel {
                        // Channel message: check subscriptions
                        let subs = channel_subscriptions.read().await;
                        if let Some(channel_subs) = subs.get(&swarm_id).and_then(|s| s.get(ch)) {
                            // Send only to subscribed members (excluding sender)
                            channel_subs
                                .iter()
                                .filter(|sid| *sid != &from_session)
                                .cloned()
                                .collect()
                        } else {
                            // No subscriptions for this channel: broadcast to all
                            swarm_session_ids
                                .iter()
                                .filter(|sid| *sid != &from_session)
                                .cloned()
                                .collect()
                        }
                    } else {
                        swarm_session_ids
                            .iter()
                            .filter(|sid| *sid != &from_session)
                            .cloned()
                            .collect()
                    };

                    for sid in &target_sessions {
                        if !swarm_session_ids.contains(sid) {
                            continue;
                        }
                        if let Some(member) = members.get(sid) {
                            let from_label = friendly_name
                                .as_deref()
                                .unwrap_or(&from_session[..8.min(from_session.len())]);
                            let scope_label = match (scope, channel.as_deref()) {
                                ("channel", Some(ch)) => format!("#{}", ch),
                                ("dm", _) => "DM".to_string(),
                                _ => "broadcast".to_string(),
                            };
                            let notification_msg =
                                format!("{} from {}: {}", scope_label, from_label, message);
                            let _ = member.event_tx.send(ServerEvent::Notification {
                                from_session: from_session.clone(),
                                from_name: friendly_name.clone(),
                                notification_type: NotificationType::Message {
                                    scope: Some(scope.to_string()),
                                    channel: channel.clone(),
                                },
                                message: notification_msg.clone(),
                            });

                            // Also push to the agent's pending alerts
                            if let Some(agent) = sessions.get(sid) {
                                if let Ok(agent) = agent.try_lock() {
                                    agent.queue_soft_interrupt(notification_msg.clone(), false);
                                }
                            }
                        }
                    }
                    let scope_value = if scope == "channel" {
                        format!("#{}", channel.clone().unwrap_or_default())
                    } else {
                        scope.to_string()
                    };
                    record_swarm_event(
                        &event_history,
                        &event_counter,
                        &swarm_event_tx,
                        from_session.clone(),
                        friendly_name.clone(),
                        Some(swarm_id.clone()),
                        SwarmEventType::Notification {
                            notification_type: scope_value,
                            message: truncate_detail(&message, 220),
                        },
                    )
                    .await;
                    let _ = client_event_tx.send(ServerEvent::Done { id });
                } else {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: "Not in a swarm. Use a git repository to enable swarm features."
                            .to_string(),
                        retry_after_secs: None,
                    });
                }
            }

            Request::CommList {
                id,
                session_id: req_session_id,
            } => {
                // Find the swarm id for this session
                let swarm_id = {
                    let members = swarm_members.read().await;
                    members
                        .get(&req_session_id)
                        .and_then(|m| m.swarm_id.clone())
                };

                if let Some(swarm_id) = swarm_id {
                    let swarm_session_ids: Vec<String> = {
                        let swarms = swarms_by_id.read().await;
                        swarms
                            .get(&swarm_id)
                            .map(|s| s.iter().cloned().collect())
                            .unwrap_or_default()
                    };

                    let members = swarm_members.read().await;
                    let touches = file_touches.read().await;

                    let member_list: Vec<AgentInfo> = swarm_session_ids
                        .iter()
                        .filter_map(|sid| {
                            members.get(sid).map(|m| {
                                // Get files this member has touched
                                let files: Vec<String> = touches
                                    .iter()
                                    .filter_map(|(path, accesses)| {
                                        if accesses.iter().any(|a| &a.session_id == sid) {
                                            Some(path.display().to_string())
                                        } else {
                                            None
                                        }
                                    })
                                    .collect();

                                AgentInfo {
                                    session_id: sid.clone(),
                                    friendly_name: m.friendly_name.clone(),
                                    files_touched: files,
                                    role: Some(m.role.clone()),
                                }
                            })
                        })
                        .collect();

                    let _ = client_event_tx.send(ServerEvent::CommMembers {
                        id,
                        members: member_list,
                    });
                } else {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: "Not in a swarm. Use a git repository to enable swarm features."
                            .to_string(),
                        retry_after_secs: None,
                    });
                }
            }

            Request::CommProposePlan {
                id,
                session_id: req_session_id,
                items,
            } => {
                let swarm_id = {
                    let members = swarm_members.read().await;
                    members
                        .get(&req_session_id)
                        .and_then(|m| m.swarm_id.clone())
                };

                let swarm_id = match swarm_id.as_ref() {
                    Some(sid) => sid.clone(),
                    None => {
                        let _ = client_event_tx.send(ServerEvent::Error {
                            id,
                            message: "Not in a swarm.".to_string(),
                            retry_after_secs: None,
                        });
                        continue;
                    }
                };

                let (from_name, coordinator_id) = {
                    let members = swarm_members.read().await;
                    let from_name = members
                        .get(&req_session_id)
                        .and_then(|m| m.friendly_name.clone());
                    let coordinators = swarm_coordinators.read().await;
                    let coordinator_id = coordinators.get(&swarm_id).cloned();
                    (from_name, coordinator_id)
                };
                let from_label = from_name
                    .clone()
                    .unwrap_or_else(|| req_session_id.chars().take(8).collect());

                let Some(coordinator_id) = coordinator_id else {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: "No coordinator for this swarm.".to_string(),
                        retry_after_secs: None,
                    });
                    continue;
                };

                if coordinator_id == req_session_id {
                    let (version, participant_ids) = {
                        let mut plans = swarm_plans.write().await;
                        let vp = plans
                            .entry(swarm_id.clone())
                            .or_insert_with(VersionedPlan::new);
                        vp.participants.insert(req_session_id.clone());
                        for item in &items {
                            if let Some(owner) = &item.assigned_to {
                                vp.participants.insert(owner.clone());
                            }
                        }
                        vp.items = items.clone();
                        vp.version += 1;
                        (vp.version, vp.participants.clone())
                    };

                    let members = swarm_members.read().await;
                    let agent_sessions = sessions.read().await;
                    let notification_msg = format!(
                        "Plan updated by {} ({} items, v{})",
                        from_label,
                        items.len(),
                        version
                    );
                    for sid in participant_ids {
                        if sid == req_session_id {
                            continue;
                        }
                        if let Some(member) = members.get(&sid) {
                            let _ = member.event_tx.send(ServerEvent::Notification {
                                from_session: req_session_id.clone(),
                                from_name: from_name.clone(),
                                notification_type: NotificationType::Message {
                                    scope: Some("plan".to_string()),
                                    channel: None,
                                },
                                message: notification_msg.clone(),
                            });
                        }
                        if let Some(agent) = agent_sessions.get(&sid) {
                            if let Ok(agent) = agent.try_lock() {
                                agent.queue_soft_interrupt(notification_msg.clone(), false);
                            }
                        }
                    }

                    broadcast_swarm_plan(
                        &swarm_id,
                        Some("coordinator_direct_update".to_string()),
                        &swarm_plans,
                        &swarm_members,
                        &swarms_by_id,
                    )
                    .await;
                    record_swarm_event(
                        &event_history,
                        &event_counter,
                        &swarm_event_tx,
                        req_session_id.clone(),
                        from_name.clone(),
                        Some(swarm_id.clone()),
                        SwarmEventType::PlanUpdate {
                            swarm_id: swarm_id.clone(),
                            item_count: items.len(),
                        },
                    )
                    .await;

                    let _ = client_event_tx.send(ServerEvent::Done { id });
                    continue;
                }

                let proposal_key = format!("plan_proposal:{}", req_session_id);
                let proposal_value =
                    serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string());
                {
                    let mut ctx = shared_context.write().await;
                    let swarm_ctx = ctx.entry(swarm_id.clone()).or_insert_with(HashMap::new);
                    let now = Instant::now();
                    swarm_ctx.insert(
                        proposal_key.clone(),
                        SharedContext {
                            key: proposal_key.clone(),
                            value: proposal_value,
                            from_session: req_session_id.clone(),
                            from_name: from_name.clone(),
                            created_at: now,
                            updated_at: now,
                        },
                    );
                }
                record_swarm_event(
                    &event_history,
                    &event_counter,
                    &swarm_event_tx,
                    req_session_id.clone(),
                    from_name.clone(),
                    Some(swarm_id.clone()),
                    SwarmEventType::PlanProposal {
                        swarm_id: swarm_id.clone(),
                        proposer_session: req_session_id.clone(),
                        item_count: items.len(),
                    },
                )
                .await;

                let summary = summarize_plan_items(&items, 3);
                let notification_msg = format!(
                    "Plan proposal from {} ({} items). Summary: {}. Review with communicate read key '{}'.",
                    from_label,
                    items.len(),
                    summary,
                    proposal_key
                );

                let members = swarm_members.read().await;
                let agent_sessions = sessions.read().await;
                if let Some(member) = members.get(&coordinator_id) {
                    let _ = member.event_tx.send(ServerEvent::Notification {
                        from_session: req_session_id.clone(),
                        from_name: from_name.clone(),
                        notification_type: NotificationType::Message {
                            scope: Some("plan_proposal".to_string()),
                            channel: None,
                        },
                        message: notification_msg.clone(),
                    });
                    let _ = member.event_tx.send(ServerEvent::SwarmPlanProposal {
                        swarm_id: swarm_id.clone(),
                        proposer_session: req_session_id.clone(),
                        proposer_name: from_name.clone(),
                        items: items.clone(),
                        summary: summary.clone(),
                        proposal_key: proposal_key.clone(),
                    });
                }
                if let Some(agent) = agent_sessions.get(&coordinator_id) {
                    if let Ok(agent) = agent.try_lock() {
                        agent.queue_soft_interrupt(notification_msg.clone(), false);
                    }
                }

                if let Some(member) = members.get(&req_session_id) {
                    let _ = member.event_tx.send(ServerEvent::Notification {
                        from_session: req_session_id.clone(),
                        from_name: from_name.clone(),
                        notification_type: NotificationType::Message {
                            scope: Some("plan_proposal".to_string()),
                            channel: None,
                        },
                        message: "Plan proposal sent to coordinator (not yet applied).".to_string(),
                    });
                }
                if let Some(agent) = agent_sessions.get(&req_session_id) {
                    if let Ok(agent) = agent.try_lock() {
                        agent.queue_soft_interrupt(
                            "Plan proposal sent to coordinator (not yet applied).".to_string(),
                            false,
                        );
                    }
                }

                let _ = client_event_tx.send(ServerEvent::Done { id });
            }

            Request::CommApprovePlan {
                id,
                session_id: req_session_id,
                proposer_session,
            } => {
                // Verify the requester is the coordinator
                let (_swarm_id, is_coordinator) = {
                    let members = swarm_members.read().await;
                    let swarm_id = members
                        .get(&req_session_id)
                        .and_then(|m| m.swarm_id.clone());
                    let is_coordinator = if let Some(ref sid) = swarm_id {
                        let coordinators = swarm_coordinators.read().await;
                        coordinators
                            .get(sid)
                            .map(|c| c == &req_session_id)
                            .unwrap_or(false)
                    } else {
                        false
                    };
                    (swarm_id, is_coordinator)
                };

                if !is_coordinator {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: "Only the coordinator can approve plan proposals.".to_string(),
                        retry_after_secs: None,
                    });
                    continue;
                }

                let swarm_id = match swarm_id.as_ref() {
                    Some(sid) => sid.clone(),
                    None => {
                        let _ = client_event_tx.send(ServerEvent::Error {
                            id,
                            message: "Not in a swarm.".to_string(),
                            retry_after_secs: None,
                        });
                        continue;
                    }
                };

                // Read proposal from shared context
                let proposal_key = format!("plan_proposal:{}", proposer_session);
                let proposal_value = {
                    let ctx = shared_context.read().await;
                    ctx.get(&swarm_id)
                        .and_then(|sc| sc.get(&proposal_key))
                        .map(|c| c.value.clone())
                };

                let proposal = match proposal_value {
                    Some(v) => v,
                    None => {
                        let _ = client_event_tx.send(ServerEvent::Error {
                            id,
                            message: format!(
                                "No pending plan proposal from session '{}'",
                                proposer_session
                            ),
                            retry_after_secs: None,
                        });
                        continue;
                    }
                };

                // Parse and apply to swarm_plans
                if let Ok(items) = serde_json::from_str::<Vec<PlanItem>>(&proposal) {
                    let participant_ids = {
                        let mut plans = swarm_plans.write().await;
                        let vp = plans
                            .entry(swarm_id.clone())
                            .or_insert_with(VersionedPlan::new);
                        vp.items.extend(items.clone());
                        vp.version += 1;
                        vp.participants.insert(req_session_id.clone());
                        vp.participants.insert(proposer_session.clone());
                        for item in &items {
                            if let Some(owner) = &item.assigned_to {
                                vp.participants.insert(owner.clone());
                            }
                        }
                        vp.participants.clone()
                    };

                    // Remove proposal from shared context
                    {
                        let mut ctx = shared_context.write().await;
                        if let Some(swarm_ctx) = ctx.get_mut(&swarm_id) {
                            swarm_ctx.remove(&proposal_key);
                        }
                    }

                    broadcast_swarm_plan(
                        &swarm_id,
                        Some("proposal_approved".to_string()),
                        &swarm_plans,
                        &swarm_members,
                        &swarms_by_id,
                    )
                    .await;
                    record_swarm_event(
                        &event_history,
                        &event_counter,
                        &swarm_event_tx,
                        req_session_id.clone(),
                        None,
                        Some(swarm_id.clone()),
                        SwarmEventType::PlanUpdate {
                            swarm_id: swarm_id.clone(),
                            item_count: items.len(),
                        },
                    )
                    .await;

                    let coordinator_name = {
                        let members = swarm_members.read().await;
                        members
                            .get(&req_session_id)
                            .and_then(|m| m.friendly_name.clone())
                    };

                    let members = swarm_members.read().await;
                    let agent_sessions = sessions.read().await;
                    for sid in participant_ids {
                        if let Some(member) = members.get(&sid) {
                            let msg = format!(
                                "Plan approved by coordinator: {} items added from {}",
                                items.len(),
                                proposer_session
                            );
                            let _ = member.event_tx.send(ServerEvent::Notification {
                                from_session: req_session_id.clone(),
                                from_name: coordinator_name.clone(),
                                notification_type: NotificationType::Message {
                                    scope: Some("plan".to_string()),
                                    channel: None,
                                },
                                message: msg.clone(),
                            });

                            if let Some(agent) = agent_sessions.get(&sid) {
                                if let Ok(agent) = agent.try_lock() {
                                    agent.queue_soft_interrupt(msg.clone(), false);
                                }
                            }
                        }
                    }
                }

                let _ = client_event_tx.send(ServerEvent::Done { id });
            }

            Request::CommRejectPlan {
                id,
                session_id: req_session_id,
                proposer_session,
                reason,
            } => {
                // Verify the requester is the coordinator
                let (swarm_id, is_coordinator) = {
                    let members = swarm_members.read().await;
                    let swarm_id = members
                        .get(&req_session_id)
                        .and_then(|m| m.swarm_id.clone());
                    let is_coordinator = if let Some(ref sid) = swarm_id {
                        let coordinators = swarm_coordinators.read().await;
                        coordinators
                            .get(sid)
                            .map(|c| c == &req_session_id)
                            .unwrap_or(false)
                    } else {
                        false
                    };
                    (swarm_id, is_coordinator)
                };

                if !is_coordinator {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: "Only the coordinator can reject plan proposals.".to_string(),
                        retry_after_secs: None,
                    });
                    continue;
                }

                let swarm_id = match swarm_id {
                    Some(sid) => sid,
                    None => {
                        let _ = client_event_tx.send(ServerEvent::Error {
                            id,
                            message: "Not in a swarm.".to_string(),
                            retry_after_secs: None,
                        });
                        continue;
                    }
                };

                // Check if proposal exists
                let proposal_key = format!("plan_proposal:{}", proposer_session);
                let proposal_exists = {
                    let ctx = shared_context.read().await;
                    ctx.get(&swarm_id)
                        .and_then(|sc| sc.get(&proposal_key))
                        .is_some()
                };

                if !proposal_exists {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: format!(
                            "No pending plan proposal from session '{}'",
                            proposer_session
                        ),
                        retry_after_secs: None,
                    });
                    continue;
                }

                // Remove proposal from shared context
                {
                    let mut ctx = shared_context.write().await;
                    if let Some(swarm_ctx) = ctx.get_mut(&swarm_id) {
                        swarm_ctx.remove(&proposal_key);
                    }
                }

                // Notify the proposer
                let coordinator_name = {
                    let members = swarm_members.read().await;
                    members
                        .get(&req_session_id)
                        .and_then(|m| m.friendly_name.clone())
                };

                let members = swarm_members.read().await;
                let agent_sessions = sessions.read().await;
                if let Some(member) = members.get(&proposer_session) {
                    let reason_msg = reason
                        .as_ref()
                        .map(|r| format!(": {}", r))
                        .unwrap_or_default();
                    let msg = format!(
                        "Your plan proposal was rejected by the coordinator{}",
                        reason_msg
                    );
                    let _ = member.event_tx.send(ServerEvent::Notification {
                        from_session: req_session_id.clone(),
                        from_name: coordinator_name.clone(),
                        notification_type: NotificationType::Message {
                            scope: Some("dm".to_string()),
                            channel: None,
                        },
                        message: msg.clone(),
                    });

                    if let Some(agent) = agent_sessions.get(&proposer_session) {
                        if let Ok(agent) = agent.try_lock() {
                            agent.queue_soft_interrupt(msg, false);
                        }
                    }
                }
                record_swarm_event(
                    &event_history,
                    &event_counter,
                    &swarm_event_tx,
                    req_session_id.clone(),
                    coordinator_name,
                    Some(swarm_id.clone()),
                    SwarmEventType::Notification {
                        notification_type: "plan_rejected".to_string(),
                        message: proposer_session.clone(),
                    },
                )
                .await;

                let _ = client_event_tx.send(ServerEvent::Done { id });
            }

            Request::CommSpawn {
                id,
                session_id: req_session_id,
                working_dir,
                initial_message,
            } => {
                // Verify the requester is the coordinator
                let (swarm_id, is_coordinator) = {
                    let members = swarm_members.read().await;
                    let swarm_id = members
                        .get(&req_session_id)
                        .and_then(|m| m.swarm_id.clone());
                    let is_coordinator = if let Some(ref sid) = swarm_id {
                        let coordinators = swarm_coordinators.read().await;
                        coordinators
                            .get(sid)
                            .map(|c| c == &req_session_id)
                            .unwrap_or(false)
                    } else {
                        false
                    };
                    (swarm_id, is_coordinator)
                };

                if !is_coordinator {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: "Only the coordinator can spawn new agents.".to_string(),
                        retry_after_secs: None,
                    });
                    continue;
                }
                let swarm_id = match swarm_id {
                    Some(sid) => sid,
                    None => {
                        let _ = client_event_tx.send(ServerEvent::Error {
                            id,
                            message: "Not in a swarm.".to_string(),
                            retry_after_secs: None,
                        });
                        continue;
                    }
                };

                let cmd = if let Some(ref dir) = working_dir {
                    format!("create_session:{}", dir)
                } else {
                    "create_session".to_string()
                };
                let coordinator_model = {
                    let agent_sessions = sessions.read().await;
                    agent_sessions.get(&req_session_id).and_then(|agent| {
                        agent
                            .try_lock()
                            .ok()
                            .map(|agent_guard| agent_guard.provider_model())
                    })
                };

                match create_headless_session(
                    &sessions,
                    &global_session_id,
                    &provider_template,
                    &cmd,
                    &swarm_members,
                    &swarms_by_id,
                    &swarm_coordinators,
                    &swarm_plans,
                    coordinator_model,
                    Some(Arc::clone(&mcp_pool)),
                )
                .await
                {
                    Ok(result_json) => {
                        let new_session_id =
                            serde_json::from_str::<serde_json::Value>(&result_json)
                                .ok()
                                .and_then(|v| {
                                    v.get("session_id")
                                        .and_then(|s| s.as_str())
                                        .map(|s| s.to_string())
                                })
                                .unwrap_or_default();

                        {
                            let mut plans = swarm_plans.write().await;
                            if let Some(vp) = plans.get_mut(&swarm_id) {
                                if !vp.items.is_empty() || !vp.participants.is_empty() {
                                    vp.participants.insert(req_session_id.clone());
                                    vp.participants.insert(new_session_id.clone());
                                }
                            }
                        }

                        broadcast_swarm_plan(
                            &swarm_id,
                            Some("participant_spawned".to_string()),
                            &swarm_plans,
                            &swarm_members,
                            &swarms_by_id,
                        )
                        .await;
                        record_swarm_event_for_session(
                            &new_session_id,
                            SwarmEventType::MemberChange {
                                action: "joined".to_string(),
                            },
                            &swarm_members,
                            &event_history,
                            &event_counter,
                            &swarm_event_tx,
                        )
                        .await;
                        // Spawn a background task to process the initial message
                        // on this headless agent. Without this, the agent sits idle forever.
                        if let Some(initial_msg) = initial_message {
                            let agent_arc = {
                                let agent_sessions = sessions.read().await;
                                agent_sessions.get(&new_session_id).cloned()
                            };
                            if let Some(agent_arc) = agent_arc {
                                let sid_clone = new_session_id.clone();
                                let swarm_members2 = Arc::clone(&swarm_members);
                                let swarms_by_id2 = Arc::clone(&swarms_by_id);
                                let event_history2 = Arc::clone(&event_history);
                                let event_counter2 = Arc::clone(&event_counter);
                                let swarm_event_tx2 = swarm_event_tx.clone();
                                tokio::spawn(async move {
                                    update_member_status(
                                        &sid_clone,
                                        "running",
                                        Some(truncate_detail(&initial_msg, 120)),
                                        &swarm_members2,
                                        &swarms_by_id2,
                                        Some(&event_history2),
                                        Some(&event_counter2),
                                        Some(&swarm_event_tx2),
                                    )
                                    .await;
                                    let (drain_tx, mut drain_rx) =
                                        tokio::sync::mpsc::unbounded_channel::<ServerEvent>();
                                    tokio::spawn(async move {
                                        while drain_rx.recv().await.is_some() {}
                                    });
                                    let result = process_message_streaming_mpsc(
                                        Arc::clone(&agent_arc),
                                        &initial_msg,
                                        vec![],
                                        drain_tx,
                                    )
                                    .await;
                                    let (new_status, new_detail) = match result {
                                        Ok(()) => ("ready", None),
                                        Err(ref e) => {
                                            ("failed", Some(truncate_detail(&e.to_string(), 120)))
                                        }
                                    };
                                    update_member_status(
                                        &sid_clone,
                                        new_status,
                                        new_detail,
                                        &swarm_members2,
                                        &swarms_by_id2,
                                        Some(&event_history2),
                                        Some(&event_counter2),
                                        Some(&swarm_event_tx2),
                                    )
                                    .await;
                                });
                            }
                        }

                        let _ = client_event_tx.send(ServerEvent::CommSpawnResponse {
                            id,
                            session_id: req_session_id.clone(),
                            new_session_id,
                        });
                    }
                    Err(e) => {
                        let _ = client_event_tx.send(ServerEvent::Error {
                            id,
                            message: format!("Failed to spawn agent: {}", e),
                            retry_after_secs: None,
                        });
                    }
                }
            }

            Request::CommStop {
                id,
                session_id: req_session_id,
                target_session,
            } => {
                // Verify the requester is the coordinator
                let (swarm_id, is_coordinator) = {
                    let members = swarm_members.read().await;
                    let swarm_id = members
                        .get(&req_session_id)
                        .and_then(|m| m.swarm_id.clone());
                    let is_coordinator = if let Some(ref sid) = swarm_id {
                        let coordinators = swarm_coordinators.read().await;
                        coordinators
                            .get(sid)
                            .map(|c| c == &req_session_id)
                            .unwrap_or(false)
                    } else {
                        false
                    };
                    (swarm_id, is_coordinator)
                };

                if !is_coordinator {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: "Only the coordinator can stop agents.".to_string(),
                        retry_after_secs: None,
                    });
                    continue;
                }

                // Remove session and trigger final memory extraction
                let mut sessions_guard = sessions.write().await;
                let removed_agent = sessions_guard.remove(&target_session);
                drop(sessions_guard);
                if let Some(agent_arc) = removed_agent {
                    {
                        let agent = agent_arc.lock().await;
                        let memory_enabled = agent.memory_enabled();
                        let transcript = if memory_enabled {
                            Some(agent.build_transcript_for_extraction())
                        } else {
                            None
                        };
                        let sid = target_session.clone();
                        drop(agent);
                        if let Some(transcript) = transcript {
                            crate::memory_agent::trigger_final_extraction(transcript, sid);
                        }
                    }
                    // Clean up swarm membership
                    let (removed_swarm_id, removed_name) = {
                        let mut members = swarm_members.write().await;
                        if let Some(member) = members.remove(&target_session) {
                            (member.swarm_id, member.friendly_name)
                        } else {
                            (None, None)
                        }
                    };
                    if let Some(ref sid) = removed_swarm_id {
                        record_swarm_event(
                            &event_history,
                            &event_counter,
                            &swarm_event_tx,
                            target_session.clone(),
                            removed_name.clone(),
                            Some(sid.clone()),
                            SwarmEventType::MemberChange {
                                action: "left".to_string(),
                            },
                        )
                        .await;
                        remove_plan_participant(sid, &target_session, &swarm_plans).await;
                        let mut swarms = swarms_by_id.write().await;
                        if let Some(swarm) = swarms.get_mut(sid) {
                            swarm.remove(&target_session);
                            if swarm.is_empty() {
                                swarms.remove(sid);
                            }
                        }
                        // Handle coordinator re-election if needed
                        let was_coordinator = {
                            let coordinators = swarm_coordinators.read().await;
                            coordinators
                                .get(sid)
                                .map(|c| c == &target_session)
                                .unwrap_or(false)
                        };
                        if was_coordinator {
                            let new_coordinator = {
                                let swarms = swarms_by_id.read().await;
                                swarms.get(sid).and_then(|s| s.iter().min().cloned())
                            };
                            let mut coordinators = swarm_coordinators.write().await;
                            coordinators.remove(sid);
                            if let Some(ref new_id) = new_coordinator {
                                coordinators.insert(sid.clone(), new_id.clone());
                                let mut members = swarm_members.write().await;
                                if let Some(m) = members.get_mut(new_id) {
                                    m.role = "coordinator".to_string();
                                }
                                let mut plans = swarm_plans.write().await;
                                if let Some(vp) = plans.get_mut(sid) {
                                    vp.participants.insert(new_id.clone());
                                }
                            }
                        }
                        broadcast_swarm_status(sid, &swarm_members, &swarms_by_id).await;
                    }
                    let _ = client_event_tx.send(ServerEvent::Done { id });
                } else {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: format!("Unknown session '{}'", target_session),
                        retry_after_secs: None,
                    });
                }
            }

            Request::CommAssignRole {
                id,
                session_id: req_session_id,
                target_session,
                role,
            } => {
                // Verify the requester is the coordinator.
                // Exception: a session may self-promote to coordinator if the current
                // coordinator has no active agent session (disconnected/stale).
                let (swarm_id, is_coordinator) = {
                    let members = swarm_members.read().await;
                    let swarm_id = members
                        .get(&req_session_id)
                        .and_then(|m| m.swarm_id.clone());

                    let is_coordinator = if let Some(ref sid) = swarm_id {
                        let coordinators = swarm_coordinators.read().await;
                        let current_coordinator = coordinators.get(sid).cloned();
                        drop(coordinators);

                        if current_coordinator.as_deref() == Some(req_session_id.as_str()) {
                            // Already the coordinator
                            true
                        } else if role == "coordinator" && target_session == req_session_id {
                            // Self-promotion: allowed if current coordinator's event channel
                            // is closed (zombie from a previous server instance) or has no
                            // active agent session.
                            drop(members);
                            let coordinator_is_zombie =
                                if let Some(ref coord_id) = current_coordinator {
                                    let channel_closed = {
                                        let m = swarm_members.read().await;
                                        m.get(coord_id)
                                            .map(|mb| mb.event_tx.is_closed())
                                            .unwrap_or(true)
                                    };
                                    let not_in_sessions =
                                        !sessions.read().await.contains_key(coord_id);
                                    channel_closed || not_in_sessions
                                } else {
                                    true
                                };
                            coordinator_is_zombie
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    (swarm_id, is_coordinator)
                };

                if !is_coordinator {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: "Only the coordinator can assign roles. (Tip: if the coordinator has disconnected, use assign_role with target_session set to your own session ID to self-promote.)".to_string(),
                        retry_after_secs: None,
                    });
                    continue;
                }

                let swarm_id = match swarm_id {
                    Some(sid) => sid,
                    None => {
                        let _ = client_event_tx.send(ServerEvent::Error {
                            id,
                            message: "Not in a swarm.".to_string(),
                            retry_after_secs: None,
                        });
                        continue;
                    }
                };

                // Update role on target member
                {
                    let mut members = swarm_members.write().await;
                    if let Some(m) = members.get_mut(&target_session) {
                        m.role = role.clone();
                    } else {
                        let _ = client_event_tx.send(ServerEvent::Error {
                            id,
                            message: format!("Unknown session '{}'", target_session),
                            retry_after_secs: None,
                        });
                        continue;
                    }
                }

                // If assigning coordinator, update swarm_coordinators
                if role == "coordinator" {
                    {
                        let mut coordinators = swarm_coordinators.write().await;
                        coordinators.insert(swarm_id.clone(), target_session.clone());
                    }
                    // Demote the old coordinator to agent (separate scope to avoid nested lock)
                    let mut members = swarm_members.write().await;
                    if let Some(m) = members.get_mut(&req_session_id) {
                        if m.session_id != target_session {
                            m.role = "agent".to_string();
                        }
                    }
                }

                broadcast_swarm_status(&swarm_id, &swarm_members, &swarms_by_id).await;
                record_swarm_event(
                    &event_history,
                    &event_counter,
                    &swarm_event_tx,
                    req_session_id.clone(),
                    None,
                    Some(swarm_id.clone()),
                    SwarmEventType::Notification {
                        notification_type: "role_assignment".to_string(),
                        message: format!("{} -> {}", target_session, role),
                    },
                )
                .await;
                let _ = client_event_tx.send(ServerEvent::Done { id });
            }

            Request::CommSummary {
                id,
                session_id: _req_session_id,
                target_session,
                limit,
            } => {
                let limit = limit.unwrap_or(10);
                let agent_sessions = sessions.read().await;
                if let Some(agent) = agent_sessions.get(&target_session) {
                    let tool_calls = if let Ok(agent) = agent.try_lock() {
                        let history = agent.get_history();
                        let mut calls: Vec<crate::protocol::ToolCallSummary> = Vec::new();
                        for msg in history.iter().rev() {
                            if calls.len() >= limit {
                                break;
                            }
                            if let Some(tool_names) = &msg.tool_calls {
                                for name in tool_names {
                                    calls.push(crate::protocol::ToolCallSummary {
                                        tool_name: name.clone(),
                                        brief_output: truncate_detail(&msg.content, 200),
                                        timestamp_secs: None,
                                    });
                                    if calls.len() >= limit {
                                        break;
                                    }
                                }
                            }
                        }
                        calls.reverse();
                        calls
                    } else {
                        Vec::new()
                    };
                    let _ = client_event_tx.send(ServerEvent::CommSummaryResponse {
                        id,
                        session_id: target_session,
                        tool_calls,
                    });
                } else {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: format!("Unknown session '{}'", target_session),
                        retry_after_secs: None,
                    });
                }
            }

            Request::CommReadContext {
                id,
                session_id: _req_session_id,
                target_session,
            } => {
                let agent_sessions = sessions.read().await;
                if let Some(agent) = agent_sessions.get(&target_session) {
                    let messages = if let Ok(agent) = agent.try_lock() {
                        agent.get_history()
                    } else {
                        Vec::new()
                    };
                    let _ = client_event_tx.send(ServerEvent::CommContextHistory {
                        id,
                        session_id: target_session,
                        messages,
                    });
                } else {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: format!("Unknown session '{}'", target_session),
                        retry_after_secs: None,
                    });
                }
            }

            Request::CommResyncPlan {
                id,
                session_id: req_session_id,
            } => {
                let swarm_id = {
                    let members = swarm_members.read().await;
                    members
                        .get(&req_session_id)
                        .and_then(|m| m.swarm_id.clone())
                };

                if let Some(swarm_id) = swarm_id {
                    let plan_state = {
                        let mut plans = swarm_plans.write().await;
                        plans.get_mut(&swarm_id).map(|vp| {
                            vp.participants.insert(req_session_id.clone());
                            (vp.version, vp.items.len())
                        })
                    };
                    if let Some((version, item_count)) = plan_state {
                        if let Some(member) = swarm_members.read().await.get(&req_session_id) {
                            let _ = member.event_tx.send(ServerEvent::Notification {
                                from_session: req_session_id.clone(),
                                from_name: member.friendly_name.clone(),
                                notification_type: NotificationType::Message {
                                    scope: Some("plan".to_string()),
                                    channel: None,
                                },
                                message: format!(
                                    "Plan attached to this session (v{}, {} items).",
                                    version, item_count
                                ),
                            });
                        }
                        broadcast_swarm_plan(
                            &swarm_id,
                            Some("resync".to_string()),
                            &swarm_plans,
                            &swarm_members,
                            &swarms_by_id,
                        )
                        .await;
                        record_swarm_event(
                            &event_history,
                            &event_counter,
                            &swarm_event_tx,
                            req_session_id.clone(),
                            None,
                            Some(swarm_id.clone()),
                            SwarmEventType::PlanUpdate {
                                swarm_id: swarm_id.clone(),
                                item_count,
                            },
                        )
                        .await;
                        let _ = client_event_tx.send(ServerEvent::Done { id });
                    } else {
                        let _ = client_event_tx.send(ServerEvent::Error {
                            id,
                            message: "No swarm plan exists for this swarm.".to_string(),
                            retry_after_secs: None,
                        });
                    }
                } else {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: "Not in a swarm.".to_string(),
                        retry_after_secs: None,
                    });
                }
            }

            Request::CommAssignTask {
                id,
                session_id: req_session_id,
                target_session,
                task_id,
                message,
            } => {
                // Verify the requester is the coordinator
                let (swarm_id, is_coordinator) = {
                    let members = swarm_members.read().await;
                    let swarm_id = members
                        .get(&req_session_id)
                        .and_then(|m| m.swarm_id.clone());
                    let is_coordinator = if let Some(ref sid) = swarm_id {
                        let coordinators = swarm_coordinators.read().await;
                        coordinators
                            .get(sid)
                            .map(|c| c == &req_session_id)
                            .unwrap_or(false)
                    } else {
                        false
                    };
                    (swarm_id, is_coordinator)
                };

                if !is_coordinator {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: "Only the coordinator can assign tasks.".to_string(),
                        retry_after_secs: None,
                    });
                    continue;
                }

                let swarm_id = match swarm_id {
                    Some(sid) => sid,
                    None => {
                        let _ = client_event_tx.send(ServerEvent::Error {
                            id,
                            message: "Not in a swarm.".to_string(),
                            retry_after_secs: None,
                        });
                        continue;
                    }
                };

                // Find and update the plan item
                let (task_content, participant_ids, plan_item_count) = {
                    let mut plans = swarm_plans.write().await;
                    let vp = plans
                        .entry(swarm_id.clone())
                        .or_insert_with(VersionedPlan::new);
                    let found = vp.items.iter_mut().find(|t| t.id == task_id);
                    if let Some(item) = found {
                        item.assigned_to = Some(target_session.clone());
                        item.status = "queued".to_string();
                        vp.version += 1;
                        vp.participants.insert(req_session_id.clone());
                        vp.participants.insert(target_session.clone());
                        (
                            Some(item.content.clone()),
                            vp.participants.clone(),
                            vp.items.len(),
                        )
                    } else {
                        (None, HashSet::new(), 0)
                    }
                };

                match task_content {
                    Some(content) => {
                        broadcast_swarm_plan(
                            &swarm_id,
                            Some("task_assigned".to_string()),
                            &swarm_plans,
                            &swarm_members,
                            &swarms_by_id,
                        )
                        .await;
                        record_swarm_event(
                            &event_history,
                            &event_counter,
                            &swarm_event_tx,
                            req_session_id.clone(),
                            None,
                            Some(swarm_id.clone()),
                            SwarmEventType::PlanUpdate {
                                swarm_id: swarm_id.clone(),
                                item_count: plan_item_count,
                            },
                        )
                        .await;

                        // Notify target via soft interrupt
                        let coordinator_name = {
                            let members = swarm_members.read().await;
                            members
                                .get(&req_session_id)
                                .and_then(|m| m.friendly_name.clone())
                        };
                        let msg = if let Some(ref extra) = message {
                            format!(
                                "Task assigned to you by coordinator: {} — {}",
                                content, extra
                            )
                        } else {
                            format!("Task assigned to you by coordinator: {}", content)
                        };

                        let target_agent = {
                            let agent_sessions = sessions.read().await;
                            agent_sessions.get(&target_session).cloned()
                        };
                        if let Some(agent) = target_agent.as_ref() {
                            if let Ok(agent) = agent.try_lock() {
                                agent.queue_soft_interrupt(msg.clone(), false);
                            }
                        }
                        if let Some(member) = swarm_members.read().await.get(&target_session) {
                            let _ = member.event_tx.send(ServerEvent::Notification {
                                from_session: req_session_id.clone(),
                                from_name: coordinator_name.clone(),
                                notification_type: NotificationType::Message {
                                    scope: Some("dm".to_string()),
                                    channel: None,
                                },
                                message: msg,
                            });
                        }

                        // Headless sessions do not have a request loop to consume queued
                        // interrupts, so run assigned work immediately when no client owns it.
                        let target_has_client = {
                            let conns = client_connections.read().await;
                            conns.values().any(|c| c.session_id == target_session)
                        };
                        if !target_has_client {
                            if let Some(agent_arc) = target_agent {
                                let target_session_for_run = target_session.clone();
                                let swarm_members_for_run = Arc::clone(&swarm_members);
                                let swarms_for_run = Arc::clone(&swarms_by_id);
                                let swarm_plans_for_run = Arc::clone(&swarm_plans);
                                let swarm_id_for_run = swarm_id.clone();
                                let task_id_for_run = task_id.clone();
                                let event_history_for_run = Arc::clone(&event_history);
                                let event_counter_for_run = Arc::clone(&event_counter);
                                let swarm_event_tx_for_run = swarm_event_tx.clone();
                                let assignment_text = if let Some(extra) = message.clone() {
                                    format!(
                                        "{}\n\nAdditional coordinator instructions:\n{}",
                                        content, extra
                                    )
                                } else {
                                    content.clone()
                                };
                                tokio::spawn(async move {
                                    {
                                        let mut plans = swarm_plans_for_run.write().await;
                                        if let Some(vp) = plans.get_mut(&swarm_id_for_run) {
                                            if let Some(item) = vp
                                                .items
                                                .iter_mut()
                                                .find(|t| t.id == task_id_for_run)
                                            {
                                                item.status = "running".to_string();
                                                vp.version += 1;
                                            }
                                        }
                                    }
                                    broadcast_swarm_plan(
                                        &swarm_id_for_run,
                                        Some("task_running".to_string()),
                                        &swarm_plans_for_run,
                                        &swarm_members_for_run,
                                        &swarms_for_run,
                                    )
                                    .await;
                                    update_member_status(
                                        &target_session_for_run,
                                        "running",
                                        Some(truncate_detail(&assignment_text, 120)),
                                        &swarm_members_for_run,
                                        &swarms_for_run,
                                        Some(&event_history_for_run),
                                        Some(&event_counter_for_run),
                                        Some(&swarm_event_tx_for_run),
                                    )
                                    .await;

                                    let result = {
                                        let mut agent = agent_arc.lock().await;
                                        agent.run_once_capture(&assignment_text).await
                                    };

                                    match result {
                                        Ok(_) => {
                                            {
                                                let mut plans = swarm_plans_for_run.write().await;
                                                if let Some(vp) = plans.get_mut(&swarm_id_for_run) {
                                                    if let Some(item) = vp
                                                        .items
                                                        .iter_mut()
                                                        .find(|t| t.id == task_id_for_run)
                                                    {
                                                        item.status = "done".to_string();
                                                        vp.version += 1;
                                                    }
                                                }
                                            }
                                            broadcast_swarm_plan(
                                                &swarm_id_for_run,
                                                Some("task_completed".to_string()),
                                                &swarm_plans_for_run,
                                                &swarm_members_for_run,
                                                &swarms_for_run,
                                            )
                                            .await;
                                            update_member_status(
                                                &target_session_for_run,
                                                "completed",
                                                None,
                                                &swarm_members_for_run,
                                                &swarms_for_run,
                                                Some(&event_history_for_run),
                                                Some(&event_counter_for_run),
                                                Some(&swarm_event_tx_for_run),
                                            )
                                            .await;
                                        }
                                        Err(e) => {
                                            {
                                                let mut plans = swarm_plans_for_run.write().await;
                                                if let Some(vp) = plans.get_mut(&swarm_id_for_run) {
                                                    if let Some(item) = vp
                                                        .items
                                                        .iter_mut()
                                                        .find(|t| t.id == task_id_for_run)
                                                    {
                                                        item.status = "failed".to_string();
                                                        vp.version += 1;
                                                    }
                                                }
                                            }
                                            broadcast_swarm_plan(
                                                &swarm_id_for_run,
                                                Some("task_failed".to_string()),
                                                &swarm_plans_for_run,
                                                &swarm_members_for_run,
                                                &swarms_for_run,
                                            )
                                            .await;
                                            update_member_status(
                                                &target_session_for_run,
                                                "failed",
                                                Some(truncate_detail(&e.to_string(), 120)),
                                                &swarm_members_for_run,
                                                &swarms_for_run,
                                                Some(&event_history_for_run),
                                                Some(&event_counter_for_run),
                                                Some(&swarm_event_tx_for_run),
                                            )
                                            .await;
                                        }
                                    }
                                });
                            }
                        }

                        let plan_msg = format!(
                            "Plan updated: task '{}' assigned to {}.",
                            task_id, target_session
                        );
                        let members = swarm_members.read().await;
                        for sid in participant_ids {
                            if sid == target_session || sid == req_session_id {
                                continue;
                            }
                            if let Some(member) = members.get(&sid) {
                                let _ = member.event_tx.send(ServerEvent::Notification {
                                    from_session: req_session_id.clone(),
                                    from_name: coordinator_name.clone(),
                                    notification_type: NotificationType::Message {
                                        scope: Some("plan".to_string()),
                                        channel: None,
                                    },
                                    message: plan_msg.clone(),
                                });
                            }
                        }

                        let _ = client_event_tx.send(ServerEvent::Done { id });
                    }
                    None => {
                        let _ = client_event_tx.send(ServerEvent::Error {
                            id,
                            message: format!("Task '{}' not found in swarm plan", task_id),
                            retry_after_secs: None,
                        });
                    }
                }
            }

            Request::CommSubscribeChannel {
                id,
                session_id: req_session_id,
                channel,
            } => {
                let swarm_id = {
                    let members = swarm_members.read().await;
                    members
                        .get(&req_session_id)
                        .and_then(|m| m.swarm_id.clone())
                };

                if let Some(swarm_id) = swarm_id {
                    let mut subs = channel_subscriptions.write().await;
                    subs.entry(swarm_id.clone())
                        .or_default()
                        .entry(channel.clone())
                        .or_default()
                        .insert(req_session_id.clone());
                    record_swarm_event(
                        &event_history,
                        &event_counter,
                        &swarm_event_tx,
                        req_session_id.clone(),
                        None,
                        Some(swarm_id.clone()),
                        SwarmEventType::Notification {
                            notification_type: "channel_subscribe".to_string(),
                            message: channel.clone(),
                        },
                    )
                    .await;
                    let _ = client_event_tx.send(ServerEvent::Done { id });
                } else {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: "Not in a swarm.".to_string(),
                        retry_after_secs: None,
                    });
                }
            }

            Request::CommUnsubscribeChannel {
                id,
                session_id: req_session_id,
                channel,
            } => {
                let swarm_id = {
                    let members = swarm_members.read().await;
                    members
                        .get(&req_session_id)
                        .and_then(|m| m.swarm_id.clone())
                };

                if let Some(swarm_id) = swarm_id {
                    let mut subs = channel_subscriptions.write().await;
                    if let Some(swarm_subs) = subs.get_mut(&swarm_id) {
                        if let Some(channel_subs) = swarm_subs.get_mut(&channel) {
                            channel_subs.remove(&req_session_id);
                            if channel_subs.is_empty() {
                                swarm_subs.remove(&channel);
                            }
                        }
                    }
                    record_swarm_event(
                        &event_history,
                        &event_counter,
                        &swarm_event_tx,
                        req_session_id.clone(),
                        None,
                        Some(swarm_id.clone()),
                        SwarmEventType::Notification {
                            notification_type: "channel_unsubscribe".to_string(),
                            message: channel.clone(),
                        },
                    )
                    .await;
                    let _ = client_event_tx.send(ServerEvent::Done { id });
                } else {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: "Not in a swarm.".to_string(),
                        retry_after_secs: None,
                    });
                }
            }

            Request::CommAwaitMembers {
                id,
                session_id: req_session_id,
                target_status,
                session_ids: requested_ids,
                timeout_secs,
            } => {
                let swarm_id = {
                    let members = swarm_members.read().await;
                    members
                        .get(&req_session_id)
                        .and_then(|m| m.swarm_id.clone())
                };

                if let Some(swarm_id) = swarm_id {
                    // Determine which sessions to watch
                    let watch_ids: Vec<String> = if requested_ids.is_empty() {
                        // Watch all non-self members in the swarm
                        let swarms = swarms_by_id.read().await;
                        swarms
                            .get(&swarm_id)
                            .map(|s| {
                                s.iter()
                                    .filter(|sid| *sid != &req_session_id)
                                    .cloned()
                                    .collect()
                            })
                            .unwrap_or_default()
                    } else {
                        requested_ids
                    };

                    if watch_ids.is_empty() {
                        let _ = client_event_tx.send(ServerEvent::CommAwaitMembersResponse {
                            id,
                            completed: true,
                            members: vec![],
                            summary: "No other members in swarm to wait for.".to_string(),
                        });
                    } else {
                        let timeout = std::time::Duration::from_secs(timeout_secs.unwrap_or(3600));
                        let swarm_members_clone = swarm_members.clone();
                        let mut event_rx = swarm_event_tx.subscribe();
                        let client_tx = client_event_tx.clone();

                        // Spawn a task that watches for status changes
                        tokio::spawn(async move {
                            let deadline = tokio::time::Instant::now() + timeout;

                            loop {
                                // Check current state
                                let (all_done, member_statuses) = {
                                    let members = swarm_members_clone.read().await;
                                    let statuses: Vec<crate::protocol::AwaitedMemberStatus> =
                                        watch_ids
                                            .iter()
                                            .map(|sid| {
                                                let (name, status) = members
                                                    .get(sid)
                                                    .map(|m| {
                                                        (m.friendly_name.clone(), m.status.clone())
                                                    })
                                                    .unwrap_or((None, "unknown".to_string()));
                                                let done = target_status.contains(&status)
                                                    || (status == "unknown"
                                                        && (target_status
                                                            .contains(&"stopped".to_string())
                                                            || target_status.contains(
                                                                &"completed".to_string(),
                                                            )));
                                                crate::protocol::AwaitedMemberStatus {
                                                    session_id: sid.clone(),
                                                    friendly_name: name,
                                                    status,
                                                    done,
                                                }
                                            })
                                            .collect();
                                    let all = statuses.iter().all(|s| s.done);
                                    (all, statuses)
                                };

                                if all_done {
                                    let done_names: Vec<String> = member_statuses
                                        .iter()
                                        .map(|m| {
                                            m.friendly_name.clone().unwrap_or_else(|| {
                                                m.session_id[..8.min(m.session_id.len())]
                                                    .to_string()
                                            })
                                        })
                                        .collect();
                                    let _ = client_tx.send(ServerEvent::CommAwaitMembersResponse {
                                        id,
                                        completed: true,
                                        members: member_statuses,
                                        summary: format!(
                                            "All {} members are done: {}",
                                            done_names.len(),
                                            done_names.join(", ")
                                        ),
                                    });
                                    return;
                                }

                                // Wait for next status change event or timeout
                                let remaining =
                                    deadline.saturating_duration_since(tokio::time::Instant::now());
                                if remaining.is_zero() {
                                    let pending: Vec<String> = member_statuses
                                        .iter()
                                        .filter(|m| !m.done)
                                        .map(|m| {
                                            let name =
                                                m.friendly_name.clone().unwrap_or_else(|| {
                                                    m.session_id[..8.min(m.session_id.len())]
                                                        .to_string()
                                                });
                                            format!("{} ({})", name, m.status)
                                        })
                                        .collect();
                                    let _ = client_tx.send(ServerEvent::CommAwaitMembersResponse {
                                        id,
                                        completed: false,
                                        members: member_statuses,
                                        summary: format!(
                                            "Timed out. Still waiting on: {}",
                                            pending.join(", ")
                                        ),
                                    });
                                    return;
                                }

                                // Wait for a relevant status_change event
                                match tokio::time::timeout(remaining, event_rx.recv()).await {
                                    Ok(Ok(event)) => {
                                        // Only recheck on status changes for watched sessions
                                        if let SwarmEventType::StatusChange { .. } = &event.event {
                                            if watch_ids.contains(&event.session_id) {
                                                continue; // Recheck at top of loop
                                            }
                                        }
                                        // Also recheck on member leave events
                                        if let SwarmEventType::MemberChange { action } =
                                            &event.event
                                        {
                                            if action == "left"
                                                && watch_ids.contains(&event.session_id)
                                            {
                                                continue;
                                            }
                                        }
                                    }
                                    Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                                        // Missed events, recheck state immediately
                                        continue;
                                    }
                                    Ok(Err(broadcast::error::RecvError::Closed)) => {
                                        let _ =
                                            client_tx.send(ServerEvent::CommAwaitMembersResponse {
                                                id,
                                                completed: false,
                                                members: member_statuses,
                                                summary: "Server shutting down.".to_string(),
                                            });
                                        return;
                                    }
                                    Err(_) => {
                                        // Timeout
                                        continue; // Will hit the timeout check at top
                                    }
                                }
                            }
                        });
                    }
                } else {
                    let _ = client_event_tx.send(ServerEvent::Error {
                        id,
                        message: "Not in a swarm. Use a git repository to enable swarm features."
                            .to_string(),
                        retry_after_secs: None,
                    });
                }
            }

            // These are handled via channels, not direct requests from TUI
            Request::ClientDebugCommand { id, .. } => {
                let _ = client_event_tx.send(ServerEvent::Error {
                    id,
                    message: "ClientDebugCommand is for internal use only".to_string(),
                    retry_after_secs: None,
                });
            }
            Request::ClientDebugResponse { id, output } => {
                let _ = client_debug_response_tx.send((id, output));
            }
        }
    }

    // Clean up: remove this client's session from the map
    // Mark session status and persist before dropping the agent
    let disconnected_while_processing = client_is_processing
        || processing_task
            .as_ref()
            .map(|handle| !handle.is_finished())
            .unwrap_or(false);
    {
        let mut sessions_guard = sessions.write().await;
        if let Some(agent_arc) = sessions_guard.remove(&client_session_id) {
            drop(sessions_guard);
            let mut agent = agent_arc.lock().await;

            if disconnected_while_processing {
                agent.mark_crashed(Some("Client disconnected while processing".to_string()));
            } else {
                agent.mark_closed();
            }

            let memory_enabled = agent.memory_enabled();
            let transcript = if memory_enabled {
                Some(agent.build_transcript_for_extraction())
            } else {
                None
            };
            let sid = client_session_id.clone();
            drop(agent);
            if let Some(transcript) = transcript {
                crate::memory_agent::trigger_final_extraction(transcript, sid);
            }
        }
    }

    // Clean up: remove from swarm tracking
    {
        let crashed = disconnected_while_processing;
        let (status, detail) = if crashed {
            ("crashed", Some("disconnect while running".to_string()))
        } else {
            ("stopped", Some("disconnected".to_string()))
        };
        update_member_status(
            &client_session_id,
            status,
            detail,
            &swarm_members,
            &swarms_by_id,
            Some(&event_history),
            Some(&event_counter),
            Some(&swarm_event_tx),
        )
        .await;

        let (swarm_id, removed_name) = {
            let mut members = swarm_members.write().await;
            if let Some(member) = members.remove(&client_session_id) {
                (member.swarm_id, member.friendly_name)
            } else {
                (None, None)
            }
        };

        if let Some(ref swarm_id) = swarm_id {
            record_swarm_event(
                &event_history,
                &event_counter,
                &swarm_event_tx,
                client_session_id.clone(),
                removed_name.clone(),
                Some(swarm_id.clone()),
                SwarmEventType::MemberChange {
                    action: "left".to_string(),
                },
            )
            .await;
            remove_plan_participant(swarm_id, &client_session_id, &swarm_plans).await;
            let was_coordinator = {
                let coordinators = swarm_coordinators.read().await;
                coordinators
                    .get(swarm_id)
                    .map(|id| id == &client_session_id)
                    .unwrap_or(false)
            };

            let mut new_coordinator: Option<String> = None;
            {
                let mut swarms = swarms_by_id.write().await;
                if let Some(swarm) = swarms.get_mut(swarm_id) {
                    swarm.remove(&client_session_id);
                    if swarm.is_empty() {
                        swarms.remove(swarm_id);
                    } else if was_coordinator {
                        new_coordinator = swarm.iter().min().cloned();
                    }
                }
            }

            if was_coordinator {
                let mut coordinators = swarm_coordinators.write().await;
                coordinators.remove(swarm_id);
                if let Some(new_id) = new_coordinator.clone() {
                    coordinators.insert(swarm_id.clone(), new_id.clone());
                    let mut plans = swarm_plans.write().await;
                    if let Some(vp) = plans.get_mut(swarm_id) {
                        vp.participants.insert(new_id.clone());
                    }
                }
            }

            if let Some(new_id) = new_coordinator {
                let members = swarm_members.read().await;
                if let Some(member) = members.get(&new_id) {
                    let msg = "You are now the coordinator for this swarm.".to_string();
                    let _ = member.event_tx.send(ServerEvent::Notification {
                        from_session: new_id.clone(),
                        from_name: member.friendly_name.clone(),
                        notification_type: NotificationType::Message {
                            scope: Some("swarm".to_string()),
                            channel: None,
                        },
                        message: msg.clone(),
                    });
                }
            }

            broadcast_swarm_status(swarm_id, &swarm_members, &swarms_by_id).await;
        }
    }

    // Clean up: remove client debug channel
    {
        let mut debug_state = client_debug_state.write().await;
        debug_state.unregister(&client_debug_id);
    }
    {
        let mut connections = client_connections.write().await;
        connections.remove(&client_connection_id);
    }
    // Clean up shutdown signal for this session
    {
        let mut signals = shutdown_signals.write().await;
        signals.remove(&client_session_id);
    }

    if let Some(handle) = processing_task.take() {
        handle.abort();
    }

    event_handle.abort();
    Ok(())
}

/// Process a message and stream events (broadcast channel - deprecated)
#[allow(dead_code)]
async fn process_message_streaming(
    agent: Arc<Mutex<Agent>>,
    content: &str,
    event_tx: broadcast::Sender<ServerEvent>,
) -> Result<()> {
    let mut agent = agent.lock().await;
    agent.run_once_streaming(content, event_tx).await
}

/// Process a message and stream events (mpsc channel - per-client)
async fn process_message_streaming_mpsc(
    agent: Arc<Mutex<Agent>>,
    content: &str,
    images: Vec<(String, String)>,
    event_tx: tokio::sync::mpsc::UnboundedSender<ServerEvent>,
) -> Result<()> {
    let mut agent = agent.lock().await;
    agent
        .run_once_streaming_mpsc(content, images, event_tx)
        .await
}

async fn broadcast_swarm_status(
    swarm_id: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
) {
    let session_ids: Vec<String> = {
        let swarms = swarms_by_id.read().await;
        swarms
            .get(swarm_id)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    };
    if session_ids.is_empty() {
        return;
    }

    let members_guard = swarm_members.read().await;
    let members_list: Vec<crate::protocol::SwarmMemberStatus> = session_ids
        .iter()
        .filter_map(|sid| {
            members_guard
                .get(sid)
                .map(|m| crate::protocol::SwarmMemberStatus {
                    session_id: m.session_id.clone(),
                    friendly_name: m.friendly_name.clone(),
                    status: m.status.clone(),
                    detail: m.detail.clone(),
                    role: Some(m.role.clone()),
                })
        })
        .collect();

    let event = ServerEvent::SwarmStatus {
        members: members_list,
    };
    for sid in session_ids {
        if let Some(member) = members_guard.get(&sid) {
            let _ = member.event_tx.send(event.clone());
        }
    }
}

/// Broadcast the authoritative swarm plan snapshot.
///
/// Plan snapshots are sent to explicit plan participants. If a plan has no
/// participants yet, fall back to all current swarm members.
async fn broadcast_swarm_plan(
    swarm_id: &str,
    reason: Option<String>,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
) {
    let (version, items, mut participants): (u64, Vec<PlanItem>, Vec<String>) = {
        let plans = swarm_plans.read().await;
        let Some(vp) = plans.get(swarm_id) else {
            return;
        };
        let mut p: Vec<String> = vp.participants.iter().cloned().collect();
        p.sort();
        (vp.version, vp.items.clone(), p)
    };

    if participants.is_empty() {
        let swarms = swarms_by_id.read().await;
        participants = swarms
            .get(swarm_id)
            .map(|s| {
                let mut ids: Vec<String> = s.iter().cloned().collect();
                ids.sort();
                ids
            })
            .unwrap_or_default();
    }

    if participants.is_empty() {
        return;
    }

    let event = ServerEvent::SwarmPlan {
        swarm_id: swarm_id.to_string(),
        version,
        items,
        participants: participants.clone(),
        reason,
    };

    let members = swarm_members.read().await;
    for sid in participants {
        if let Some(member) = members.get(&sid) {
            let _ = member.event_tx.send(event.clone());
        }
    }
}

async fn rename_plan_participant(
    swarm_id: &str,
    old_session_id: &str,
    new_session_id: &str,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
) {
    let mut plans = swarm_plans.write().await;
    if let Some(vp) = plans.get_mut(swarm_id) {
        if vp.participants.remove(old_session_id) {
            vp.participants.insert(new_session_id.to_string());
        }
        for item in &mut vp.items {
            if item.assigned_to.as_deref() == Some(old_session_id) {
                item.assigned_to = Some(new_session_id.to_string());
            }
        }
    }
}

async fn remove_plan_participant(
    swarm_id: &str,
    session_id: &str,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
) {
    let mut plans = swarm_plans.write().await;
    if let Some(vp) = plans.get_mut(swarm_id) {
        vp.participants.remove(session_id);
    }
}

async fn remove_session_from_swarm(
    session_id: &str,
    swarm_id: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
) {
    remove_plan_participant(swarm_id, session_id, swarm_plans).await;

    {
        let mut swarms = swarms_by_id.write().await;
        if let Some(swarm) = swarms.get_mut(swarm_id) {
            swarm.remove(session_id);
            if swarm.is_empty() {
                swarms.remove(swarm_id);
            }
        }
    }

    let was_coordinator = {
        let coordinators = swarm_coordinators.read().await;
        coordinators
            .get(swarm_id)
            .map(|id| id == session_id)
            .unwrap_or(false)
    };

    if was_coordinator {
        let new_coordinator = {
            let swarms = swarms_by_id.read().await;
            swarms.get(swarm_id).and_then(|s| s.iter().min().cloned())
        };

        {
            let mut coordinators = swarm_coordinators.write().await;
            coordinators.remove(swarm_id);
            if let Some(ref new_id) = new_coordinator {
                coordinators.insert(swarm_id.to_string(), new_id.clone());
            }
        }

        if let Some(new_id) = new_coordinator {
            {
                let mut members = swarm_members.write().await;
                if let Some(member) = members.get_mut(&new_id) {
                    member.role = "coordinator".to_string();
                }
            }
            let mut plans = swarm_plans.write().await;
            if let Some(vp) = plans.get_mut(swarm_id) {
                vp.participants.insert(new_id.clone());
            }
            let members = swarm_members.read().await;
            if let Some(member) = members.get(&new_id) {
                let _ = member.event_tx.send(ServerEvent::Notification {
                    from_session: new_id.clone(),
                    from_name: member.friendly_name.clone(),
                    notification_type: NotificationType::Message {
                        scope: Some("swarm".to_string()),
                        channel: None,
                    },
                    message: "You are now the coordinator for this swarm.".to_string(),
                });
            }
        }
    }

    {
        let mut members = swarm_members.write().await;
        if let Some(member) = members.get_mut(session_id) {
            member.role = "agent".to_string();
        }
    }

    broadcast_swarm_status(swarm_id, swarm_members, swarms_by_id).await;
}

async fn record_swarm_event(
    event_history: &Arc<RwLock<Vec<SwarmEvent>>>,
    event_counter: &Arc<std::sync::atomic::AtomicU64>,
    swarm_event_tx: &broadcast::Sender<SwarmEvent>,
    session_id: String,
    session_name: Option<String>,
    swarm_id: Option<String>,
    event: SwarmEventType,
) {
    let swarm_event = SwarmEvent {
        id: event_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        session_id,
        session_name,
        swarm_id,
        event,
        timestamp: Instant::now(),
        absolute_time: std::time::SystemTime::now(),
    };
    let _ = swarm_event_tx.send(swarm_event.clone());
    let mut history = event_history.write().await;
    history.push(swarm_event);
    if history.len() > MAX_EVENT_HISTORY {
        history.remove(0);
    }
}

async fn record_swarm_event_for_session(
    session_id: &str,
    event: SwarmEventType,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    event_history: &Arc<RwLock<Vec<SwarmEvent>>>,
    event_counter: &Arc<std::sync::atomic::AtomicU64>,
    swarm_event_tx: &broadcast::Sender<SwarmEvent>,
) {
    let (session_name, swarm_id) = {
        let members = swarm_members.read().await;
        if let Some(member) = members.get(session_id) {
            (member.friendly_name.clone(), member.swarm_id.clone())
        } else {
            (None, None)
        }
    };
    record_swarm_event(
        event_history,
        event_counter,
        swarm_event_tx,
        session_id.to_string(),
        session_name,
        swarm_id,
        event,
    )
    .await;
}

async fn update_member_status(
    session_id: &str,
    status: &str,
    detail: Option<String>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    event_history: Option<&Arc<RwLock<Vec<SwarmEvent>>>>,
    event_counter: Option<&Arc<std::sync::atomic::AtomicU64>>,
    swarm_event_tx: Option<&broadcast::Sender<SwarmEvent>>,
) {
    let (swarm_id, agent_name, status_changed, old_status) = {
        let mut members = swarm_members.write().await;
        if let Some(member) = members.get_mut(session_id) {
            let previous_status = member.status.clone();
            let changed = member.status != status;
            if changed {
                member.last_status_change = Instant::now();
            }
            let name = member.friendly_name.clone();
            member.status = status.to_string();
            member.detail = detail;
            (member.swarm_id.clone(), name, changed, previous_status)
        } else {
            (None, None, false, String::new())
        }
    };
    if let Some(ref id) = swarm_id {
        if status_changed {
            if let (Some(history), Some(counter), Some(tx)) =
                (event_history, event_counter, swarm_event_tx)
            {
                record_swarm_event(
                    history,
                    counter,
                    tx,
                    session_id.to_string(),
                    agent_name.clone(),
                    Some(id.clone()),
                    SwarmEventType::StatusChange {
                        old_status,
                        new_status: status.to_string(),
                    },
                )
                .await;
            }
        }

        broadcast_swarm_status(id, swarm_members, swarms_by_id).await;

        // Notify coordinator when an agent completes
        if status_changed && status == "completed" {
            let coordinator_id = {
                // We don't have swarm_coordinators here, so we check the members
                let members = swarm_members.read().await;
                members
                    .values()
                    .find(|m| {
                        m.swarm_id.as_deref() == Some(id)
                            && m.role == "coordinator"
                            && m.session_id != session_id
                    })
                    .map(|m| (m.session_id.clone(), m.event_tx.clone()))
            };
            if let Some((coord_id, coord_tx)) = coordinator_id {
                let name = agent_name
                    .as_deref()
                    .unwrap_or(&session_id[..8.min(session_id.len())]);
                let msg = format!(
                    "Agent {} completed their work. Use assign_task to give them new work, or stop to remove them.",
                    name
                );
                let _ = coord_tx.send(ServerEvent::Notification {
                    from_session: session_id.to_string(),
                    from_name: agent_name.clone(),
                    notification_type: NotificationType::Message {
                        scope: Some("swarm".to_string()),
                        channel: None,
                    },
                    message: msg,
                });
            }
        }
    }
}

fn truncate_detail(text: &str, max_len: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim();
    let max_len = max_len.max(1);
    if trimmed.chars().count() <= max_len {
        return trimmed.to_string();
    }
    if max_len <= 3 {
        return trimmed.chars().take(max_len).collect();
    }
    let mut out: String = trimmed.chars().take(max_len - 3).collect();
    out.push_str("...");
    out
}

fn summarize_plan_items(items: &[PlanItem], max_items: usize) -> String {
    if items.is_empty() {
        return "no items".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    for item in items.iter().take(max_items.max(1)) {
        parts.push(item.content.clone());
    }
    let mut summary = parts.join("; ");
    if items.len() > max_items.max(1) {
        summary.push_str(&format!(" (+{} more)", items.len() - max_items.max(1)));
    }
    summary
}

async fn run_swarm_task(
    agent: Arc<Mutex<Agent>>,
    description: &str,
    subagent_type: &str,
    prompt: &str,
) -> Result<String> {
    let (provider, registry, session_id, working_dir, coordinator_model) = {
        let agent = agent.lock().await;
        (
            agent.provider_fork(),
            agent.registry(),
            agent.session_id().to_string(),
            agent.working_dir().map(PathBuf::from),
            agent.provider_model(),
        )
    };
    let mut session = Session::create(
        Some(session_id),
        Some(format!("{} (@{} swarm)", description, subagent_type)),
    );
    session.model = Some(coordinator_model);
    if let Some(dir) = working_dir {
        session.working_dir = Some(dir.display().to_string());
    }
    session.save()?;

    let mut allowed: HashSet<String> = registry.tool_names().await.into_iter().collect();
    for blocked in ["subagent", "task", "todowrite", "todoread"] {
        allowed.remove(blocked);
    }

    let mut worker = Agent::new_with_session(provider, registry, session, Some(allowed));
    let output = worker.run_once_capture(prompt).await?;
    Ok(output)
}

async fn run_swarm_message(agent: Arc<Mutex<Agent>>, message: &str) -> Result<String> {
    let working_dir = {
        let agent = agent.lock().await;
        agent.working_dir().map(|dir| dir.to_string())
    };
    let working_dir_hint = working_dir
        .as_deref()
        .map(|dir| format!("Working directory: {}\n", dir))
        .unwrap_or_default();

    let planner_prompt = format!(
        "{working_dir_hint}You are a task planner. Break the request into 2-4 subtasks. \
Return ONLY a JSON array of objects with keys: description, prompt, subagent_type. \
No extra text.\n\nRequest:\n{message}"
    );

    let plan_text = {
        let mut agent = agent.lock().await;
        agent.run_once_capture(&planner_prompt).await?
    };

    let mut tasks = parse_swarm_tasks(&plan_text);
    if tasks.is_empty() {
        tasks.push(SwarmTaskSpec {
            description: "Main task".to_string(),
            prompt: message.to_string(),
            subagent_type: Some("general".to_string()),
        });
    }

    let task_futures = tasks.iter().map(|task| {
        let agent = agent.clone();
        let working_dir_hint = working_dir_hint.clone();
        let description = task.description.clone();
        let prompt = format!("{working_dir_hint}{}", task.prompt);
        let subagent_type = task
            .subagent_type
            .clone()
            .unwrap_or_else(|| "general".to_string());
        async move {
            let output = run_swarm_task(agent, &description, &subagent_type, &prompt).await?;
            Ok::<(String, String), anyhow::Error>((description, output))
        }
    });
    let task_outputs = try_join_all(task_futures).await?;

    let mut integration_prompt = String::new();
    integration_prompt.push_str(
        "You are the coordinator. Complete the original request using the subagent outputs below. ",
    );
    integration_prompt.push_str("Do not stop early; run any requested tests and fix failures.\n\n");
    integration_prompt.push_str("Original request:\n");
    integration_prompt.push_str(message);
    integration_prompt.push_str("\n\nSubagent outputs:\n");
    for (desc, output) in &task_outputs {
        integration_prompt.push_str(&format!("\n--- {} ---\n{}\n", desc, output));
    }
    integration_prompt.push_str("\nNow complete the task.\n");

    let final_output = {
        let mut agent = agent.lock().await;
        agent.run_once_capture(&integration_prompt).await?
    };

    Ok(final_output)
}

#[derive(Debug, Deserialize)]
struct SwarmTaskSpec {
    description: String,
    prompt: String,
    #[serde(default)]
    subagent_type: Option<String>,
}

fn parse_swarm_tasks(text: &str) -> Vec<SwarmTaskSpec> {
    if let Ok(tasks) = serde_json::from_str::<Vec<SwarmTaskSpec>>(text) {
        return tasks;
    }

    if let (Some(start), Some(end)) = (text.find('['), text.rfind(']')) {
        if start < end {
            if let Ok(tasks) = serde_json::from_str::<Vec<SwarmTaskSpec>>(&text[start..=end]) {
                return tasks;
            }
        }
    }

    Vec::new()
}

/// Handle debug socket connections (introspection + optional debug control)
/// Client for connecting to a running server
pub struct Client {
    reader: BufReader<ReadHalf>,
    writer: WriteHalf,
    next_id: u64,
}

impl Client {
    pub async fn connect() -> Result<Self> {
        Self::connect_with_path(socket_path()).await
    }

    pub async fn connect_with_path(path: PathBuf) -> Result<Self> {
        let stream = connect_socket(&path).await?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(reader),
            writer,
            next_id: 1,
        })
    }

    pub async fn connect_debug() -> Result<Self> {
        Self::connect_debug_with_path(debug_socket_path()).await
    }

    pub async fn connect_debug_with_path(path: PathBuf) -> Result<Self> {
        let stream = connect_socket(&path).await?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(reader),
            writer,
            next_id: 1,
        })
    }

    /// Send a message and return immediately (events come via read_event)
    pub async fn send_message(&mut self, content: &str) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;

        let request = Request::Message {
            id,
            content: content.to_string(),
            images: vec![],
        };
        let json = serde_json::to_string(&request)? + "\n";
        self.writer.write_all(json.as_bytes()).await?;
        Ok(id)
    }

    /// Subscribe to events
    pub async fn subscribe(&mut self) -> Result<u64> {
        self.subscribe_with_info(None, None).await
    }

    pub async fn subscribe_with_info(
        &mut self,
        working_dir: Option<String>,
        selfdev: Option<bool>,
    ) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;

        let request = Request::Subscribe {
            id,
            working_dir,
            selfdev,
        };
        let json = serde_json::to_string(&request)? + "\n";
        self.writer.write_all(json.as_bytes()).await?;
        Ok(id)
    }

    /// Read the next event from the server
    pub async fn read_event(&mut self) -> Result<ServerEvent> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await?;
        if n == 0 {
            anyhow::bail!("Server disconnected");
        }
        let event: ServerEvent = serde_json::from_str(&line)?;
        Ok(event)
    }

    pub async fn ping(&mut self) -> Result<bool> {
        let id = self.next_id;
        self.next_id += 1;

        let request = Request::Ping { id };
        let json = serde_json::to_string(&request)? + "\n";
        self.writer.write_all(json.as_bytes()).await?;

        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await?;
        if n == 0 {
            anyhow::bail!("Server disconnected");
        }
        let event: ServerEvent = serde_json::from_str(&line)?;

        match event {
            ServerEvent::Pong { .. } => Ok(true),
            _ => Ok(false),
        }
    }

    pub async fn get_state(&mut self) -> Result<ServerEvent> {
        let id = self.next_id;
        self.next_id += 1;

        let request = Request::GetState { id };
        let json = serde_json::to_string(&request)? + "\n";
        self.writer.write_all(json.as_bytes()).await?;

        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await?;
        if n == 0 {
            anyhow::bail!("Server disconnected");
        }
        let event: ServerEvent = serde_json::from_str(&line)?;
        Ok(event)
    }

    pub async fn clear(&mut self) -> Result<()> {
        let id = self.next_id;
        self.next_id += 1;

        let request = Request::Clear { id };
        let json = serde_json::to_string(&request)? + "\n";
        self.writer.write_all(json.as_bytes()).await?;
        Ok(())
    }

    pub async fn get_history(&mut self) -> Result<Vec<HistoryMessage>> {
        let event = self.get_history_event().await?;
        match event {
            ServerEvent::History { messages, .. } => Ok(messages),
            _ => Ok(Vec::new()),
        }
    }

    pub async fn get_history_event(&mut self) -> Result<ServerEvent> {
        let id = self.next_id;
        self.next_id += 1;

        let request = Request::GetHistory { id };
        let json = serde_json::to_string(&request)? + "\n";
        self.writer.write_all(json.as_bytes()).await?;
        for _ in 0..10 {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).await?;
            if n == 0 {
                anyhow::bail!("Server disconnected");
            }
            let event: ServerEvent = serde_json::from_str(&line)?;
            match event {
                ServerEvent::Ack { .. } => continue,
                _ => return Ok(event),
            }
        }

        Ok(ServerEvent::Error {
            id,
            message: "History response not received".to_string(),
            retry_after_secs: None,
        })
    }

    pub async fn resume_session(&mut self, session_id: &str) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;

        let request = Request::ResumeSession {
            id,
            session_id: session_id.to_string(),
        };
        let json = serde_json::to_string(&request)? + "\n";
        self.writer.write_all(json.as_bytes()).await?;
        Ok(id)
    }

    pub async fn reload(&mut self) -> Result<()> {
        let id = self.next_id;
        self.next_id += 1;

        let request = Request::Reload { id };
        let json = serde_json::to_string(&request)? + "\n";
        self.writer.write_all(json.as_bytes()).await?;
        Ok(())
    }

    pub async fn cycle_model(&mut self, direction: i8) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;

        let request = Request::CycleModel { id, direction };
        let json = serde_json::to_string(&request)? + "\n";
        self.writer.write_all(json.as_bytes()).await?;
        Ok(id)
    }

    pub async fn debug_command(&mut self, command: &str, session_id: Option<&str>) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;

        let request = Request::DebugCommand {
            id,
            command: command.to_string(),
            session_id: session_id.map(|s| s.to_string()),
        };
        let json = serde_json::to_string(&request)? + "\n";
        self.writer.write_all(json.as_bytes()).await?;
        Ok(id)
    }
}

/// Get the jcode repository directory
fn get_repo_dir() -> Option<PathBuf> {
    // Try CARGO_MANIFEST_DIR first (works when running from source)
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let path = PathBuf::from(manifest_dir);
    if path.join(".git").exists() {
        return Some(path);
    }

    // Fallback: check relative to executable
    if let Ok(exe) = std::env::current_exe() {
        // Assume structure: repo/target/release/<binary> (or platform-specific equivalent)
        if let Some(repo) = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            if repo.join(".git").exists() {
                return Some(repo.to_path_buf());
            }
        }
    }

    None
}

/// Server hot-reload: pull, build, and exec into new binary
#[allow(dead_code)]
fn do_server_reload() -> Result<()> {
    let repo_dir =
        get_repo_dir().ok_or_else(|| anyhow::anyhow!("Could not find jcode repository"))?;

    crate::logging::info("Server hot-reload starting...");

    // Pull latest changes
    crate::logging::info("Pulling latest changes...");
    if let Err(e) = crate::update::run_git_pull_ff_only(&repo_dir, true) {
        crate::logging::info(&format!("Warning: {}. Continuing with current code.", e));
    }

    // Build release
    crate::logging::info("Building...");
    let build = ProcessCommand::new("cargo")
        .args(["build", "--release"])
        .current_dir(&repo_dir)
        .status()?;

    if !build.success() {
        anyhow::bail!("Build failed");
    }

    if let Err(e) = build::install_local_release(&repo_dir) {
        crate::logging::info(&format!("Warning: install failed: {}", e));
    }

    crate::logging::info("✓ Build complete, restarting server...");

    // Find the new executable
    let exe = build::release_binary_path(&repo_dir);
    if !exe.exists() {
        anyhow::bail!("Built executable not found at {:?}", exe);
    }

    // Exec into new binary with serve command
    let err = crate::platform::replace_process(ProcessCommand::new(&exe).arg("serve"));

    // replace_process() only returns on error
    Err(anyhow::anyhow!("Failed to exec {:?}: {}", exe, err))
}

/// Server hot-reload with progress streaming to client
/// This just restarts with the existing binary - no rebuild
async fn do_server_reload_with_progress(
    tx: tokio::sync::mpsc::UnboundedSender<ServerEvent>,
    provider_arg: Option<String>,
    model_arg: Option<String>,
    socket_arg: String,
) -> Result<()> {
    let send_progress =
        |step: &str, message: &str, success: Option<bool>, output: Option<String>| {
            let _ = tx.send(ServerEvent::ReloadProgress {
                step: step.to_string(),
                message: message.to_string(),
                success,
                output,
            });
        };

    // Step 1: Find repo
    send_progress("init", "🔄 Starting hot-reload...", None, None);

    let repo_dir = get_repo_dir();
    if let Some(repo_dir) = &repo_dir {
        send_progress(
            "init",
            &format!("📁 Repository: {}", repo_dir.display()),
            Some(true),
            None,
        );
    } else {
        send_progress("init", "📁 Repository: (not found)", Some(true), None);
    }

    // Step 2: Check for binary
    let (exe, exe_label) =
        server_update_candidate().ok_or_else(|| anyhow::anyhow!("No reloadable binary found"))?;
    if !exe.exists() {
        send_progress("verify", "❌ No reloadable binary found", Some(false), None);
        send_progress(
            "verify",
            "💡 Run 'cargo build --release' first, then use 'selfdev reload'",
            Some(false),
            None,
        );
        anyhow::bail!("No binary found. Build first with 'cargo build --release'");
    }

    // Step 3: Get binary info
    let metadata = std::fs::metadata(&exe)?;
    let size_mb = metadata.len() as f64 / (1024.0 * 1024.0);
    let modified = metadata.modified().ok();

    let age_str = if let Some(mod_time) = modified {
        if let Ok(elapsed) = mod_time.elapsed() {
            let secs = elapsed.as_secs();
            if secs < 60 {
                format!("{} seconds ago", secs)
            } else if secs < 3600 {
                format!("{} minutes ago", secs / 60)
            } else if secs < 86400 {
                format!("{} hours ago", secs / 3600)
            } else {
                format!("{} days ago", secs / 86400)
            }
        } else {
            "unknown".to_string()
        }
    } else {
        "unknown".to_string()
    };

    send_progress(
        "verify",
        &format!(
            "✓ Binary ({}): {:.1} MB, built {}",
            exe_label, size_mb, age_str
        ),
        Some(true),
        None,
    );

    // Step 4: Show current git state (informational)
    if let Some(repo_dir) = &repo_dir {
        let head_output = ProcessCommand::new("git")
            .args(["log", "--oneline", "-1"])
            .current_dir(repo_dir)
            .output();

        if let Ok(output) = head_output {
            let head_str = String::from_utf8_lossy(&output.stdout);
            send_progress(
                "git",
                &format!("📍 HEAD: {}", head_str.trim()),
                Some(true),
                None,
            );
        }
    }

    // Step 5: Exec
    send_progress(
        "exec",
        "🚀 Restarting server with existing binary...",
        None,
        None,
    );

    // Small delay to ensure the progress message is sent
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    crate::logging::info(&format!("Exec'ing into binary: {:?}", exe));

    // Exec into new binary with serve command (preserve args)
    let mut cmd = ProcessCommand::new(&exe);
    cmd.arg("serve").arg("--socket").arg(socket_arg);
    if let Some(provider) = provider_arg {
        cmd.arg("--provider").arg(provider);
    }
    if let Some(model) = model_arg {
        cmd.arg("--model").arg(model);
    }
    let err = crate::platform::replace_process(&mut cmd);

    // replace_process() only returns on error
    Err(anyhow::anyhow!("Failed to exec {:?}: {}", exe, err))
}

fn provider_cli_arg(provider_name: &str) -> Option<String> {
    let lowered = provider_name.trim().to_lowercase();
    match lowered.as_str() {
        "openai" => Some("openai".to_string()),
        "claude" => Some("claude".to_string()),
        "cursor" => Some("cursor".to_string()),
        "copilot" => Some("copilot".to_string()),
        "antigravity" => Some("antigravity".to_string()),
        _ => None,
    }
}

fn normalize_model_arg(model: String) -> Option<String> {
    let trimmed = model.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("unknown") {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Await reload signal via in-process channel and exec into new binary.
/// Before exec'ing, gracefully stops all active generations so partial
/// responses are checkpointed to disk and can be resumed.
/// NOTE: This should only be called on selfdev servers (guarded at call site)
async fn await_reload_signal(
    sessions: Arc<RwLock<HashMap<String, Arc<Mutex<Agent>>>>>,
    swarm_members: Arc<RwLock<HashMap<String, SwarmMember>>>,
    shutdown_signals: Arc<RwLock<HashMap<String, crate::agent::InterruptSignal>>>,
) {
    use std::process::Command as ProcessCommand;

    // Double-check: only run on selfdev servers
    if !is_selfdev_env() {
        return;
    }

    let mut rx = reload_signal().1.clone();

    // Wait for a reload signal
    loop {
        if rx.changed().await.is_err() {
            return;
        }

        let signal = match rx.borrow_and_update().clone() {
            Some(s) => s,
            None => continue,
        };

        crate::logging::info("Server: reload signal received via channel");

        // Gracefully stop all active generations except the triggering session
        // (it's just sleeping in the selfdev tool, no point waiting for it)
        graceful_shutdown_sessions(
            &sessions,
            &swarm_members,
            &shutdown_signals,
            signal.triggering_session.as_deref(),
        )
        .await;

        // Get canary binary path
        if let Ok(binary) = crate::build::canary_binary_path() {
            if binary.exists() {
                crate::logging::info(&format!("Server: exec'ing into canary binary {:?}", binary));

                // Exec into the new binary with serve mode
                let err =
                    crate::platform::replace_process(ProcessCommand::new(&binary).arg("serve"));

                // If we get here, exec failed
                crate::logging::error(&format!("Failed to exec into canary {:?}: {}", binary, err));
            }
        }
        // Fallback: just exit and let something else restart us
        std::process::exit(42);
    }
}

/// Signal all active sessions to gracefully stop generation and checkpoint.
/// Only waits for sessions that are *actively generating* (status == "running"),
/// not sessions that are idle/ready/waiting for user input.
/// Uses a short 2-second timeout since most checkpoints complete in under 1s.
/// `skip_session` is the session that triggered the reload (selfdev tool) -
/// it's just sleeping in an infinite loop and will never finish on its own.
async fn graceful_shutdown_sessions(
    _sessions: &Arc<RwLock<HashMap<String, Arc<Mutex<Agent>>>>>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    shutdown_signals: &Arc<RwLock<HashMap<String, crate::agent::InterruptSignal>>>,
    skip_session: Option<&str>,
) {
    // Find sessions that are actively processing (status == "running").
    // Sessions with status "ready", "stopped", "failed", etc. are idle and
    // don't need graceful shutdown - they'll reconnect after the server restarts.
    let actively_generating: Vec<String> = {
        let members = swarm_members.read().await;
        members
            .iter()
            .filter(|(id, m)| {
                m.status == "running" && skip_session.map_or(true, |skip| !id.starts_with(skip))
            })
            .map(|(id, _)| id.clone())
            .collect()
    };

    if actively_generating.is_empty() {
        crate::logging::info(
            "Server: no sessions actively generating, proceeding with reload immediately",
        );
        return;
    }

    crate::logging::info(&format!(
        "Server: signaling {} actively generating session(s) to checkpoint: {:?}",
        actively_generating.len(),
        actively_generating
    ));

    // Signal graceful shutdown using the server-level signal map.
    // This avoids needing to lock the agent mutex (which is held by the
    // processing task during tool execution).
    {
        let signals = shutdown_signals.read().await;
        for session_id in &actively_generating {
            if let Some(signal) = signals.get(session_id) {
                signal.fire();
                crate::logging::info(&format!(
                    "Server: sent graceful shutdown signal to session {}",
                    session_id
                ));
            } else {
                crate::logging::warn(&format!(
                    "Server: no shutdown signal registered for session {} (may have already disconnected)",
                    session_id
                ));
            }
        }
    }

    // Wait for actively generating sessions to checkpoint, with a short timeout.
    // Most sessions checkpoint within 1s; 2s is generous. We don't need to wait
    // long because:
    // - The graceful shutdown signal causes the agent to stop after the current
    //   tool finishes and checkpoint partial content
    // - Clients will automatically reconnect after the server restarts
    // - Any in-flight API stream will be interrupted by the exec anyway
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(2);
    let mut poll_interval = tokio::time::interval(tokio::time::Duration::from_millis(50));

    loop {
        poll_interval.tick().await;

        let still_running: usize = {
            let members = swarm_members.read().await;
            actively_generating
                .iter()
                .filter(|id| {
                    members
                        .get(*id)
                        .map(|m| m.status == "running")
                        .unwrap_or(false)
                })
                .count()
        };

        if still_running == 0 {
            crate::logging::info("Server: all sessions checkpointed, proceeding with reload");
            break;
        }

        if tokio::time::Instant::now() >= deadline {
            crate::logging::warn(&format!(
                "Server: {} session(s) still generating after 2s timeout, proceeding with reload anyway",
                still_running
            ));
            break;
        }
    }
}
