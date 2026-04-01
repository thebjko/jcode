//! Memory system for cross-session learning
//!
//! Provides persistent memory that survives across sessions, organized by:
//! - Project (per working directory)
//! - Global (user-level preferences)
//!
//! Integrates with the Haiku sidecar for relevance verification and extraction.

use crate::memory_graph::{GRAPH_VERSION, MemoryGraph};
use crate::sidecar::Sidecar;
use crate::storage;
use crate::tui::info_widget::{
    InjectedMemoryItem, MemoryActivity, MemoryEvent, MemoryEventKind, MemoryState, StepResult,
    StepStatus,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime};

const LEGACY_NOTE_CATEGORY: &str = "note";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryScope {
    Project,
    Global,
    All,
}

impl MemoryScope {
    fn includes_project(self) -> bool {
        matches!(self, Self::Project | Self::All)
    }

    fn includes_global(self) -> bool {
        matches!(self, Self::Global | Self::All)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct LegacyNotesFile {
    #[serde(default)]
    entries: Vec<LegacyNoteEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyNoteEntry {
    id: String,
    content: String,
    created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
}

// === Global Activity Tracking ===

/// Global memory activity state - updated by sidecar, read by info widget
static MEMORY_ACTIVITY: Mutex<Option<MemoryActivity>> = Mutex::new(None);

/// Maximum number of recent events to keep
const MAX_RECENT_EVENTS: usize = 10;

// === Graph Cache ===

struct GraphCacheEntry {
    graph: MemoryGraph,
    modified: Option<SystemTime>,
}

struct GraphCache {
    entries: HashMap<PathBuf, GraphCacheEntry>,
}

impl GraphCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }
}

static GRAPH_CACHE: OnceLock<Mutex<GraphCache>> = OnceLock::new();

fn graph_cache() -> &'static Mutex<GraphCache> {
    GRAPH_CACHE.get_or_init(|| Mutex::new(GraphCache::new()))
}

fn graph_mtime(path: &PathBuf) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

fn cached_graph(path: &PathBuf) -> Option<MemoryGraph> {
    let modified = graph_mtime(path);
    let cache = graph_cache().lock().ok()?;
    let entry = cache.entries.get(path)?;
    if entry.modified == modified {
        Some(entry.graph.clone())
    } else {
        None
    }
}

fn cache_graph(path: PathBuf, graph: &MemoryGraph) {
    let modified = graph_mtime(&path);
    if let Ok(mut cache) = graph_cache().lock() {
        cache.entries.insert(
            path,
            GraphCacheEntry {
                graph: graph.clone(),
                modified,
            },
        );
    }
}

// === Per-Session Async Memory Buffer ===
//
// All memory injection state is keyed by session ID to prevent cross-session
// contamination. Previously these were process-global singletons, which meant
// Session B could consume a pending memory that was computed for Session A.

/// Pending memory prompt from background check - ready to inject on next turn.
/// Keyed by session ID so each session gets its own pending memory.
static PENDING_MEMORY: Mutex<Option<HashMap<String, PendingMemory>>> = Mutex::new(None);

/// Signature of the last injected prompt to suppress near-immediate duplicates.
/// Keyed by session ID.
static LAST_INJECTED_PROMPT_SIGNATURE: Mutex<Option<HashMap<String, (String, Instant)>>> =
    Mutex::new(None);

/// Memory IDs that have already been injected into the conversation.
/// Used to prevent the same memory from being re-injected on subsequent turns.
/// Keyed by session ID.
static INJECTED_MEMORY_IDS: Mutex<Option<HashMap<String, HashSet<String>>>> = Mutex::new(None);

/// Guard to ensure only one memory check runs at a time, per session.
/// Keyed by session ID.
static MEMORY_CHECK_IN_PROGRESS: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Suppress repeated identical memory payloads within this many seconds.
const MEMORY_REPEAT_SUPPRESSION_SECS: u64 = 90;

/// A pending memory result from async checking
#[derive(Debug, Clone)]
pub struct PendingMemory {
    /// The formatted memory prompt ready for injection
    pub prompt: String,
    /// Optional UI-focused rendering of the injected memory payload.
    /// This can contain extra display-only metadata that is not sent to the model.
    pub display_prompt: Option<String>,
    /// When this was computed
    pub computed_at: Instant,
    /// Number of relevant memories found
    pub count: usize,
    /// IDs of memories included in this prompt (for dedup tracking)
    pub memory_ids: Vec<String>,
}

impl PendingMemory {
    /// Check if this pending memory is still fresh (not too old)
    pub fn is_fresh(&self) -> bool {
        // Consider stale after 2 minutes
        self.computed_at.elapsed().as_secs() < 120
    }
}

fn prompt_signature(prompt: &str) -> String {
    prompt
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase()
}

/// Take pending memory if available and fresh for the given session.
pub fn take_pending_memory(session_id: &str) -> Option<PendingMemory> {
    if let Ok(mut guard) = PENDING_MEMORY.lock() {
        let map = guard.get_or_insert_with(HashMap::new);
        if let Some(pending) = map.remove(session_id) {
            if !pending.is_fresh() {
                crate::memory_log::log_pending_discarded(session_id, "stale (>120s)");
                return None;
            }

            let sig = prompt_signature(&pending.prompt);
            if let Ok(mut last_guard) = LAST_INJECTED_PROMPT_SIGNATURE.lock() {
                let sig_map = last_guard.get_or_insert_with(HashMap::new);
                if let Some((last_sig, last_at)) = sig_map.get(session_id) {
                    if *last_sig == sig
                        && last_at.elapsed().as_secs() < MEMORY_REPEAT_SUPPRESSION_SECS
                    {
                        crate::memory_log::log_pending_discarded(
                            session_id,
                            "duplicate suppressed",
                        );
                        return None;
                    }
                }
                sig_map.insert(session_id.to_string(), (sig, Instant::now()));
            }

            if !pending.memory_ids.is_empty() {
                mark_memories_injected(session_id, &pending.memory_ids);
            }

            crate::memory_log::log_pending_consumed(
                session_id,
                pending.count,
                pending.computed_at.elapsed().as_millis() as u64,
                pending.prompt.chars().count(),
            );

            return Some(pending);
        }
    }
    None
}

/// Store a pending memory result for the given session.
pub fn set_pending_memory(session_id: &str, prompt: String, count: usize) {
    set_pending_memory_with_ids(session_id, prompt, count, Vec::new());
}

/// Store a pending memory result with associated memory IDs for dedup tracking.
pub fn set_pending_memory_with_ids(
    session_id: &str,
    prompt: String,
    count: usize,
    memory_ids: Vec<String>,
) {
    set_pending_memory_with_ids_and_display(session_id, prompt, count, memory_ids, None);
}

/// Store a pending memory result with associated memory IDs and optional display-only content.
pub fn set_pending_memory_with_ids_and_display(
    session_id: &str,
    prompt: String,
    count: usize,
    memory_ids: Vec<String>,
    display_prompt: Option<String>,
) {
    crate::memory_log::log_pending_prepared(session_id, &prompt, count, &memory_ids);

    if let Ok(mut guard) = PENDING_MEMORY.lock() {
        let map = guard.get_or_insert_with(HashMap::new);
        map.insert(
            session_id.to_string(),
            PendingMemory {
                prompt,
                display_prompt,
                computed_at: Instant::now(),
                count,
                memory_ids,
            },
        );
    }
}

/// Mark memory IDs as already injected for a session (prevents re-injection on future turns)
pub fn mark_memories_injected(session_id: &str, ids: &[String]) {
    crate::memory_log::log_marked_injected(session_id, ids);

    if let Ok(mut guard) = INJECTED_MEMORY_IDS.lock() {
        let outer = guard.get_or_insert_with(HashMap::new);
        let set = outer
            .entry(session_id.to_string())
            .or_insert_with(HashSet::new);
        for id in ids {
            set.insert(id.clone());
        }
        crate::logging::info(&format!(
            "[{}] Marked {} memory IDs as injected (total tracked: {})",
            session_id,
            ids.len(),
            set.len()
        ));
    }
}

/// Check if a memory ID has already been injected for a session
pub fn is_memory_injected(session_id: &str, id: &str) -> bool {
    if let Ok(guard) = INJECTED_MEMORY_IDS.lock() {
        if let Some(outer) = guard.as_ref() {
            if let Some(set) = outer.get(session_id) {
                return set.contains(id);
            }
        }
    }
    false
}

/// Check if a memory ID has already been injected in ANY session.
/// Used by the singleton memory agent which doesn't track per-session state.
pub fn is_memory_injected_any(id: &str) -> bool {
    if let Ok(guard) = INJECTED_MEMORY_IDS.lock() {
        if let Some(outer) = guard.as_ref() {
            return outer.values().any(|set| set.contains(id));
        }
    }
    false
}

/// Clear injected memory tracking for a session (call on session reset or topic change)
pub fn clear_injected_memories(session_id: &str) {
    if let Ok(mut guard) = INJECTED_MEMORY_IDS.lock() {
        if let Some(outer) = guard.as_mut() {
            if let Some(set) = outer.remove(session_id) {
                if !set.is_empty() {
                    crate::logging::info(&format!(
                        "[{}] Clearing {} tracked injected memory IDs",
                        session_id,
                        set.len()
                    ));
                }
            }
        }
    }
}

/// Clear all injected memory tracking across all sessions
pub fn clear_all_injected_memories() {
    if let Ok(mut guard) = INJECTED_MEMORY_IDS.lock() {
        if let Some(outer) = guard.as_ref() {
            let total: usize = outer.values().map(|s| s.len()).sum();
            if total > 0 {
                crate::logging::info(&format!(
                    "Clearing {} tracked injected memory IDs across {} sessions",
                    total,
                    outer.len()
                ));
            }
        }
        *guard = None;
    }
}

/// Clear any pending memory result for a session.
pub fn clear_pending_memory(session_id: &str) {
    if let Ok(mut guard) = PENDING_MEMORY.lock() {
        if let Some(map) = guard.as_mut() {
            map.remove(session_id);
        }
    }
    if let Ok(mut guard) = LAST_INJECTED_PROMPT_SIGNATURE.lock() {
        if let Some(map) = guard.as_mut() {
            map.remove(session_id);
        }
    }
    clear_injected_memories(session_id);
}

/// Clear all pending memory state across all sessions.
pub fn clear_all_pending_memory() {
    if let Ok(mut guard) = PENDING_MEMORY.lock() {
        *guard = None;
    }
    if let Ok(mut guard) = LAST_INJECTED_PROMPT_SIGNATURE.lock() {
        *guard = None;
    }
    clear_all_injected_memories();
}

/// Check if there's a pending memory for a specific session
pub fn has_pending_memory(session_id: &str) -> bool {
    PENDING_MEMORY
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|m| m.contains_key(session_id)))
        .unwrap_or(false)
}

/// Check if there's any pending memory across all sessions
pub fn has_any_pending_memory() -> bool {
    PENDING_MEMORY
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|m| !m.is_empty()))
        .unwrap_or(false)
}

/// Get current memory activity state
pub fn get_activity() -> Option<MemoryActivity> {
    MEMORY_ACTIVITY.lock().ok().and_then(|guard| guard.clone())
}

/// Staleness timeout: auto-reset to Idle if state has been non-Idle for this long
const STALENESS_TIMEOUT_SECS: u64 = 10;

/// Update the memory activity state
pub fn set_state(state: MemoryState) {
    if let Ok(mut guard) = MEMORY_ACTIVITY.lock() {
        if let Some(activity) = guard.as_mut() {
            activity.state = state;
            activity.state_since = Instant::now();
        } else {
            *guard = Some(MemoryActivity {
                state,
                state_since: Instant::now(),
                pipeline: None,
                recent_events: Vec::new(),
            });
        }
    }
}

/// Add an event to the activity log
pub fn add_event(kind: MemoryEventKind) {
    crate::memory_log::log_event(&kind);

    if let Ok(mut guard) = MEMORY_ACTIVITY.lock() {
        let event = MemoryEvent {
            kind,
            timestamp: Instant::now(),
            detail: None,
        };

        if let Some(activity) = guard.as_mut() {
            activity.recent_events.insert(0, event);
            activity.recent_events.truncate(MAX_RECENT_EVENTS);
        } else {
            *guard = Some(MemoryActivity {
                state: MemoryState::Idle,
                state_since: Instant::now(),
                pipeline: None,
                recent_events: vec![event],
            });
        }
    }
}

/// Start a new pipeline run (called at the beginning of each memory check)
pub fn pipeline_start() {
    use crate::tui::info_widget::PipelineState;
    if let Ok(mut guard) = MEMORY_ACTIVITY.lock() {
        if let Some(activity) = guard.as_mut() {
            activity.pipeline = Some(PipelineState::new());
        } else {
            *guard = Some(MemoryActivity {
                state: MemoryState::Idle,
                state_since: Instant::now(),
                pipeline: Some(PipelineState::new()),
                recent_events: Vec::new(),
            });
        }
    }
}

/// Update pipeline step status
pub fn pipeline_update(f: impl FnOnce(&mut crate::tui::info_widget::PipelineState)) {
    if let Ok(mut guard) = MEMORY_ACTIVITY.lock() {
        if let Some(activity) = guard.as_mut() {
            if let Some(pipeline) = activity.pipeline.as_mut() {
                f(pipeline);
            }
        }
    }
}

/// Check for staleness and auto-reset if needed.
/// Returns true if state was reset due to staleness.
pub fn check_staleness() -> bool {
    if let Ok(mut guard) = MEMORY_ACTIVITY.lock() {
        if let Some(activity) = guard.as_mut() {
            if !matches!(activity.state, MemoryState::Idle)
                && activity.state_since.elapsed().as_secs() >= STALENESS_TIMEOUT_SECS
            {
                crate::logging::info(&format!(
                    "Memory state stale ({:?} for {}s), auto-resetting to Idle",
                    activity.state,
                    activity.state_since.elapsed().as_secs()
                ));
                activity.state = MemoryState::Idle;
                activity.state_since = Instant::now();
                return true;
            }
        }
    }
    false
}

/// Clear activity (reset to idle with no events)
pub fn clear_activity() {
    if let Ok(mut guard) = MEMORY_ACTIVITY.lock() {
        *guard = None;
    }
}

/// Record that a memory payload was injected into model context.
/// This feeds the memory info widget with injected content + metadata.
pub fn record_injected_prompt(prompt: &str, count: usize, age_ms: u64) {
    crate::telemetry::record_memory_injected(count, age_ms);
    let items = parse_injected_items(prompt, 8);
    let preview = prompt_preview(prompt, 72);
    add_event(MemoryEventKind::MemoryInjected {
        count,
        prompt_chars: prompt.chars().count(),
        age_ms,
        preview: preview.clone(),
        items,
    });
    add_event(MemoryEventKind::MemorySurfaced {
        memory_preview: preview,
    });
}

fn parse_injected_items(prompt: &str, max_items: usize) -> Vec<InjectedMemoryItem> {
    let mut items: Vec<InjectedMemoryItem> = Vec::new();
    let mut section = String::from("Memory");

    for raw_line in prompt.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line == "# Memory" {
            continue;
        }
        if let Some(header) = line.strip_prefix("## ") {
            let header = header.trim();
            if !header.is_empty() {
                section = header.to_string();
            }
            continue;
        }

        let content = if let Some(rest) = line.strip_prefix("- ") {
            Some(rest.trim())
        } else if let Some((prefix, rest)) = line.split_once(". ") {
            if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()) {
                Some(rest.trim())
            } else {
                None
            }
        } else {
            None
        };

        if let Some(content) = content {
            if content.is_empty() {
                continue;
            }
            items.push(InjectedMemoryItem {
                section: section.clone(),
                content: content.to_string(),
            });
            if items.len() >= max_items {
                return items;
            }
        }
    }

    if items.is_empty() {
        let fallback = prompt
            .lines()
            .map(str::trim)
            .filter(|line| {
                !line.is_empty()
                    && !line.starts_with('#')
                    && !line.starts_with("## ")
                    && !line.starts_with("- ")
            })
            .collect::<Vec<_>>()
            .join(" ");
        if !fallback.is_empty() {
            items.push(InjectedMemoryItem {
                section,
                content: fallback,
            });
        }
    }

    items
}

fn prompt_preview(prompt: &str, max_chars: usize) -> String {
    let bullet = prompt
        .lines()
        .map(str::trim)
        .find_map(|line| {
            if line.starts_with("- ") {
                Some(line.trim_start_matches("- ").trim())
            } else if let Some((prefix, rest)) = line.split_once(". ") {
                if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()) {
                    Some(rest.trim())
                } else {
                    None
                }
            } else {
                None
            }
        })
        .unwrap_or_else(|| prompt.trim());

    if bullet.chars().count() <= max_chars {
        bullet.to_string()
    } else {
        let mut out = String::new();
        for (i, ch) in bullet.chars().enumerate() {
            if i >= max_chars.saturating_sub(3) {
                break;
            }
            out.push(ch);
        }
        out.push_str("...");
        out
    }
}

/// Trust levels for memories
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum TrustLevel {
    /// User explicitly stated this
    High,
    /// Observed from user behavior
    #[default]
    Medium,
    /// Inferred by the agent
    Low,
}

/// A reinforcement breadcrumb tracking when/where a memory was reinforced
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reinforcement {
    pub session_id: String,
    pub message_index: usize,
    pub timestamp: DateTime<Utc>,
}

/// A single memory entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub category: MemoryCategory,
    pub content: String,
    pub tags: Vec<String>,
    /// Pre-normalized lowercase search text for content + tags.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub search_text: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub access_count: u32,
    pub source: Option<String>,
    /// Trust level for this memory
    #[serde(default)]
    pub trust: TrustLevel,
    /// Consolidation strength (how many times this was reinforced)
    #[serde(default)]
    pub strength: u32,
    /// Whether this memory is active or superseded
    #[serde(default = "default_active")]
    pub active: bool,
    /// ID of memory that superseded this one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// Reinforcement provenance (breadcrumbs of when/where this was reinforced)
    #[serde(default)]
    pub reinforcements: Vec<Reinforcement>,
    /// Embedding vector for similarity search (384 dimensions for MiniLM)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
    /// Confidence score (0.0-1.0) - decays over time, boosted by use
    #[serde(default = "default_confidence")]
    pub confidence: f32,
}

fn default_confidence() -> f32 {
    1.0
}

fn default_active() -> bool {
    true
}

impl MemoryEntry {
    pub fn new(category: MemoryCategory, content: impl Into<String>) -> Self {
        let now = Utc::now();
        let content = content.into();
        Self {
            id: crate::id::new_id("mem"),
            category,
            search_text: normalize_memory_search_text(&content, &[]),
            content,
            tags: Vec::new(),
            created_at: now,
            updated_at: now,
            access_count: 0,
            source: None,
            trust: TrustLevel::default(),
            strength: 1,
            active: true,
            superseded_by: None,
            reinforcements: Vec::new(),
            embedding: None,
            confidence: 1.0,
        }
    }

    pub fn refresh_search_text(&mut self) {
        self.search_text = normalize_memory_search_text(&self.content, &self.tags);
    }

    pub fn searchable_text(&self) -> std::borrow::Cow<'_, str> {
        if self.search_text.is_empty() {
            std::borrow::Cow::Owned(normalize_memory_search_text(&self.content, &self.tags))
        } else {
            std::borrow::Cow::Borrowed(&self.search_text)
        }
    }

    /// Get effective confidence after time-based decay
    /// Half-life varies by category:
    /// - Correction: 365 days (user corrections are high value)
    /// - Preference: 90 days (preferences may evolve)
    /// - Fact: 30 days (codebase facts can become stale)
    /// - Entity: 60 days (entities change moderately)
    pub fn effective_confidence(&self) -> f32 {
        let age_days = (Utc::now() - self.created_at).num_days() as f32;
        let half_life = match self.category {
            MemoryCategory::Correction => 365.0,
            MemoryCategory::Preference => 90.0,
            MemoryCategory::Fact => 30.0,
            MemoryCategory::Entity => 60.0,
            MemoryCategory::Custom(_) => 45.0, // Default for custom categories
        };

        // Exponential decay: confidence * e^(-age/half_life * ln(2))
        // Also boost slightly for access count
        let decay = (-age_days / half_life * 0.693).exp();
        let access_boost = 1.0 + 0.1 * (self.access_count as f32 + 1.0).ln();

        (self.confidence * decay * access_boost).min(1.0)
    }

    /// Boost confidence (called when memory was useful)
    pub fn boost_confidence(&mut self, amount: f32) {
        self.confidence = (self.confidence + amount).min(1.0);
        self.access_count += 1;
        self.updated_at = Utc::now();
    }

    /// Decay confidence (called when memory was retrieved but not relevant)
    pub fn decay_confidence(&mut self, amount: f32) {
        self.confidence = (self.confidence - amount).max(0.0);
    }

    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self.refresh_search_text();
        self
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn with_trust(mut self, trust: TrustLevel) -> Self {
        self.trust = trust;
        self
    }

    pub fn touch(&mut self) {
        self.updated_at = Utc::now();
        self.access_count += 1;
    }

    /// Reinforce this memory (called when same info is encountered again)
    pub fn reinforce(&mut self, session_id: &str, message_index: usize) {
        self.strength += 1;
        self.updated_at = Utc::now();
        self.reinforcements.push(Reinforcement {
            session_id: session_id.to_string(),
            message_index,
            timestamp: Utc::now(),
        });
    }

    /// Mark this memory as superseded by another
    pub fn supersede(&mut self, new_id: &str) {
        self.active = false;
        self.superseded_by = Some(new_id.to_string());
    }

    /// Set embedding vector
    pub fn with_embedding(mut self, embedding: Vec<f32>) -> Self {
        self.embedding = Some(embedding);
        self
    }

    /// Generate and set embedding if not already present
    /// Returns true if embedding was generated, false if already exists or failed
    pub fn ensure_embedding(&mut self) -> bool {
        if self.embedding.is_some() {
            return false;
        }

        match crate::embedding::embed(&self.content) {
            Ok(emb) => {
                self.embedding = Some(emb);
                true
            }
            Err(e) => {
                crate::logging::info(&format!("Failed to generate embedding: {}", e));
                false
            }
        }
    }

    /// Check if this memory has an embedding
    pub fn has_embedding(&self) -> bool {
        self.embedding.is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum MemoryCategory {
    Fact,
    Preference,
    Entity,
    Correction,
    Custom(String),
}

impl std::fmt::Display for MemoryCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryCategory::Fact => write!(f, "fact"),
            MemoryCategory::Preference => write!(f, "preference"),
            MemoryCategory::Entity => write!(f, "entity"),
            MemoryCategory::Correction => write!(f, "correction"),
            MemoryCategory::Custom(s) => write!(f, "{}", s),
        }
    }
}

impl std::str::FromStr for MemoryCategory {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_lowercase().as_str() {
            "fact" => MemoryCategory::Fact,
            "preference" => MemoryCategory::Preference,
            "entity" => MemoryCategory::Entity,
            "correction" => MemoryCategory::Correction,
            other => MemoryCategory::Custom(other.to_string()),
        })
    }
}

impl MemoryCategory {
    /// Parse a category string from LLM extraction output.
    /// Maps legacy/incorrect category names to the correct variant and avoids
    /// blindly defaulting to Fact.
    pub fn from_extracted(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "fact" | "facts" => MemoryCategory::Fact,
            "preference" | "preferences" | "pref" => MemoryCategory::Preference,
            "correction" | "corrections" | "fix" | "bug" => MemoryCategory::Correction,
            "entity" | "entities" => MemoryCategory::Entity,
            "observation" | "lesson" | "learning" => MemoryCategory::Fact,
            other => {
                crate::logging::info(&format!(
                    "Unknown memory category from extraction: '{}', defaulting to fact",
                    other
                ));
                MemoryCategory::Fact
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryStore {
    pub entries: Vec<MemoryEntry>,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, entry: MemoryEntry) -> String {
        let id = entry.id.clone();
        self.entries.push(entry);
        id
    }

    pub fn by_category(&self, category: &MemoryCategory) -> Vec<&MemoryEntry> {
        self.entries
            .iter()
            .filter(|e| &e.category == category)
            .collect()
    }

    pub fn search(&self, query: &str) -> Vec<&MemoryEntry> {
        let query_lower = normalize_search_text(query);
        if query_lower.is_empty() {
            return Vec::new();
        }

        self.entries
            .iter()
            .filter(|e| memory_matches_search(e, &query_lower))
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<&MemoryEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn remove(&mut self, id: &str) -> Option<MemoryEntry> {
        if let Some(pos) = self.entries.iter().position(|e| e.id == id) {
            Some(self.entries.remove(pos))
        } else {
            None
        }
    }

    pub fn get_relevant(&self, limit: usize) -> Vec<&MemoryEntry> {
        top_k_by_score(
            self.entries
                .iter()
                .filter(|entry| entry.active)
                .map(|entry| (entry, memory_score(entry) as f32)),
            limit,
        )
        .into_iter()
        .map(|(entry, _)| entry)
        .collect()
    }

    pub fn format_for_prompt(&self, limit: usize) -> Option<String> {
        let relevant: Vec<MemoryEntry> = self.get_relevant(limit).into_iter().cloned().collect();
        format_entries_for_prompt(&relevant, limit)
    }
}

const MEMORY_CONTEXT_MAX_CHARS: usize = 8_000;
const MEMORY_CONTEXT_MAX_MESSAGES: usize = 12;
const MEMORY_CONTEXT_MAX_BLOCK_CHARS: usize = 1_200;
const MEMORY_RELEVANCE_MAX_CANDIDATES: usize = 30;
const MEMORY_RELEVANCE_MAX_RESULTS: usize = 10;

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    value.chars().take(max_chars).collect()
}

fn format_content_block(block: &crate::message::ContentBlock) -> Option<String> {
    match block {
        crate::message::ContentBlock::Text { text, .. } => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(truncate_chars(trimmed, MEMORY_CONTEXT_MAX_BLOCK_CHARS))
            }
        }
        crate::message::ContentBlock::ToolUse { name, input, .. } => {
            let input_str =
                serde_json::to_string(input).unwrap_or_else(|_| "<invalid json>".into());
            let input_str = truncate_chars(&input_str, MEMORY_CONTEXT_MAX_BLOCK_CHARS / 2);
            Some(format!("[Tool: {} input: {}]", name, input_str))
        }
        crate::message::ContentBlock::ToolResult {
            content, is_error, ..
        } => {
            let label = if is_error.unwrap_or(false) {
                "Tool error"
            } else {
                "Tool result"
            };
            let content = truncate_chars(content, MEMORY_CONTEXT_MAX_BLOCK_CHARS / 2);
            Some(format!("[{}: {}]", label, content))
        }
        crate::message::ContentBlock::Reasoning { .. } => None,
        crate::message::ContentBlock::Image { .. } => Some("[Image]".to_string()),
        crate::message::ContentBlock::OpenAICompaction { .. } => {
            Some("[OpenAI native compaction]".to_string())
        }
    }
}

fn format_message_context(message: &crate::message::Message) -> String {
    let role = match message.role {
        crate::message::Role::User => "User",
        crate::message::Role::Assistant => "Assistant",
    };

    let mut chunk = String::new();
    chunk.push_str(role);
    chunk.push_str(":\n");

    let mut has_content = false;
    for block in &message.content {
        if let Some(text) = format_content_block(block) {
            if !text.is_empty() {
                has_content = true;
                chunk.push_str(&text);
                chunk.push('\n');
            }
        }
    }

    if has_content { chunk } else { String::new() }
}

/// Format messages into a context string for relevance checking
pub fn format_context_for_relevance(messages: &[crate::message::Message]) -> String {
    let mut chunks: Vec<String> = Vec::new();
    let mut total_chars = 0usize;

    for message in messages.iter().rev().take(MEMORY_CONTEXT_MAX_MESSAGES) {
        let chunk = format_message_context(message);
        if chunk.is_empty() {
            continue;
        }
        let chunk_len = chunk.chars().count();
        if total_chars + chunk_len > MEMORY_CONTEXT_MAX_CHARS {
            if total_chars == 0 {
                chunks.push(truncate_chars(&chunk, MEMORY_CONTEXT_MAX_CHARS));
            }
            break;
        }
        total_chars += chunk_len;
        chunks.push(chunk);
    }

    chunks.reverse();
    chunks.join("\n").trim().to_string()
}

/// Format messages into a wider context string for extraction.
/// Uses a larger window than relevance checking since extraction needs to
/// capture learnings from a broader portion of the conversation.
const EXTRACTION_CONTEXT_MAX_MESSAGES: usize = 40;
const EXTRACTION_CONTEXT_MAX_CHARS: usize = 24_000;

pub fn format_context_for_extraction(messages: &[crate::message::Message]) -> String {
    let mut chunks: Vec<String> = Vec::new();
    let mut total_chars = 0usize;

    for message in messages.iter().rev().take(EXTRACTION_CONTEXT_MAX_MESSAGES) {
        let chunk = format_message_context(message);
        if chunk.is_empty() {
            continue;
        }
        let chunk_len = chunk.chars().count();
        if total_chars + chunk_len > EXTRACTION_CONTEXT_MAX_CHARS {
            if total_chars == 0 {
                chunks.push(truncate_chars(&chunk, EXTRACTION_CONTEXT_MAX_CHARS));
            }
            break;
        }
        total_chars += chunk_len;
        chunks.push(chunk);
    }

    chunks.reverse();
    chunks.join("\n").trim().to_string()
}

fn selected_entries_for_prompt<'a>(
    entries: &'a [MemoryEntry],
    limit: usize,
) -> Vec<&'a MemoryEntry> {
    let mut selected = Vec::new();
    let mut seen_content = HashSet::new();

    for entry in entries.iter().filter(|e| e.active) {
        if selected.len() >= limit {
            break;
        }

        let dedupe_key = entry
            .content
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        if dedupe_key.is_empty() || !seen_content.insert(dedupe_key) {
            continue;
        }

        selected.push(entry);
    }

    selected
}

fn format_entries_for_prompt(entries: &[MemoryEntry], limit: usize) -> Option<String> {
    let mut sections: HashMap<MemoryCategory, Vec<&MemoryEntry>> = HashMap::new();

    for entry in selected_entries_for_prompt(entries, limit) {
        sections
            .entry(entry.category.clone())
            .or_default()
            .push(entry);
    }

    if sections.is_empty() {
        return None;
    }

    let mut output = String::new();
    let order = [
        MemoryCategory::Correction,
        MemoryCategory::Fact,
        MemoryCategory::Preference,
        MemoryCategory::Entity,
    ];

    let mut write_section = |title: &str, items: Vec<&MemoryEntry>| {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&format!("## {}\n", title));
        for (idx, item) in items.into_iter().enumerate() {
            output.push_str(&format!("{}. {}\n", idx + 1, item.content.trim()));
        }
    };

    for cat in &order {
        if let Some(items) = sections.remove(cat) {
            let title = match cat {
                MemoryCategory::Correction => "Corrections",
                MemoryCategory::Fact => "Facts",
                MemoryCategory::Preference => "Preferences",
                MemoryCategory::Entity => "Entities",
                MemoryCategory::Custom(_) => "Custom",
            };
            write_section(title, items);
        }
    }

    let mut custom_sections: BTreeMap<String, Vec<&MemoryEntry>> = BTreeMap::new();
    for (cat, items) in sections {
        match cat {
            MemoryCategory::Custom(name) => {
                custom_sections.insert(name, items);
            }
            other => {
                custom_sections.insert(other.to_string(), items);
            }
        }
    }
    for (name, items) in custom_sections {
        write_section(&name, items);
    }

    if output.is_empty() {
        None
    } else {
        Some(output.trim().to_string())
    }
}

pub(crate) fn format_relevant_prompt(entries: &[MemoryEntry], limit: usize) -> Option<String> {
    format_entries_for_prompt(entries, limit).map(|formatted| format!("# Memory\n\n{}", formatted))
}

pub(crate) fn format_relevant_display_prompt(
    entries: &[MemoryEntry],
    limit: usize,
) -> Option<String> {
    let mut sections: HashMap<MemoryCategory, Vec<&MemoryEntry>> = HashMap::new();

    for entry in selected_entries_for_prompt(entries, limit) {
        sections
            .entry(entry.category.clone())
            .or_default()
            .push(entry);
    }

    if sections.is_empty() {
        return None;
    }

    let mut output = String::new();
    let order = [
        MemoryCategory::Correction,
        MemoryCategory::Fact,
        MemoryCategory::Preference,
        MemoryCategory::Entity,
    ];

    let mut write_section = |title: &str, items: Vec<&MemoryEntry>| {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&format!("## {}\n", title));
        for (idx, item) in items.into_iter().enumerate() {
            output.push_str(&format!("{}. {}\n", idx + 1, item.content.trim()));
            output.push_str(&format!(
                "<!-- updated_at: {} -->\n",
                item.updated_at.to_rfc3339()
            ));
        }
    };

    for cat in &order {
        if let Some(items) = sections.remove(cat) {
            let title = match cat {
                MemoryCategory::Correction => "Corrections",
                MemoryCategory::Fact => "Facts",
                MemoryCategory::Preference => "Preferences",
                MemoryCategory::Entity => "Entities",
                MemoryCategory::Custom(_) => "Custom",
            };
            write_section(title, items);
        }
    }

    let mut custom_sections: BTreeMap<String, Vec<&MemoryEntry>> = BTreeMap::new();
    for (cat, items) in sections {
        match cat {
            MemoryCategory::Custom(name) => {
                custom_sections.insert(name, items);
            }
            other => {
                custom_sections.insert(other.to_string(), items);
            }
        }
    }
    for (name, items) in custom_sections {
        write_section(&name, items);
    }

    if output.is_empty() {
        None
    } else {
        Some(format!("# Memory\n\n{}", output.trim()))
    }
}

fn memory_score(entry: &MemoryEntry) -> f64 {
    // Skip inactive memories
    if !entry.active {
        return 0.0;
    }

    let mut score = 0.0;

    // Recency factor (decays over time)
    let age_hours = (Utc::now() - entry.updated_at).num_hours() as f64;
    score += 100.0 / (1.0 + age_hours / 24.0);

    // Access frequency bonus
    score += (entry.access_count as f64).sqrt() * 10.0;

    // Category importance
    score += match entry.category {
        MemoryCategory::Correction => 50.0,
        MemoryCategory::Preference => 30.0,
        MemoryCategory::Fact => 20.0,
        MemoryCategory::Entity => 10.0,
        MemoryCategory::Custom(_) => 5.0,
    };

    // Trust level multiplier
    score *= match entry.trust {
        TrustLevel::High => 1.5,
        TrustLevel::Medium => 1.0,
        TrustLevel::Low => 0.7,
    };

    // Consolidation strength bonus
    score += (entry.strength as f64).ln() * 5.0;

    score
}

#[derive(Debug)]
struct TopKItem<T> {
    score: f32,
    ordinal: usize,
    value: T,
}

impl<T> PartialEq for TopKItem<T> {
    fn eq(&self, other: &Self) -> bool {
        self.score.to_bits() == other.score.to_bits() && self.ordinal == other.ordinal
    }
}

impl<T> Eq for TopKItem<T> {}

impl<T> PartialOrd for TopKItem<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for TopKItem<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| self.ordinal.cmp(&other.ordinal))
    }
}

fn top_k_by_score<T, I>(items: I, limit: usize) -> Vec<(T, f32)>
where
    I: IntoIterator<Item = (T, f32)>,
{
    if limit == 0 {
        return Vec::new();
    }

    let mut heap: BinaryHeap<Reverse<TopKItem<T>>> = BinaryHeap::new();

    for (ordinal, (value, score)) in items.into_iter().enumerate() {
        let candidate = Reverse(TopKItem {
            score,
            ordinal,
            value,
        });

        if heap.len() < limit {
            heap.push(candidate);
            continue;
        }

        let replace = heap
            .peek()
            .map(|smallest| score > smallest.0.score)
            .unwrap_or(false);
        if replace {
            heap.pop();
            heap.push(candidate);
        }
    }

    let mut results: Vec<_> = heap
        .into_iter()
        .map(|Reverse(item)| (item.value, item.score, item.ordinal))
        .collect();
    results.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.2.cmp(&b.2)));
    results
        .into_iter()
        .map(|(value, score, _)| (value, score))
        .collect()
}

#[derive(Debug)]
struct TopKOrdItem<T, K> {
    key: K,
    ordinal: usize,
    value: T,
}

impl<T, K: Ord> PartialEq for TopKOrdItem<T, K> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.ordinal == other.ordinal
    }
}

impl<T, K: Ord> Eq for TopKOrdItem<T, K> {}

impl<T, K: Ord> PartialOrd for TopKOrdItem<T, K> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T, K: Ord> Ord for TopKOrdItem<T, K> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key
            .cmp(&other.key)
            .then_with(|| self.ordinal.cmp(&other.ordinal))
    }
}

fn top_k_by_ord<T, K, I>(items: I, limit: usize) -> Vec<(T, K)>
where
    I: IntoIterator<Item = (T, K)>,
    K: Ord,
{
    if limit == 0 {
        return Vec::new();
    }

    let mut heap: BinaryHeap<Reverse<TopKOrdItem<T, K>>> = BinaryHeap::new();

    for (ordinal, (value, key)) in items.into_iter().enumerate() {
        let candidate = Reverse(TopKOrdItem {
            key,
            ordinal,
            value,
        });

        if heap.len() < limit {
            heap.push(candidate);
            continue;
        }

        let replace = heap
            .peek()
            .map(|smallest| candidate.0.key > smallest.0.key)
            .unwrap_or(false);
        if replace {
            heap.pop();
            heap.push(candidate);
        }
    }

    let mut results: Vec<_> = heap
        .into_iter()
        .map(|Reverse(item)| (item.value, item.key, item.ordinal))
        .collect();
    results.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(&b.2)));
    results
        .into_iter()
        .map(|(value, key, _)| (value, key))
        .collect()
}

fn normalize_search_text(text: &str) -> String {
    let lowered = text.trim().to_lowercase();
    let mut normalized = String::with_capacity(lowered.len());
    let mut last_was_space = true;

    for ch in lowered.chars() {
        let mapped = if ch.is_whitespace() || matches!(ch, '-' | '_' | '/' | '\\' | '.' | ':') {
            ' '
        } else {
            ch
        };

        if mapped == ' ' {
            if !last_was_space {
                normalized.push(' ');
                last_was_space = true;
            }
        } else {
            normalized.push(mapped);
            last_was_space = false;
        }
    }

    normalized.trim_end().to_string()
}

fn normalize_memory_search_text(content: &str, tags: &[String]) -> String {
    let normalized_content = normalize_search_text(content);
    let normalized_tags: Vec<String> = tags
        .iter()
        .map(|tag| normalize_search_text(tag))
        .filter(|tag| !tag.is_empty())
        .collect();

    if normalized_tags.is_empty() {
        return normalized_content;
    }

    if normalized_content.is_empty() {
        return normalized_tags.join(" ");
    }

    format!("{} {}", normalized_content, normalized_tags.join(" "))
}

fn memory_matches_search(memory: &MemoryEntry, normalized_query: &str) -> bool {
    memory.searchable_text().contains(normalized_query)
}

#[derive(Clone)]
pub struct MemoryManager {
    project_dir: Option<PathBuf>,
    /// When true, use isolated test storage instead of real memory
    test_mode: bool,
}

impl MemoryManager {
    pub fn new() -> Self {
        Self {
            project_dir: None,
            test_mode: false,
        }
    }

    pub fn with_project_dir(mut self, project_dir: impl Into<PathBuf>) -> Self {
        self.project_dir = Some(project_dir.into());
        self
    }

    /// Create a memory manager in test mode (isolated storage)
    pub fn new_test() -> Self {
        Self {
            project_dir: None,
            test_mode: true,
        }
    }

    /// Check if running in test mode
    pub fn is_test_mode(&self) -> bool {
        self.test_mode
    }

    /// Set test mode (for debug sessions)
    pub fn set_test_mode(&mut self, test_mode: bool) {
        self.test_mode = test_mode;
    }

    /// Clear all test memories (only works in test mode)
    pub fn clear_test_storage(&self) -> Result<()> {
        if !self.test_mode {
            anyhow::bail!("clear_test_storage only allowed in test mode");
        }

        let test_dir = storage::jcode_dir()?.join("memory").join("test");
        if test_dir.exists() {
            std::fs::remove_dir_all(&test_dir)?;
            crate::logging::info("Cleared test memory storage");
        }
        Ok(())
    }

    fn get_project_dir(&self) -> Option<PathBuf> {
        self.project_dir
            .clone()
            .or_else(|| std::env::current_dir().ok())
    }

    fn project_memory_path(&self) -> Result<Option<PathBuf>> {
        // In test mode, use test directory
        if self.test_mode {
            let test_dir = storage::jcode_dir()?.join("memory").join("test");
            std::fs::create_dir_all(&test_dir)?;
            return Ok(Some(test_dir.join("test_project.json")));
        }

        let project_dir = match self.get_project_dir() {
            Some(d) => d,
            None => return Ok(None),
        };

        let project_hash = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            project_dir.hash(&mut hasher);
            format!("{:016x}", hasher.finish())
        };

        let memory_dir = storage::jcode_dir()?.join("memory").join("projects");
        Ok(Some(memory_dir.join(format!("{}.json", project_hash))))
    }

    fn legacy_notes_path(&self) -> Result<Option<PathBuf>> {
        if self.test_mode {
            let test_dir = storage::jcode_dir()?.join("notes").join("test");
            std::fs::create_dir_all(&test_dir)?;
            return Ok(Some(test_dir.join("test_notes.json")));
        }

        let project_dir = match self.get_project_dir() {
            Some(d) => d,
            None => return Ok(None),
        };

        let project_hash = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            project_dir.hash(&mut hasher);
            format!("{:016x}", hasher.finish())
        };

        Ok(Some(
            storage::jcode_dir()?
                .join("notes")
                .join(format!("{}.json", project_hash)),
        ))
    }

    fn normalize_graph_search_text(graph: &mut MemoryGraph) -> bool {
        let mut changed = false;
        for memory in graph.memories.values_mut() {
            let expected = normalize_memory_search_text(&memory.content, &memory.tags);
            if memory.search_text != expected {
                memory.search_text = expected;
                changed = true;
            }
        }
        changed
    }

    fn import_legacy_notes_into_graph(&self, graph: &mut MemoryGraph) -> Result<bool> {
        let Some(path) = self.legacy_notes_path()? else {
            return Ok(false);
        };
        if !path.exists() {
            return Ok(false);
        }

        let legacy: LegacyNotesFile = storage::read_json(&path)?;
        if legacy.entries.is_empty() {
            return Ok(false);
        }

        let mut changed = false;
        for note in legacy.entries {
            if graph.memories.contains_key(&note.id) {
                continue;
            }

            let mut entry = MemoryEntry::new(
                MemoryCategory::Custom(LEGACY_NOTE_CATEGORY.to_string()),
                note.content,
            );
            entry.id = note.id;
            entry.created_at = note.created_at;
            entry.updated_at = note.created_at;
            entry.source = Some("legacy_remember_migration".to_string());
            if let Some(tag) = note.tag {
                entry.tags.push(tag);
            }
            entry.ensure_embedding();
            graph.add_memory(entry);
            changed = true;
        }

        Ok(changed)
    }

    fn global_memory_path(&self) -> Result<PathBuf> {
        if self.test_mode {
            let test_dir = storage::jcode_dir()?.join("memory").join("test");
            std::fs::create_dir_all(&test_dir)?;
            Ok(test_dir.join("test_global.json"))
        } else {
            Ok(storage::jcode_dir()?.join("memory").join("global.json"))
        }
    }

    pub fn load_project(&self) -> Result<MemoryStore> {
        match self.project_memory_path()? {
            Some(path) if path.exists() => storage::read_json(&path),
            _ => Ok(MemoryStore::new()),
        }
    }

    pub fn load_global(&self) -> Result<MemoryStore> {
        let path = self.global_memory_path()?;
        if path.exists() {
            storage::read_json(&path)
        } else {
            Ok(MemoryStore::new())
        }
    }

    pub fn save_project(&self, store: &MemoryStore) -> Result<()> {
        if let Some(path) = self.project_memory_path()? {
            storage::write_json(&path, store)?;
        }
        Ok(())
    }

    pub fn save_global(&self, store: &MemoryStore) -> Result<()> {
        let path = self.global_memory_path()?;
        storage::write_json(&path, store)
    }

    /// Similarity threshold for storage-layer dedup.
    /// Memories above this threshold are considered duplicates and reinforced instead.
    const STORAGE_DEDUP_THRESHOLD: f32 = 0.85;

    pub fn remember_project(&self, entry: MemoryEntry) -> Result<String> {
        let mut entry = entry;
        entry.ensure_embedding();

        let mut graph = self.load_project_graph()?;

        if let Some(ref emb) = entry.embedding {
            if let Some(existing_id) =
                Self::find_duplicate_in_graph(&graph, emb, Self::STORAGE_DEDUP_THRESHOLD)
            {
                if let Some(existing) = graph.get_memory_mut(&existing_id) {
                    existing.reinforce(entry.source.as_deref().unwrap_or("dedup"), 0);
                    self.save_project_graph(&graph)?;
                    return Ok(existing_id);
                }
            }

            // Cross-store dedup: also check global graph
            if let Ok(mut global_graph) = self.load_global_graph() {
                if let Some(existing_id) =
                    Self::find_duplicate_in_graph(&global_graph, emb, Self::STORAGE_DEDUP_THRESHOLD)
                {
                    if let Some(existing) = global_graph.get_memory_mut(&existing_id) {
                        existing.reinforce(entry.source.as_deref().unwrap_or("cross-dedup"), 0);
                        self.save_global_graph(&global_graph)?;
                        return Ok(existing_id);
                    }
                }
            }
        }

        let id = graph.add_memory(entry);
        self.save_project_graph(&graph)?;
        Ok(id)
    }

    pub fn remember_global(&self, entry: MemoryEntry) -> Result<String> {
        let mut entry = entry;
        entry.ensure_embedding();

        let mut graph = self.load_global_graph()?;

        if let Some(ref emb) = entry.embedding {
            if let Some(existing_id) =
                Self::find_duplicate_in_graph(&graph, emb, Self::STORAGE_DEDUP_THRESHOLD)
            {
                if let Some(existing) = graph.get_memory_mut(&existing_id) {
                    existing.reinforce(entry.source.as_deref().unwrap_or("dedup"), 0);
                    self.save_global_graph(&graph)?;
                    return Ok(existing_id);
                }
            }

            // Cross-store dedup: also check project graph
            if let Ok(mut project_graph) = self.load_project_graph() {
                if let Some(existing_id) = Self::find_duplicate_in_graph(
                    &project_graph,
                    emb,
                    Self::STORAGE_DEDUP_THRESHOLD,
                ) {
                    if let Some(existing) = project_graph.get_memory_mut(&existing_id) {
                        existing.reinforce(entry.source.as_deref().unwrap_or("cross-dedup"), 0);
                        self.save_project_graph(&project_graph)?;
                        return Ok(existing_id);
                    }
                }
            }
        }

        let id = graph.add_memory(entry);
        self.save_global_graph(&graph)?;
        Ok(id)
    }

    /// Insert or update a memory with a stable ID in the project graph.
    /// Preserves existing inbound/outbound graph relationships while refreshing
    /// content and tags.
    pub fn upsert_project_memory(&self, entry: MemoryEntry) -> Result<String> {
        let mut graph = self.load_project_graph()?;
        let id = Self::upsert_memory_in_graph(&mut graph, entry);
        self.save_project_graph(&graph)?;
        Ok(id)
    }

    /// Insert or update a memory with a stable ID in the global graph.
    /// Preserves existing inbound/outbound graph relationships while refreshing
    /// content and tags.
    pub fn upsert_global_memory(&self, entry: MemoryEntry) -> Result<String> {
        let mut graph = self.load_global_graph()?;
        let id = Self::upsert_memory_in_graph(&mut graph, entry);
        self.save_global_graph(&graph)?;
        Ok(id)
    }

    fn upsert_memory_in_graph(
        graph: &mut crate::memory_graph::MemoryGraph,
        mut entry: MemoryEntry,
    ) -> String {
        let id = entry.id.clone();
        entry.ensure_embedding();

        let Some(existing_snapshot) = graph.get_memory(&id).cloned() else {
            return graph.add_memory(entry);
        };

        let old_tags: std::collections::HashSet<String> =
            existing_snapshot.tags.iter().cloned().collect();
        let new_tags: std::collections::HashSet<String> = entry.tags.iter().cloned().collect();

        for tag in old_tags.difference(&new_tags) {
            graph.untag_memory(&id, tag);
        }
        for tag in new_tags.difference(&old_tags) {
            graph.tag_memory(&id, tag);
        }

        if let Some(existing) = graph.get_memory_mut(&id) {
            let content_changed = existing.content != entry.content;
            existing.category = entry.category;
            existing.content = entry.content;
            existing.tags = entry.tags;
            existing.updated_at = entry.updated_at;
            existing.source = entry.source;
            existing.trust = entry.trust;
            existing.active = entry.active;
            existing.superseded_by = entry.superseded_by;
            existing.confidence = entry.confidence;
            if content_changed {
                existing.embedding = None;
                existing.ensure_embedding();
            }
        }

        id
    }

    fn find_duplicate_in_graph(
        graph: &crate::memory_graph::MemoryGraph,
        query_emb: &[f32],
        threshold: f32,
    ) -> Option<String> {
        let mut best: Option<(String, f32)> = None;
        for entry in graph.active_memories() {
            if let Some(ref emb) = entry.embedding {
                let sim = crate::embedding::cosine_similarity(query_emb, emb);
                if sim >= threshold && best.as_ref().map(|(_, s)| sim > *s).unwrap_or(true) {
                    best = Some((entry.id.clone(), sim));
                }
            }
        }
        best.map(|(id, _)| id)
    }

    /// Find memories similar to the given text using embedding search
    /// Returns memories with similarity above threshold, sorted by similarity
    pub fn find_similar(
        &self,
        text: &str,
        threshold: f32,
        limit: usize,
    ) -> Result<Vec<(MemoryEntry, f32)>> {
        // Generate embedding for query text
        let query_embedding = match crate::embedding::embed(text) {
            Ok(emb) => emb,
            Err(e) => {
                crate::logging::info(&format!(
                    "Embedding failed, falling back to keyword search: {}",
                    e
                ));
                return Ok(Vec::new());
            }
        };

        self.find_similar_with_embedding(&query_embedding, threshold, limit)
    }

    pub fn find_similar_scoped(
        &self,
        text: &str,
        threshold: f32,
        limit: usize,
        scope: MemoryScope,
    ) -> Result<Vec<(MemoryEntry, f32)>> {
        let query_embedding = match crate::embedding::embed(text) {
            Ok(emb) => emb,
            Err(e) => {
                crate::logging::info(&format!(
                    "Embedding failed, falling back to keyword search: {}",
                    e
                ));
                return Ok(Vec::new());
            }
        };

        self.find_similar_with_embedding_scoped(&query_embedding, threshold, limit, scope)
    }

    /// Find memories similar to the given embedding
    pub fn find_similar_with_embedding(
        &self,
        query_embedding: &[f32],
        threshold: f32,
        limit: usize,
    ) -> Result<Vec<(MemoryEntry, f32)>> {
        let entries_with_emb = self.collect_all_memories_with_embeddings()?;
        Self::score_and_filter(entries_with_emb, query_embedding, threshold, limit)
    }

    pub fn find_similar_with_embedding_scoped(
        &self,
        query_embedding: &[f32],
        threshold: f32,
        limit: usize,
        scope: MemoryScope,
    ) -> Result<Vec<(MemoryEntry, f32)>> {
        let entries_with_emb = self.collect_memories_with_embeddings_scoped(scope)?;
        Self::score_and_filter(entries_with_emb, query_embedding, threshold, limit)
    }

    fn collect_all_memories_with_embeddings(&self) -> Result<Vec<MemoryEntry>> {
        self.collect_memories_with_embeddings_scoped(MemoryScope::All)
    }

    fn collect_memories_with_embeddings_scoped(
        &self,
        scope: MemoryScope,
    ) -> Result<Vec<MemoryEntry>> {
        let mut entries: Vec<MemoryEntry> = Vec::new();
        if scope.includes_project() {
            if let Ok(project) = self.load_project_graph() {
                entries.extend(
                    project
                        .active_memories()
                        .filter(|m| m.embedding.is_some())
                        .cloned(),
                );
            }
        }
        if scope.includes_global() {
            if let Ok(global) = self.load_global_graph() {
                entries.extend(
                    global
                        .active_memories()
                        .filter(|m| m.embedding.is_some())
                        .cloned(),
                );
            }
        }
        Ok(entries)
    }

    fn collect_memories_scoped(&self, scope: MemoryScope) -> Result<Vec<MemoryEntry>> {
        let mut entries = Vec::new();
        if scope.includes_project() {
            if let Ok(project) = self.load_project_graph() {
                entries.extend(project.all_memories().cloned());
            }
        }
        if scope.includes_global() {
            if let Ok(global) = self.load_global_graph() {
                entries.extend(global.all_memories().cloned());
            }
        }
        Ok(entries)
    }

    fn score_and_filter(
        entries: Vec<MemoryEntry>,
        query_embedding: &[f32],
        threshold: f32,
        limit: usize,
    ) -> Result<Vec<(MemoryEntry, f32)>> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }

        let emb_refs: Vec<&[f32]> = entries
            .iter()
            .map(|e| e.embedding.as_ref().unwrap().as_slice())
            .collect();
        let scores = crate::embedding::batch_cosine_similarity(query_embedding, &emb_refs);

        let scored = top_k_by_score(
            entries
                .into_iter()
                .zip(scores)
                .filter(|(_, sim)| *sim >= threshold),
            limit,
        );

        let scored = Self::apply_gap_filter(scored);

        Ok(scored)
    }

    /// Drop trailing low-relevance results by detecting natural gaps in the
    /// score distribution. If the top hit is 0.85 and the next cluster is
    /// 0.40-0.42, the 0.15+ gap tells us those lower results are noise.
    ///
    /// Algorithm: walk the sorted scores and cut when the drop from one score
    /// to the next exceeds `GAP_FACTOR` of the range (top - floor_threshold).
    fn apply_gap_filter(scored: Vec<(MemoryEntry, f32)>) -> Vec<(MemoryEntry, f32)> {
        if scored.len() <= 1 {
            return scored;
        }

        const GAP_FACTOR: f32 = 0.25;
        const MIN_KEEP: usize = 1;

        let top_score = scored[0].1;
        let range = (top_score - EMBEDDING_SIMILARITY_THRESHOLD).max(0.01);
        let max_gap = range * GAP_FACTOR;

        let mut keep = scored.len();
        for i in 1..scored.len() {
            let drop = scored[i - 1].1 - scored[i].1;
            if drop > max_gap && i >= MIN_KEEP {
                keep = i;
                break;
            }
        }

        scored.into_iter().take(keep).collect()
    }

    /// Ensure all memories have embeddings (backfill for existing memories)
    pub fn backfill_embeddings(&self) -> Result<(usize, usize)> {
        let mut generated = 0;
        let mut failed = 0;

        // Process project memories
        if let Ok(mut graph) = self.load_project_graph() {
            let mut changed = false;
            for entry in graph.memories.values_mut() {
                if entry.embedding.is_none() {
                    if entry.ensure_embedding() {
                        generated += 1;
                        changed = true;
                    } else {
                        failed += 1;
                    }
                }
            }
            if changed {
                self.save_project_graph(&graph)?;
            }
        }

        // Process global memories
        if let Ok(mut graph) = self.load_global_graph() {
            let mut changed = false;
            for entry in graph.memories.values_mut() {
                if entry.embedding.is_none() {
                    if entry.ensure_embedding() {
                        generated += 1;
                        changed = true;
                    } else {
                        failed += 1;
                    }
                }
            }
            if changed {
                self.save_global_graph(&graph)?;
            }
        }

        Ok((generated, failed))
    }

    fn touch_entries(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }

        let id_set: std::collections::HashSet<&str> = ids.iter().map(|id| id.as_str()).collect();

        let mut project = self.load_project_graph()?;
        let mut project_changed = false;
        for entry in project.memories.values_mut() {
            if id_set.contains(entry.id.as_str()) {
                entry.touch();
                project_changed = true;
            }
        }
        if project_changed {
            self.save_project_graph(&project)?;
        }

        let mut global = self.load_global_graph()?;
        let mut global_changed = false;
        for entry in global.memories.values_mut() {
            if id_set.contains(entry.id.as_str()) {
                entry.touch();
                global_changed = true;
            }
        }
        if global_changed {
            self.save_global_graph(&global)?;
        }

        Ok(())
    }

    pub fn get_prompt_memories(&self, limit: usize) -> Option<String> {
        self.get_prompt_memories_scoped(limit, MemoryScope::All)
    }

    pub fn get_prompt_memories_scoped(&self, limit: usize, scope: MemoryScope) -> Option<String> {
        let all_entries: Vec<_> = top_k_by_ord(
            self.collect_memories_scoped(scope)
                .ok()?
                .into_iter()
                .map(|entry| {
                    let updated_at = entry.updated_at.timestamp_millis();
                    (entry, updated_at)
                }),
            limit,
        )
        .into_iter()
        .map(|(entry, _)| entry)
        .collect();

        if all_entries.is_empty() {
            return None;
        }

        format_entries_for_prompt(&all_entries, limit)
    }

    pub async fn relevant_prompt_for_messages(
        &self,
        messages: &[crate::message::Message],
    ) -> Result<Option<String>> {
        let context = format_context_for_relevance(messages);
        if context.is_empty() {
            return Ok(None);
        }
        self.relevant_prompt_for_context(
            &context,
            MEMORY_RELEVANCE_MAX_CANDIDATES,
            MEMORY_RELEVANCE_MAX_RESULTS,
        )
        .await
    }

    pub async fn relevant_prompt_for_context(
        &self,
        context: &str,
        max_candidates: usize,
        limit: usize,
    ) -> Result<Option<String>> {
        let relevant = self
            .get_relevant_for_context(context, max_candidates)
            .await?;
        if relevant.is_empty() {
            return Ok(None);
        }
        Ok(format_relevant_prompt(&relevant, limit))
    }

    pub fn search(&self, query: &str) -> Result<Vec<MemoryEntry>> {
        self.search_scoped(query, MemoryScope::All)
    }

    pub fn search_scoped(&self, query: &str, scope: MemoryScope) -> Result<Vec<MemoryEntry>> {
        let query_lower = normalize_search_text(query);
        if query_lower.is_empty() {
            return Ok(Vec::new());
        }

        let mut results = Vec::new();

        for memory in self.collect_memories_scoped(scope)? {
            if memory_matches_search(&memory, &query_lower) {
                results.push(memory);
            }
        }

        Ok(results)
    }

    pub fn list_all(&self) -> Result<Vec<MemoryEntry>> {
        self.list_all_scoped(MemoryScope::All)
    }

    pub fn list_all_scoped(&self, scope: MemoryScope) -> Result<Vec<MemoryEntry>> {
        let mut all = self.collect_memories_scoped(scope)?;
        all.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(all)
    }

    pub fn forget(&self, id: &str) -> Result<bool> {
        // Try graph-based removal first (new format)
        let mut project_graph = self.load_project_graph()?;
        if project_graph.remove_memory(id).is_some() {
            self.save_project_graph(&project_graph)?;
            return Ok(true);
        }

        let mut global_graph = self.load_global_graph()?;
        if global_graph.remove_memory(id).is_some() {
            self.save_global_graph(&global_graph)?;
            return Ok(true);
        }

        Ok(false)
    }

    // === Sidecar Integration ===

    /// Extract memories from a session transcript using the Haiku sidecar
    pub async fn extract_from_transcript(
        &self,
        transcript: &str,
        session_id: &str,
    ) -> Result<Vec<String>> {
        let sidecar = Sidecar::new();
        let extracted = sidecar.extract_memories(transcript).await?;

        let mut ids = Vec::new();
        for memory in extracted {
            let category: MemoryCategory = memory.category.parse().unwrap_or(MemoryCategory::Fact);
            let trust = match memory.trust.as_str() {
                "high" => TrustLevel::High,
                "medium" => TrustLevel::Medium,
                _ => TrustLevel::Low,
            };

            let entry = MemoryEntry::new(category, memory.content)
                .with_source(session_id)
                .with_trust(trust);

            // Store in project scope by default
            let id = self.remember_project(entry)?;
            ids.push(id);
        }

        Ok(ids)
    }

    /// Check if stored memories are relevant to the current context
    /// Returns memories that the sidecar deems relevant
    pub async fn get_relevant_for_context(
        &self,
        context: &str,
        max_candidates: usize,
    ) -> Result<Vec<MemoryEntry>> {
        // Get top candidate memories by score
        let candidates: Vec<_> = top_k_by_score(
            self.collect_memories_scoped(MemoryScope::All)?
                .into_iter()
                .filter(|entry| entry.active)
                .map(|entry| {
                    let score = memory_score(&entry) as f32;
                    (entry, score)
                }),
            max_candidates,
        )
        .into_iter()
        .map(|(entry, _)| entry)
        .collect();

        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        // Update activity state - checking memories
        set_state(MemoryState::SidecarChecking {
            count: candidates.len(),
        });
        add_event(MemoryEventKind::SidecarStarted);

        let sidecar = Sidecar::new();
        let mut relevant = Vec::new();
        let mut relevant_ids = Vec::new();

        for memory in candidates {
            let start = Instant::now();
            match sidecar.check_relevance(&memory.content, context).await {
                Ok((is_relevant, _reason)) => {
                    let latency_ms = start.elapsed().as_millis() as u64;
                    add_event(MemoryEventKind::SidecarComplete { latency_ms });

                    if is_relevant {
                        let preview = if memory.content.len() > 30 {
                            format!("{}...", crate::util::truncate_str(&memory.content, 30))
                        } else {
                            memory.content.clone()
                        };
                        add_event(MemoryEventKind::SidecarRelevant {
                            memory_preview: preview,
                        });
                        relevant_ids.push(memory.id.clone());
                        relevant.push(memory);
                    } else {
                        add_event(MemoryEventKind::SidecarNotRelevant);
                    }
                }
                Err(e) => {
                    add_event(MemoryEventKind::Error {
                        message: e.to_string(),
                    });
                    crate::logging::error(&format!("Sidecar relevance check failed: {}", e));
                }
            }
        }

        let _ = self.touch_entries(&relevant_ids);

        // Update final state
        if relevant.is_empty() {
            set_state(MemoryState::Idle);
        } else {
            set_state(MemoryState::FoundRelevant {
                count: relevant.len(),
            });
        }

        Ok(relevant)
    }

    /// Simple relevance check without sidecar (keyword-based)
    /// Use this for quick checks when sidecar is not needed
    pub fn get_relevant_keywords(
        &self,
        keywords: &[&str],
        limit: usize,
    ) -> Result<Vec<MemoryEntry>> {
        let normalized_keywords: Vec<String> = keywords
            .iter()
            .map(|keyword| normalize_search_text(keyword))
            .filter(|keyword| !keyword.is_empty())
            .collect();
        if normalized_keywords.is_empty() {
            return Ok(Vec::new());
        }

        let matches: Vec<_> = top_k_by_ord(
            self.collect_memories_scoped(MemoryScope::All)?
                .into_iter()
                .filter(|entry| {
                    let content_lower = normalize_search_text(&entry.content);
                    normalized_keywords
                        .iter()
                        .any(|kw| content_lower.contains(kw))
                })
                .map(|entry| {
                    let updated_at = entry.updated_at.timestamp_millis();
                    (entry, updated_at)
                }),
            limit,
        )
        .into_iter()
        .map(|(entry, _)| entry)
        .collect();

        Ok(matches)
    }

    // === Async Memory Checking ===

    /// Spawn a background task to check memory relevance for a specific session.
    /// Results are stored in PENDING_MEMORY keyed by session_id and can be retrieved
    /// with take_pending_memory(session_id).
    /// This method returns immediately and never blocks the caller.
    /// Only ONE memory check runs at a time per session - additional calls are ignored.
    pub fn spawn_relevance_check(
        &self,
        session_id: &str,
        messages: std::sync::Arc<[crate::message::Message]>,
    ) {
        let sid = session_id.to_string();

        // Only spawn if no check is currently in progress for this session
        {
            if let Ok(mut guard) = MEMORY_CHECK_IN_PROGRESS.lock() {
                let set = guard.get_or_insert_with(HashSet::new);
                if !set.insert(sid.clone()) {
                    return;
                }
            } else {
                return;
            }
        }

        let project_dir = self.project_dir.clone();

        tokio::spawn(async move {
            let manager = MemoryManager {
                project_dir: project_dir.or_else(|| std::env::current_dir().ok()),
                test_mode: false,
            };

            match manager.get_relevant_parallel(&sid, &messages).await {
                Ok((Some(prompt), memory_ids, display_prompt)) => {
                    let count = prompt
                        .lines()
                        .map(str::trim_start)
                        .filter(|line| {
                            line.starts_with("- ")
                                || line
                                    .split_once(". ")
                                    .map(|(prefix, _)| {
                                        !prefix.is_empty()
                                            && prefix.chars().all(|c| c.is_ascii_digit())
                                    })
                                    .unwrap_or(false)
                        })
                        .count()
                        .max(1);
                    set_pending_memory_with_ids_and_display(
                        &sid,
                        prompt,
                        count,
                        memory_ids,
                        display_prompt,
                    );
                    add_event(MemoryEventKind::SidecarComplete { latency_ms: 0 });
                }
                Ok((None, _, _)) => {
                    set_state(MemoryState::Idle);
                }
                Err(e) => {
                    crate::logging::error(&format!("Background memory check failed: {}", e));
                    add_event(MemoryEventKind::Error {
                        message: e.to_string(),
                    });
                    set_state(MemoryState::Idle);
                }
            }

            // Release the per-session guard when done
            if let Ok(mut guard) = MEMORY_CHECK_IN_PROGRESS.lock() {
                if let Some(set) = guard.as_mut() {
                    set.remove(&sid);
                }
            }
        });
    }

    /// Get relevant memories using embedding search + sidecar verification
    /// 1. Embed the context (fast, local, ~30ms)
    /// 2. Find similar memories by embedding (instant)
    /// 3. Only call sidecar for embedding hits (1-5 calls instead of 30)
    /// Returns (formatted_prompt, memory_ids) on success
    pub async fn get_relevant_parallel(
        &self,
        session_id: &str,
        messages: &[crate::message::Message],
    ) -> Result<(Option<String>, Vec<String>, Option<String>)> {
        let context = format_context_for_relevance(messages);
        if context.is_empty() {
            return Ok((None, Vec::new(), None));
        }

        // Start pipeline tracking
        pipeline_start();

        // Step 1: Embedding search (fast, local)
        set_state(MemoryState::Embedding);
        add_event(MemoryEventKind::EmbeddingStarted);
        pipeline_update(|p| p.search = StepStatus::Running);

        let embedding_start = Instant::now();
        let candidates =
            match self.find_similar(&context, EMBEDDING_SIMILARITY_THRESHOLD, EMBEDDING_MAX_HITS) {
                Ok(hits) => {
                    let latency_ms = embedding_start.elapsed().as_millis() as u64;
                    if hits.is_empty() {
                        add_event(MemoryEventKind::EmbeddingComplete {
                            latency_ms,
                            hits: 0,
                        });
                        pipeline_update(|p| {
                            p.search = StepStatus::Done;
                            p.search_result = Some(StepResult {
                                summary: "0 hits".to_string(),
                                latency_ms,
                            });
                            p.verify = StepStatus::Skipped;
                            p.inject = StepStatus::Skipped;
                            p.maintain = StepStatus::Skipped;
                        });
                        set_state(MemoryState::Idle);
                        return Ok((None, Vec::new(), None));
                    }
                    pipeline_update(|p| {
                        p.search = StepStatus::Done;
                        p.search_result = Some(StepResult {
                            summary: format!("{} hits", hits.len()),
                            latency_ms,
                        });
                    });
                    add_event(MemoryEventKind::EmbeddingComplete {
                        latency_ms,
                        hits: hits.len(),
                    });
                    hits
                }
                Err(e) => {
                    crate::logging::info(&format!("Embedding search failed, falling back: {}", e));
                    add_event(MemoryEventKind::Error {
                        message: e.to_string(),
                    });
                    pipeline_update(|p| {
                        p.search = StepStatus::Error;
                        p.search_result = Some(StepResult {
                            summary: "fallback".to_string(),
                            latency_ms: embedding_start.elapsed().as_millis() as u64,
                        });
                    });

                    top_k_by_score(
                        self.collect_memories_scoped(MemoryScope::All)?
                            .into_iter()
                            .filter(|entry| entry.active)
                            .map(|entry| {
                                let score = memory_score(&entry) as f32;
                                (entry, score)
                            }),
                        MEMORY_RELEVANCE_MAX_CANDIDATES,
                    )
                    .into_iter()
                    .map(|(entry, _)| (entry, 0.0))
                    .collect()
                }
            };

        // Filter out memories that have already been injected in this session
        let pre_filter_count = candidates.len();
        let candidates: Vec<_> = candidates
            .into_iter()
            .filter(|(entry, _)| !is_memory_injected_any(&entry.id))
            .collect();
        if candidates.len() < pre_filter_count {
            crate::logging::info(&format!(
                "Filtered out {} already-injected memories ({} -> {} candidates)",
                pre_filter_count - candidates.len(),
                pre_filter_count,
                candidates.len()
            ));
        }

        if candidates.is_empty() {
            pipeline_update(|p| {
                p.verify = StepStatus::Skipped;
                p.inject = StepStatus::Skipped;
                p.maintain = StepStatus::Skipped;
            });
            set_state(MemoryState::Idle);
            return Ok((None, Vec::new(), None));
        }

        // Step 2: Sidecar verification (only for embedding hits - much fewer calls!)
        let total_candidates = candidates.len();
        set_state(MemoryState::SidecarChecking {
            count: total_candidates,
        });
        add_event(MemoryEventKind::SidecarStarted);
        pipeline_update(|p| {
            p.verify = StepStatus::Running;
            p.verify_progress = Some((0, total_candidates));
        });

        let sidecar = Sidecar::new();
        let mut relevant = Vec::new();
        let mut relevant_ids = Vec::new();

        // Process in parallel batches
        const BATCH_SIZE: usize = 5;
        for batch in candidates.chunks(BATCH_SIZE) {
            let futures: Vec<_> = batch
                .iter()
                .map(|(memory, _sim)| {
                    let sidecar = sidecar.clone();
                    let content = memory.content.clone();
                    let ctx = context.clone();
                    async move {
                        let start = Instant::now();
                        let result = sidecar.check_relevance(&content, &ctx).await;
                        (result, start.elapsed())
                    }
                })
                .collect();

            let results = futures::future::join_all(futures).await;

            for ((memory, sim), (result, elapsed)) in batch.iter().zip(results) {
                match result {
                    Ok((is_relevant, _reason)) => {
                        add_event(MemoryEventKind::SidecarComplete {
                            latency_ms: elapsed.as_millis() as u64,
                        });

                        if is_relevant {
                            let preview = if memory.content.len() > 30 {
                                format!("{}...", crate::util::truncate_str(&memory.content, 30))
                            } else {
                                memory.content.clone()
                            };
                            add_event(MemoryEventKind::SidecarRelevant {
                                memory_preview: preview,
                            });
                            relevant_ids.push(memory.id.clone());
                            relevant.push(memory.clone());
                            crate::logging::info(&format!(
                                "[{}] Memory relevant (sim={:.2}): {}",
                                session_id,
                                sim,
                                crate::util::truncate_str(&memory.content, 50)
                            ));
                        } else {
                            add_event(MemoryEventKind::SidecarNotRelevant);
                        }
                    }
                    Err(e) => {
                        add_event(MemoryEventKind::Error {
                            message: e.to_string(),
                        });
                        crate::logging::info(&format!("Sidecar check failed: {}", e));
                    }
                }
                // Update verify progress
                let checked = relevant.len()
                    + batch.len().saturating_sub(
                        batch.len(), // approximate
                    );
                let _ = checked; // Progress updated below per-batch
            }
            // Update pipeline verify progress after each batch
            pipeline_update(|p| {
                p.verify_progress = Some((
                    relevant_ids.len()
                        + (total_candidates - candidates.len().min(total_candidates)),
                    total_candidates,
                ));
            });
        }

        let verify_latency_ms = embedding_start.elapsed().as_millis() as u64;
        let _ = self.touch_entries(&relevant_ids);

        if relevant.is_empty() {
            pipeline_update(|p| {
                p.verify = StepStatus::Done;
                p.verify_result = Some(StepResult {
                    summary: "0 relevant".to_string(),
                    latency_ms: verify_latency_ms,
                });
                p.inject = StepStatus::Skipped;
                p.maintain = StepStatus::Skipped;
            });
            set_state(MemoryState::Idle);
            return Ok((None, Vec::new(), None));
        }

        pipeline_update(|p| {
            p.verify = StepStatus::Done;
            p.verify_result = Some(StepResult {
                summary: format!("{} relevant", relevant.len()),
                latency_ms: verify_latency_ms,
            });
            p.inject = StepStatus::Running;
        });

        set_state(MemoryState::FoundRelevant {
            count: relevant.len(),
        });

        let prompt = format_relevant_prompt(&relevant, MEMORY_RELEVANCE_MAX_RESULTS);
        let display_prompt =
            format_relevant_display_prompt(&relevant, MEMORY_RELEVANCE_MAX_RESULTS);

        // Mark inject as done - the prompt is ready for injection
        pipeline_update(|p| {
            p.inject = StepStatus::Done;
            p.inject_result = Some(StepResult {
                summary: format!("{} memories", relevant.len()),
                latency_ms: 0,
            });
        });

        Ok((prompt, relevant_ids, display_prompt))
    }

    // ==================== Graph-Based Operations ====================

    /// Load project memories as a MemoryGraph with automatic migration
    pub fn load_project_graph(&self) -> Result<MemoryGraph> {
        let Some(path) = self.project_memory_path()? else {
            return Ok(MemoryGraph::new());
        };

        if !self.test_mode {
            if let Some(mut graph) = cached_graph(&path) {
                if Self::normalize_graph_search_text(&mut graph) {
                    cache_graph(path.clone(), &graph);
                }
                return Ok(graph);
            }
        }

        if path.exists() {
            // Try loading as MemoryGraph first
            if let Ok(graph) = storage::read_json::<MemoryGraph>(&path) {
                if graph.graph_version == GRAPH_VERSION {
                    let mut graph = graph;
                    let normalized = Self::normalize_graph_search_text(&mut graph);
                    if self.import_legacy_notes_into_graph(&mut graph)? {
                        self.save_project_graph(&graph)?;
                    } else if normalized {
                        storage::write_json(&path, &graph)?;
                    }
                    if !self.test_mode {
                        cache_graph(path, &graph);
                    }
                    return Ok(graph);
                }
            }

            // Fall back to legacy MemoryStore and migrate
            let store: MemoryStore = storage::read_json(&path)?;
            let mut graph = MemoryGraph::from_legacy_store(store);
            let _ = self.import_legacy_notes_into_graph(&mut graph)?;

            // Save migrated format (create backup first)
            let backup_path = path.with_extension("json.bak");
            if !backup_path.exists() {
                let _ = std::fs::copy(&path, &backup_path);
            }
            storage::write_json(&path, &graph)?;

            crate::logging::info(&format!(
                "Migrated memory store to graph format: {}",
                path.display()
            ));
            if !self.test_mode {
                cache_graph(path, &graph);
            }
            Ok(graph)
        } else {
            let mut graph = MemoryGraph::new();
            if self.import_legacy_notes_into_graph(&mut graph)? {
                self.save_project_graph(&graph)?;
            }
            if !self.test_mode {
                cache_graph(path, &graph);
            }
            Ok(graph)
        }
    }

    /// Load global memories as a MemoryGraph with automatic migration
    pub fn load_global_graph(&self) -> Result<MemoryGraph> {
        let path = self.global_memory_path()?;
        if !self.test_mode {
            if let Some(mut graph) = cached_graph(&path) {
                if Self::normalize_graph_search_text(&mut graph) {
                    cache_graph(path.clone(), &graph);
                }
                return Ok(graph);
            }
        }

        if path.exists() {
            // Try loading as MemoryGraph first
            if let Ok(graph) = storage::read_json::<MemoryGraph>(&path) {
                if graph.graph_version == GRAPH_VERSION {
                    let mut graph = graph;
                    if Self::normalize_graph_search_text(&mut graph) {
                        storage::write_json(&path, &graph)?;
                    }
                    if !self.test_mode {
                        cache_graph(path, &graph);
                    }
                    return Ok(graph);
                }
            }

            // Fall back to legacy MemoryStore and migrate
            let store: MemoryStore = storage::read_json(&path)?;
            let graph = MemoryGraph::from_legacy_store(store);

            // Save migrated format (create backup first)
            let backup_path = path.with_extension("json.bak");
            if !backup_path.exists() {
                let _ = std::fs::copy(&path, &backup_path);
            }
            storage::write_json(&path, &graph)?;

            crate::logging::info(&format!(
                "Migrated global memory store to graph format: {}",
                path.display()
            ));
            if !self.test_mode {
                cache_graph(path, &graph);
            }
            Ok(graph)
        } else {
            let graph = MemoryGraph::new();
            if !self.test_mode {
                cache_graph(path, &graph);
            }
            Ok(graph)
        }
    }

    /// Save project memories as a MemoryGraph
    pub fn save_project_graph(&self, graph: &MemoryGraph) -> Result<()> {
        if let Some(path) = self.project_memory_path()? {
            storage::write_json(&path, graph)?;
            if !self.test_mode {
                cache_graph(path, graph);
            }
        }
        Ok(())
    }

    /// Save global memories as a MemoryGraph
    pub fn save_global_graph(&self, graph: &MemoryGraph) -> Result<()> {
        let path = self.global_memory_path()?;
        storage::write_json(&path, graph)?;
        if !self.test_mode {
            cache_graph(path, graph);
        }
        Ok(())
    }

    /// Add a tag to a memory
    pub fn tag_memory(&self, memory_id: &str, tag: &str) -> Result<()> {
        // Try project first
        let mut graph = self.load_project_graph()?;
        if graph.memories.contains_key(memory_id) {
            graph.tag_memory(memory_id, tag);
            return self.save_project_graph(&graph);
        }

        // Try global
        let mut graph = self.load_global_graph()?;
        if graph.memories.contains_key(memory_id) {
            graph.tag_memory(memory_id, tag);
            return self.save_global_graph(&graph);
        }

        Err(anyhow::anyhow!("Memory not found: {}", memory_id))
    }

    /// Link two memories with a RelatesTo edge
    pub fn link_memories(&self, from_id: &str, to_id: &str, weight: f32) -> Result<()> {
        // Try project first
        let mut graph = self.load_project_graph()?;
        if graph.memories.contains_key(from_id) && graph.memories.contains_key(to_id) {
            graph.link_memories(from_id, to_id, weight);
            return self.save_project_graph(&graph);
        }

        // Try global
        let mut graph = self.load_global_graph()?;
        if graph.memories.contains_key(from_id) && graph.memories.contains_key(to_id) {
            graph.link_memories(from_id, to_id, weight);
            return self.save_global_graph(&graph);
        }

        // Cross-store links not supported for now
        Err(anyhow::anyhow!(
            "Both memories must be in the same store (project or global)"
        ))
    }

    /// Get memories related to a given memory via graph traversal
    pub fn get_related(&self, memory_id: &str, depth: usize) -> Result<Vec<MemoryEntry>> {
        // Find which store contains the memory
        let (mut graph, _is_project) = {
            let project_graph = self.load_project_graph()?;
            if project_graph.memories.contains_key(memory_id) {
                (project_graph, true)
            } else {
                let global_graph = self.load_global_graph()?;
                if global_graph.memories.contains_key(memory_id) {
                    (global_graph, false)
                } else {
                    return Err(anyhow::anyhow!("Memory not found: {}", memory_id));
                }
            }
        };

        // Use cascade retrieval to find related memories
        let results = graph.cascade_retrieve(&[memory_id.to_string()], &[1.0], depth, 20);

        // Collect memory entries (excluding the seed)
        let entries: Vec<MemoryEntry> = results
            .into_iter()
            .filter(|(id, _)| id != memory_id)
            .filter_map(|(id, _)| graph.get_memory(&id).cloned())
            .collect();

        Ok(entries)
    }

    /// Find similar memories with cascade retrieval through the graph
    ///
    /// This extends the basic embedding search by also traversing through
    /// tags to find related memories that might not have direct embedding similarity.
    pub fn find_similar_with_cascade(
        &self,
        text: &str,
        threshold: f32,
        limit: usize,
    ) -> Result<Vec<(MemoryEntry, f32)>> {
        self.find_similar_with_cascade_scoped(text, threshold, limit, MemoryScope::All)
    }

    pub fn find_similar_with_cascade_scoped(
        &self,
        text: &str,
        threshold: f32,
        limit: usize,
        scope: MemoryScope,
    ) -> Result<Vec<(MemoryEntry, f32)>> {
        // First, do basic embedding search
        let embedding_hits = self.find_similar_scoped(text, threshold, limit, scope)?;

        if embedding_hits.is_empty() {
            return Ok(Vec::new());
        }

        // Get seed IDs and scores
        let seed_ids: Vec<String> = embedding_hits.iter().map(|(e, _)| e.id.clone()).collect();
        let seed_scores: Vec<f32> = embedding_hits.iter().map(|(_, s)| *s).collect();

        // Load graphs and perform cascade retrieval
        let mut project_graph = if scope.includes_project() {
            Some(self.load_project_graph()?)
        } else {
            None
        };
        let mut global_graph = if scope.includes_global() {
            Some(self.load_global_graph()?)
        } else {
            None
        };

        // Cascade through project graph
        let project_cascade = project_graph
            .as_mut()
            .map(|graph| graph.cascade_retrieve(&seed_ids, &seed_scores, 2, limit * 2))
            .unwrap_or_default();

        // Cascade through global graph
        let global_cascade = global_graph
            .as_mut()
            .map(|graph| graph.cascade_retrieve(&seed_ids, &seed_scores, 2, limit * 2))
            .unwrap_or_default();

        // Merge results, keeping highest score for each memory
        let mut merged: std::collections::HashMap<String, f32> = std::collections::HashMap::new();

        for (id, score) in embedding_hits.iter() {
            merged.insert(id.id.clone(), *score);
        }
        for (id, score) in project_cascade {
            let existing = merged.get(&id).copied().unwrap_or(0.0);
            if score > existing {
                merged.insert(id, score);
            }
        }
        for (id, score) in global_cascade {
            let existing = merged.get(&id).copied().unwrap_or(0.0);
            if score > existing {
                merged.insert(id, score);
            }
        }

        // Look up entries and keep only the top-scoring results
        let results: Vec<(MemoryEntry, f32)> = top_k_by_score(
            merged.into_iter().filter_map(|(id, score)| {
                project_graph
                    .as_ref()
                    .and_then(|graph| graph.get_memory(&id))
                    .or_else(|| {
                        global_graph
                            .as_ref()
                            .and_then(|graph| graph.get_memory(&id))
                    })
                    .cloned()
                    .map(|entry| (entry, score))
            }),
            limit,
        );

        Ok(results)
    }

    /// Get graph statistics for display
    pub fn graph_stats(&self) -> Result<(usize, usize, usize, usize)> {
        let project = self.load_project_graph()?;
        let global = self.load_global_graph()?;

        let memories = project.memories.len() + global.memories.len();
        let tags = project.tags.len() + global.tags.len();
        let edges = project.edge_count() + global.edge_count();
        let clusters = project.clusters.len() + global.clusters.len();

        Ok((memories, tags, edges, clusters))
    }
}

/// Embedding similarity threshold (0.0 - 1.0)
/// Lower = more candidates, higher = fewer but more relevant
pub const EMBEDDING_SIMILARITY_THRESHOLD: f32 = 0.5;

/// Maximum embedding hits to verify with sidecar
pub const EMBEDDING_MAX_HITS: usize = 10;

impl Default for MemoryManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ContentBlock, Message, Role};
    use serde_json::json;
    use std::fs;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    static PENDING_MEMORY_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn with_temp_home<F, T>(f: F) -> T
    where
        F: FnOnce(&Path) -> T,
    {
        let _guard = crate::storage::lock_test_env();
        let old = std::env::var("JCODE_HOME").ok();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("jcode-test-{}", unique));
        fs::create_dir_all(&dir).expect("create temp dir");
        crate::env::set_var("JCODE_HOME", &dir);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&dir)));

        match old {
            Some(value) => crate::env::set_var("JCODE_HOME", value),
            None => crate::env::remove_var("JCODE_HOME"),
        }
        let _ = fs::remove_dir_all(&dir);

        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    #[test]
    fn pending_memory_freshness_and_clear() {
        let _guard = PENDING_MEMORY_TEST_LOCK
            .lock()
            .expect("pending memory test lock poisoned");
        clear_all_pending_memory();

        let sid = "test-session-1";
        set_pending_memory(sid, "hello".to_string(), 2);
        assert!(has_pending_memory(sid));
        let pending = take_pending_memory(sid).expect("pending memory");
        assert_eq!(pending.prompt, "hello");
        assert_eq!(pending.count, 2);
        assert!(!has_pending_memory(sid));

        {
            let mut guard = PENDING_MEMORY.lock().expect("pending memory lock");
            let map = guard.get_or_insert_with(HashMap::new);
            map.insert(
                sid.to_string(),
                PendingMemory {
                    prompt: "stale".to_string(),
                    display_prompt: None,
                    computed_at: Instant::now() - Duration::from_secs(121),
                    count: 1,
                    memory_ids: Vec::new(),
                },
            );
        }
        assert!(take_pending_memory(sid).is_none());
    }

    #[test]
    fn pending_memory_suppresses_immediate_duplicate_payloads() {
        let _guard = PENDING_MEMORY_TEST_LOCK
            .lock()
            .expect("pending memory test lock poisoned");
        clear_all_pending_memory();

        let sid = "test-session-2";
        set_pending_memory(sid, "same payload".to_string(), 1);
        assert!(take_pending_memory(sid).is_some());

        set_pending_memory(sid, "same payload".to_string(), 1);
        assert!(
            take_pending_memory(sid).is_none(),
            "identical payload should be suppressed when repeated immediately"
        );
    }

    #[test]
    fn pending_memory_per_session_isolation() {
        let _guard = PENDING_MEMORY_TEST_LOCK
            .lock()
            .expect("pending memory test lock poisoned");
        clear_all_pending_memory();

        let sid_a = "test-session-a";
        let sid_b = "test-session-b";

        set_pending_memory(sid_a, "memory for A".to_string(), 1);
        set_pending_memory(sid_b, "memory for B".to_string(), 2);

        assert!(has_pending_memory(sid_a));
        assert!(has_pending_memory(sid_b));

        let pending_a = take_pending_memory(sid_a).expect("session A should have pending memory");
        assert_eq!(pending_a.prompt, "memory for A");
        assert!(!has_pending_memory(sid_a));

        // Session B's memory should still be there
        assert!(has_pending_memory(sid_b));
        let pending_b = take_pending_memory(sid_b).expect("session B should have pending memory");
        assert_eq!(pending_b.prompt, "memory for B");
        assert_eq!(pending_b.count, 2);
    }

    #[test]
    fn format_context_includes_roles_and_tools() {
        let messages = vec![
            Message::user("Hello world"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "tool-1".to_string(),
                    name: "memory".to_string(),
                    input: json!({"action": "list"}),
                }],
                timestamp: None,
                tool_duration_ms: None,
            },
            Message::tool_result("tool-1", "ok", false),
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "tool-2".to_string(),
                    content: "boom".to_string(),
                    is_error: Some(true),
                }],
                timestamp: None,
                tool_duration_ms: None,
            },
        ];

        let context = format_context_for_relevance(&messages);
        assert!(context.contains("User:\nHello world"));
        assert!(context.contains("[Tool: memory input:"));
        assert!(context.contains("[Tool result: ok]"));
        assert!(context.contains("[Tool error: boom]"));
    }

    #[test]
    fn memory_store_format_groups_by_category() {
        let mut store = MemoryStore::new();
        let now = Utc::now();
        let mut correction = MemoryEntry::new(MemoryCategory::Correction, "Fix lint rules");
        correction.updated_at = now;
        let mut fact = MemoryEntry::new(MemoryCategory::Fact, "Uses tokio");
        fact.updated_at = now;
        let mut preference =
            MemoryEntry::new(MemoryCategory::Preference, "Prefers ASCII-only edits");
        preference.updated_at = now;
        let mut entity = MemoryEntry::new(MemoryCategory::Entity, "Jeremy");
        entity.updated_at = now;
        let mut custom = MemoryEntry::new(MemoryCategory::Custom("team".to_string()), "Platform");
        custom.updated_at = now;

        store.add(correction);
        store.add(fact);
        store.add(preference);
        store.add(entity);
        store.add(custom);

        let output = store.format_for_prompt(10).expect("formatted output");
        let correction_idx = output.find("## Corrections").expect("correction heading");
        let fact_idx = output.find("## Facts").expect("fact heading");
        let preference_idx = output.find("## Preferences").expect("preference heading");
        let entity_idx = output.find("## Entities").expect("entity heading");
        let custom_idx = output.find("## team").expect("custom heading");

        assert!(correction_idx < fact_idx);
        assert!(fact_idx < preference_idx);
        assert!(preference_idx < entity_idx);
        assert!(entity_idx < custom_idx);
    }

    #[test]
    fn memory_store_search_matches_content_and_tags() {
        let mut store = MemoryStore::new();
        let entry = MemoryEntry::new(MemoryCategory::Fact, "Uses Tokio runtime")
            .with_tags(vec!["async".to_string()]);
        store.add(entry);

        let content_hits = store.search("tokio");
        assert_eq!(content_hits.len(), 1);

        let tag_hits = store.search("ASYNC");
        assert_eq!(tag_hits.len(), 1);
    }

    #[test]
    fn memory_search_normalizes_whitespace_and_separators() {
        let mut store = MemoryStore::new();
        let entry = MemoryEntry::new(MemoryCategory::Fact, "Uses side panel layout")
            .with_tags(vec!["build_cache".to_string()]);
        store.add(entry);

        assert_eq!(store.search("  side-panel  ").len(), 1);
        assert_eq!(store.search("BUILD.CACHE").len(), 1);
        assert!(store.search("   ").is_empty());
    }

    #[test]
    fn manager_persists_and_forgets_memories() {
        with_temp_home(|_dir| {
            let manager = MemoryManager::new_test();
            let entry_project = MemoryEntry::new(MemoryCategory::Fact, "Project memory")
                .with_embedding(vec![1.0, 0.0, 0.0]);
            let entry_global = MemoryEntry::new(MemoryCategory::Preference, "Global memory")
                .with_embedding(vec![0.0, 1.0, 0.0]);

            let project_id = manager
                .remember_project(entry_project)
                .expect("remember project");
            let global_id = manager
                .remember_global(entry_global)
                .expect("remember global");

            let all = manager.list_all().expect("list all");
            assert_eq!(all.len(), 2);

            let search = manager.search("global").expect("search");
            assert_eq!(search.len(), 1);

            assert!(manager.forget(&project_id).expect("forget project"));
            let remaining = manager.list_all().expect("list all");
            assert_eq!(remaining.len(), 1);

            assert!(!manager.forget(&project_id).expect("forget missing"));
            assert!(manager.forget(&global_id).expect("forget global"));
        });
    }

    #[test]
    fn graph_based_memory_operations() {
        with_temp_home(|_home| {
            let manager = MemoryManager::new_test();

            // Create two memories
            let entry1 = MemoryEntry::new(
                MemoryCategory::Fact,
                "The capital of France is Paris, a city known for the Eiffel Tower",
            );
            let entry2 = MemoryEntry::new(
                MemoryCategory::Fact,
                "Photosynthesis converts carbon dioxide and water into glucose using sunlight energy",
            );

            let id1 = manager.remember_project(entry1).expect("remember 1");
            let id2 = manager.remember_project(entry2).expect("remember 2");

            // Test tagging
            manager.tag_memory(&id1, "rust").expect("tag memory");
            manager.tag_memory(&id1, "language").expect("tag memory 2");
            manager.tag_memory(&id2, "rust").expect("tag memory 3");

            // Check graph stats (memories, tags, edges, clusters)
            let (mems, tags, edges, _clusters) = manager.graph_stats().expect("stats");
            assert_eq!(mems, 2, "expected 2 memories");
            assert_eq!(tags, 2, "expected 2 tags: rust and language");
            assert!(edges >= 3, "expected at least 3 edges, got {}", edges);

            // Test linking
            manager.link_memories(&id1, &id2, 0.8).expect("link");

            // Test get_related
            let related = manager.get_related(&id1, 2).expect("get related");
            assert!(!related.is_empty());
            // Should find id2 through the RelatesTo edge
            assert!(related.iter().any(|e| e.id == id2));

            // Clean up
            manager.forget(&id1).expect("forget 1");
            manager.forget(&id2).expect("forget 2");
        });
    }

    #[test]
    fn project_memories_are_isolated_by_explicit_project_dir() {
        with_temp_home(|_home| {
            let manager_a = MemoryManager::new().with_project_dir("/tmp/jcode-project-a");
            let manager_b = MemoryManager::new().with_project_dir("/tmp/jcode-project-b");

            manager_a
                .remember_project(MemoryEntry::new(
                    MemoryCategory::Fact,
                    "memory from project a",
                ))
                .expect("remember project a");
            manager_b
                .remember_project(MemoryEntry::new(
                    MemoryCategory::Fact,
                    "memory from project b",
                ))
                .expect("remember project b");

            let project_a: Vec<String> = manager_a
                .load_project_graph()
                .expect("load project a")
                .all_memories()
                .map(|m| m.content.clone())
                .collect();
            let project_b: Vec<String> = manager_b
                .load_project_graph()
                .expect("load project b")
                .all_memories()
                .map(|m| m.content.clone())
                .collect();

            assert_eq!(project_a, vec!["memory from project a".to_string()]);
            assert_eq!(project_b, vec!["memory from project b".to_string()]);
        });
    }

    #[test]
    fn manager_search_scoped_normalizes_whitespace_and_separators() {
        with_temp_home(|_home| {
            let manager = MemoryManager::new().with_project_dir("/tmp/jcode-search-normalization");

            manager
                .remember_project(MemoryEntry::new(
                    MemoryCategory::Fact,
                    "project compile notes",
                ))
                .expect("remember project");

            let hits = manager
                .search_scoped("  compile/notes  ", MemoryScope::Project)
                .expect("search project");
            assert_eq!(hits.len(), 1);
        });
    }

    #[test]
    fn prompt_memories_scoped_keeps_only_most_recent_entries() {
        with_temp_home(|_home| {
            let manager = MemoryManager::new().with_project_dir("/tmp/jcode-prompt-topk");

            let mut oldest = MemoryEntry::new(MemoryCategory::Fact, "oldest memory");
            oldest.created_at = Utc::now() - chrono::Duration::seconds(30);
            oldest.updated_at = oldest.created_at;

            let mut middle = MemoryEntry::new(MemoryCategory::Fact, "middle memory");
            middle.created_at = Utc::now() - chrono::Duration::seconds(20);
            middle.updated_at = middle.created_at;

            let mut newest = MemoryEntry::new(MemoryCategory::Fact, "newest memory");
            newest.created_at = Utc::now() - chrono::Duration::seconds(10);
            newest.updated_at = newest.created_at;

            manager.remember_project(oldest).expect("remember oldest");
            manager.remember_project(middle).expect("remember middle");
            manager.remember_project(newest).expect("remember newest");

            let prompt = manager
                .get_prompt_memories_scoped(2, MemoryScope::Project)
                .expect("prompt memories");

            assert!(prompt.contains("newest memory"));
            assert!(prompt.contains("middle memory"));
            assert!(!prompt.contains("oldest memory"));

            let newest_idx = prompt.find("newest memory").expect("newest in prompt");
            let middle_idx = prompt.find("middle memory").expect("middle in prompt");
            assert!(newest_idx < middle_idx);
        });
    }

    #[test]
    fn scoped_retrieval_respects_project_vs_global() {
        with_temp_home(|_home| {
            let manager = MemoryManager::new().with_project_dir("/tmp/jcode-scope-test");

            manager
                .remember_project(MemoryEntry::new(
                    MemoryCategory::Fact,
                    "project zebra compile notes",
                ))
                .expect("remember project");
            manager
                .remember_global(MemoryEntry::new(
                    MemoryCategory::Fact,
                    "global coffee preference",
                ))
                .expect("remember global");

            let project = manager
                .list_all_scoped(MemoryScope::Project)
                .expect("list project");
            let global = manager
                .list_all_scoped(MemoryScope::Global)
                .expect("list global");
            let all = manager.list_all_scoped(MemoryScope::All).expect("list all");

            assert_eq!(project.len(), 1);
            assert_eq!(project[0].content, "project zebra compile notes");
            assert_eq!(global.len(), 1);
            assert_eq!(global[0].content, "global coffee preference");
            assert_eq!(all.len(), 2);

            let project_search = manager
                .search_scoped("zebra", MemoryScope::Project)
                .expect("search project");
            let global_search = manager
                .search_scoped("coffee", MemoryScope::Global)
                .expect("search global");

            assert_eq!(project_search.len(), 1);
            assert_eq!(project_search[0].content, "project zebra compile notes");
            assert_eq!(global_search.len(), 1);
            assert_eq!(global_search[0].content, "global coffee preference");
        });
    }
}
