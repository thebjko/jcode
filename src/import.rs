//! Import Claude Code sessions into jcode
//!
//! This module handles discovering, parsing, and converting Claude Code sessions
//! so they can be resumed within jcode.

use crate::message::{ContentBlock, Role};
use crate::session::{Session, SessionStatus, StoredMessage};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::path::PathBuf;

/// Entry in the Claude Code sessions-index.json file
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIndexEntry {
    pub session_id: String,
    pub full_path: String,
    #[serde(default)]
    pub file_mtime: Option<u64>,
    #[serde(default)]
    pub first_prompt: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub message_count: Option<u32>,
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub modified: Option<String>,
    #[serde(default)]
    pub git_branch: Option<String>,
    #[serde(default)]
    pub project_path: Option<String>,
    #[serde(default)]
    pub is_sidechain: Option<bool>,
}

/// Claude Code sessions-index.json format
#[derive(Debug, Deserialize)]
pub struct SessionsIndex {
    pub version: u32,
    pub entries: Vec<SessionIndexEntry>,
}

/// Info about a Claude Code session for listing
#[derive(Debug, Clone)]
pub struct ClaudeCodeSessionInfo {
    pub session_id: String,
    pub first_prompt: String,
    pub summary: Option<String>,
    pub message_count: u32,
    pub created: Option<DateTime<Utc>>,
    pub modified: Option<DateTime<Utc>>,
    pub project_path: Option<String>,
    pub full_path: String,
}

/// Entry in a Claude Code JSONL session file
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeCodeEntry {
    #[serde(rename = "type")]
    entry_type: String,
    uuid: Option<String>,
    parent_uuid: Option<String>,
    #[serde(rename = "sessionId")]
    _session_id: Option<String>,
    cwd: Option<String>,
    message: Option<ClaudeCodeMessage>,
    timestamp: Option<String>,
    #[serde(default)]
    is_sidechain: bool,
}

/// Message content in Claude Code format
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeCodeMessage {
    role: String,
    #[serde(default)]
    model: Option<String>,
    // Content can be a string or array
    #[serde(default)]
    content: ClaudeCodeContent,
}

/// Content can be either a plain string or array of blocks
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(untagged)]
enum ClaudeCodeContent {
    #[default]
    Empty,
    Text(String),
    Blocks(Vec<ClaudeCodeContentBlock>),
}

/// Individual content block in Claude Code format
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClaudeCodeContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[serde(default)]
        #[serde(rename = "signature")]
        _signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: Option<bool>,
    },
    #[serde(other)]
    Unknown,
}

/// Discover all Claude Code project directories under ~/.claude/projects.
fn discover_project_dirs() -> Result<Vec<PathBuf>> {
    let claude_dir = crate::storage::user_home_path(".claude/projects")
        .context("Could not find Claude projects directory")?;

    if !claude_dir.exists() {
        return Ok(Vec::new());
    }

    let mut project_dirs = Vec::new();
    for entry in std::fs::read_dir(&claude_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            project_dirs.push(path);
        }
    }

    project_dirs.sort();
    Ok(project_dirs)
}

/// Discover all Claude Code projects and their sessions-index.json files.
#[cfg(test)]
fn discover_projects() -> Result<Vec<PathBuf>> {
    Ok(discover_project_dirs()?
        .into_iter()
        .map(|dir| dir.join("sessions-index.json"))
        .filter(|path| path.exists())
        .collect())
}

fn parse_rfc3339_string(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

fn claude_text_from_content(content: &ClaudeCodeContent) -> Option<String> {
    match content {
        ClaudeCodeContent::Empty => None,
        ClaudeCodeContent::Text(text) => {
            let text = text.trim();
            if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        }
        ClaudeCodeContent::Blocks(blocks) => {
            let text = blocks
                .iter()
                .filter_map(|block| match block {
                    ClaudeCodeContentBlock::Text { text } => Some(text.trim()),
                    ClaudeCodeContentBlock::Thinking { thinking, .. } => Some(thinking.trim()),
                    ClaudeCodeContentBlock::ToolResult { content, .. } => Some(content.trim()),
                    _ => None,
                })
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() { None } else { Some(text) }
        }
    }
}

fn load_claude_code_entries(path: &Path) -> Result<Vec<ClaudeCodeEntry>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read session file: {}", path.display()))?;

    let mut entries = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<ClaudeCodeEntry>(line) {
            Ok(entry) => entries.push(entry),
            Err(e) => {
                crate::logging::debug(&format!(
                    "Skipping malformed Claude Code entry in {}: {}",
                    path.display(),
                    e
                ));
            }
        }
    }
    Ok(entries)
}

fn ordered_claude_code_message_entries<'a>(
    entries: &'a [ClaudeCodeEntry],
) -> Vec<&'a ClaudeCodeEntry> {
    let message_entries: Vec<&ClaudeCodeEntry> = entries
        .iter()
        .filter(|e| {
            (e.entry_type == "user" || e.entry_type == "assistant")
                && e.message.is_some()
                && !e.is_sidechain
        })
        .collect();

    let mut uuid_to_entry: HashMap<String, &ClaudeCodeEntry> = HashMap::new();
    for entry in &message_entries {
        if let Some(ref uuid) = entry.uuid {
            uuid_to_entry.insert(uuid.clone(), entry);
        }
    }

    let mut ordered_entries: Vec<&ClaudeCodeEntry> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();

    let roots: Vec<&ClaudeCodeEntry> = message_entries
        .iter()
        .filter(|e| {
            e.parent_uuid.is_none()
                || !uuid_to_entry.contains_key(e.parent_uuid.as_deref().unwrap_or_default())
        })
        .copied()
        .collect();

    for root in roots {
        let mut current = root;
        loop {
            if let Some(ref uuid) = current.uuid {
                if visited.contains(uuid) {
                    break;
                }
                visited.insert(uuid.clone());
            }
            ordered_entries.push(current);

            let next = message_entries.iter().find(|e| {
                e.parent_uuid.as_ref() == current.uuid.as_ref()
                    && e.uuid
                        .as_ref()
                        .map(|u| !visited.contains(u))
                        .unwrap_or(true)
            });

            match next {
                Some(n) => current = n,
                None => break,
            }
        }
    }

    for entry in message_entries {
        if entry
            .uuid
            .as_ref()
            .map(|uuid| visited.contains(uuid))
            .unwrap_or(false)
        {
            continue;
        }
        ordered_entries.push(entry);
    }

    ordered_entries
}

fn claude_code_session_info_from_file(
    path: &Path,
    indexed: Option<&SessionIndexEntry>,
) -> Result<ClaudeCodeSessionInfo> {
    let entries = load_claude_code_entries(path)?;
    let ordered_entries = ordered_claude_code_message_entries(&entries);
    let first_entry = ordered_entries.first().copied();
    let last_entry = ordered_entries.last().copied();

    let session_id = indexed
        .map(|entry| entry.session_id.clone())
        .or_else(|| {
            entries
                .iter()
                .find_map(|entry| entry._session_id.clone())
                .or_else(|| {
                    path.file_stem()
                        .and_then(|stem| stem.to_str())
                        .map(|s| s.to_string())
                })
        })
        .unwrap_or_else(|| path.to_string_lossy().to_string());

    let first_prompt = indexed
        .and_then(|entry| entry.first_prompt.clone())
        .filter(|text| !text.trim().is_empty())
        .or_else(|| {
            ordered_entries.iter().find_map(|entry| {
                (entry.entry_type == "user")
                    .then(|| entry.message.as_ref())
                    .flatten()
                    .and_then(|message| claude_text_from_content(&message.content))
            })
        })
        .unwrap_or_else(|| "No prompt".to_string());

    let summary = indexed
        .and_then(|entry| entry.summary.clone())
        .filter(|text| !text.trim().is_empty());
    let message_count = indexed
        .and_then(|entry| entry.message_count)
        .unwrap_or(ordered_entries.len() as u32);
    let created = indexed
        .and_then(|entry| parse_rfc3339_string(entry.created.as_deref()))
        .or_else(|| first_entry.and_then(|entry| parse_rfc3339_string(entry.timestamp.as_deref())));
    let modified = indexed
        .and_then(|entry| parse_rfc3339_string(entry.modified.as_deref()))
        .or_else(|| last_entry.and_then(|entry| parse_rfc3339_string(entry.timestamp.as_deref())));
    let project_path = indexed
        .and_then(|entry| entry.project_path.clone())
        .or_else(|| first_entry.and_then(|entry| entry.cwd.clone()));

    Ok(ClaudeCodeSessionInfo {
        session_id,
        first_prompt,
        summary,
        message_count,
        created,
        modified,
        project_path,
        full_path: path.to_string_lossy().to_string(),
    })
}

/// List all available Claude Code sessions
pub fn list_claude_code_sessions() -> Result<Vec<ClaudeCodeSessionInfo>> {
    let mut all_sessions = Vec::new();
    let mut seen_session_ids = HashSet::new();

    for project_dir in discover_project_dirs()? {
        let index_path = project_dir.join("sessions-index.json");
        if index_path.exists() {
            let content = std::fs::read_to_string(&index_path)
                .with_context(|| format!("Failed to read {}", index_path.display()))?;

            let index: SessionsIndex = serde_json::from_str(&content)
                .with_context(|| format!("Failed to parse {}", index_path.display()))?;

            for entry in index.entries {
                if entry.is_sidechain.unwrap_or(false) {
                    continue;
                }

                let indexed_path = PathBuf::from(&entry.full_path);
                let fallback_path = project_dir.join(format!("{}.jsonl", entry.session_id));
                let path = if indexed_path.exists() {
                    indexed_path
                } else if fallback_path.exists() {
                    fallback_path
                } else {
                    continue;
                };

                let session = claude_code_session_info_from_file(&path, Some(&entry))?;
                seen_session_ids.insert(session.session_id.clone());
                all_sessions.push(session);
            }
        }

        for path in collect_files_recursive(&project_dir, "jsonl") {
            let Some(session_id) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(|stem| stem.to_string())
            else {
                continue;
            };
            if seen_session_ids.contains(&session_id) {
                continue;
            }
            let session = claude_code_session_info_from_file(&path, None)?;
            seen_session_ids.insert(session.session_id.clone());
            all_sessions.push(session);
        }
    }

    // Sort by modified date descending
    all_sessions.sort_by(|a, b| {
        let a_date = a.modified.or(a.created);
        let b_date = b.modified.or(b.created);
        b_date.cmp(&a_date)
    });

    Ok(all_sessions)
}

/// List sessions filtered by project path
pub fn list_sessions_for_project(project_filter: &str) -> Result<Vec<ClaudeCodeSessionInfo>> {
    let sessions = list_claude_code_sessions()?;
    Ok(sessions
        .into_iter()
        .filter(|s| {
            s.project_path
                .as_ref()
                .map(|p| p.contains(project_filter))
                .unwrap_or(false)
        })
        .collect())
}

/// Find a session file by ID
fn find_session_file(session_id: &str) -> Result<PathBuf> {
    let sessions = list_claude_code_sessions()?;

    for session in sessions {
        if session.session_id == session_id {
            let path = PathBuf::from(&session.full_path);
            if path.exists() {
                return Ok(path);
            }
        }
    }

    anyhow::bail!("Session {} not found", session_id);
}

/// Convert Claude Code content blocks to jcode ContentBlocks
fn convert_content_blocks(content: &ClaudeCodeContent) -> Vec<ContentBlock> {
    match content {
        ClaudeCodeContent::Empty => vec![],
        ClaudeCodeContent::Text(text) => {
            if text.is_empty() {
                vec![]
            } else {
                vec![ContentBlock::Text {
                    text: text.clone(),
                    cache_control: None,
                }]
            }
        }
        ClaudeCodeContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ClaudeCodeContentBlock::Text { text } => Some(ContentBlock::Text {
                    text: text.clone(),
                    cache_control: None,
                }),
                ClaudeCodeContentBlock::Thinking { thinking, .. } => {
                    Some(ContentBlock::Reasoning {
                        text: thinking.clone(),
                    })
                }
                ClaudeCodeContentBlock::ToolUse { id, name, input } => {
                    Some(ContentBlock::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    })
                }
                ClaudeCodeContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => Some(ContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: content.clone(),
                    is_error: *is_error,
                }),
                ClaudeCodeContentBlock::Unknown => None,
            })
            .collect(),
    }
}

/// Import a Claude Code session by ID
pub fn import_session(session_id: &str) -> Result<Session> {
    let session_file = find_session_file(session_id)?;
    import_session_from_file(&session_file, session_id)
}

pub fn imported_claude_code_session_id(session_id: &str) -> String {
    format!("imported_cc_{}", session_id)
}

pub fn imported_codex_session_id(session_id: &str) -> String {
    format!("imported_codex_{}", session_id)
}

pub fn imported_opencode_session_id(session_id: &str) -> String {
    format!("imported_opencode_{}", session_id)
}

pub fn imported_pi_session_id(session_path: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_path.as_bytes());
    let digest = hasher.finalize();
    format!("imported_pi_{}", hex::encode(&digest[..8]))
}

pub fn imported_session_id_for_target(
    target: &crate::tui::session_picker::ResumeTarget,
) -> Option<String> {
    match target {
        crate::tui::session_picker::ResumeTarget::JcodeSession { session_id } => {
            Some(session_id.clone())
        }
        crate::tui::session_picker::ResumeTarget::ClaudeCodeSession { session_id } => {
            Some(imported_claude_code_session_id(session_id))
        }
        crate::tui::session_picker::ResumeTarget::CodexSession { session_id } => {
            Some(imported_codex_session_id(session_id))
        }
        crate::tui::session_picker::ResumeTarget::PiSession { session_path } => {
            Some(imported_pi_session_id(session_path))
        }
        crate::tui::session_picker::ResumeTarget::OpenCodeSession { session_id } => {
            Some(imported_opencode_session_id(session_id))
        }
    }
}

pub fn resolve_resume_target_to_jcode(
    target: &crate::tui::session_picker::ResumeTarget,
) -> Result<crate::tui::session_picker::ResumeTarget> {
    use crate::tui::session_picker::ResumeTarget;

    let session_id = match target {
        ResumeTarget::JcodeSession { session_id } => {
            return Ok(ResumeTarget::JcodeSession {
                session_id: session_id.clone(),
            });
        }
        ResumeTarget::ClaudeCodeSession { session_id } => {
            import_session(session_id)?;
            imported_claude_code_session_id(session_id)
        }
        ResumeTarget::CodexSession { session_id } => {
            import_codex_session(session_id)?;
            imported_codex_session_id(session_id)
        }
        ResumeTarget::PiSession { session_path } => {
            import_pi_session(session_path)?;
            imported_pi_session_id(session_path)
        }
        ResumeTarget::OpenCodeSession { session_id } => {
            import_opencode_session(session_id)?;
            imported_opencode_session_id(session_id)
        }
    };

    Ok(ResumeTarget::JcodeSession { session_id })
}

/// Import a Claude Code session from a file path
pub fn import_session_from_file(path: &PathBuf, session_id: &str) -> Result<Session> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read session file: {}", path.display()))?;

    // Parse JSONL entries
    let mut entries: Vec<ClaudeCodeEntry> = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<ClaudeCodeEntry>(line) {
            Ok(entry) => entries.push(entry),
            Err(e) => {
                // Log but skip malformed lines
                crate::logging::debug(&format!("Skipping malformed entry: {}", e));
            }
        }
    }

    // Filter to actual messages (user/assistant types, not progress/snapshots)
    let message_entries: Vec<&ClaudeCodeEntry> = entries
        .iter()
        .filter(|e| {
            (e.entry_type == "user" || e.entry_type == "assistant")
                && e.message.is_some()
                && !e.is_sidechain
        })
        .collect();

    // Build a map of uuid -> entry for ordering
    let mut uuid_to_entry: HashMap<String, &ClaudeCodeEntry> = HashMap::new();
    for entry in &message_entries {
        if let Some(ref uuid) = entry.uuid {
            uuid_to_entry.insert(uuid.clone(), entry);
        }
    }

    // Find root entries (no parent or parent not in our message set)
    // Then build the conversation in order by following parent_uuid links
    let mut ordered_entries: Vec<&ClaudeCodeEntry> = Vec::new();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Find entry with no parent (or parent is not a message entry)
    let roots: Vec<&ClaudeCodeEntry> = message_entries
        .iter()
        .filter(|e| {
            e.parent_uuid.is_none()
                || !uuid_to_entry.contains_key(e.parent_uuid.as_deref().unwrap_or_default())
        })
        .copied()
        .collect();

    // For each root, follow the chain
    for root in roots {
        let mut current = root;
        loop {
            if let Some(ref uuid) = current.uuid {
                if visited.contains(uuid) {
                    break;
                }
                visited.insert(uuid.clone());
            }
            ordered_entries.push(current);

            // Find next entry that has this one as parent
            let next = message_entries.iter().find(|e| {
                e.parent_uuid.as_ref() == current.uuid.as_ref()
                    && e.uuid
                        .as_ref()
                        .map(|u| !visited.contains(u))
                        .unwrap_or(true)
            });

            match next {
                Some(n) => current = n,
                None => break,
            }
        }
    }

    // Extract metadata from entries
    let first_entry = ordered_entries.first();
    let working_dir = first_entry.and_then(|e| e.cwd.clone());
    // Get model from first assistant message (user messages don't have model)
    let model = ordered_entries
        .iter()
        .find(|e| e.entry_type == "assistant")
        .and_then(|e| e.message.as_ref()?.model.clone());
    let created_at = first_entry
        .and_then(|e| e.timestamp.as_ref())
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);

    // Get title from first user message or sessions index
    let title = first_entry
        .and_then(|e| {
            if e.entry_type == "user" {
                match &e.message.as_ref()?.content {
                    ClaudeCodeContent::Text(t) => Some(truncate_title(t)),
                    ClaudeCodeContent::Blocks(blocks) => {
                        for b in blocks {
                            if let ClaudeCodeContentBlock::Text { text } = b {
                                return Some(truncate_title(text));
                            }
                        }
                        None
                    }
                    _ => None,
                }
            } else {
                None
            }
        })
        .or_else(|| {
            // Try to get from index
            list_claude_code_sessions()
                .ok()?
                .into_iter()
                .find(|s| s.session_id == session_id)
                .and_then(|s| s.summary.or(Some(s.first_prompt)))
        });

    // Create jcode session
    let jcode_session_id = imported_claude_code_session_id(session_id);
    let mut session = Session::create_with_id(jcode_session_id, None, title);
    session.provider_session_id = Some(session_id.to_string());
    session.provider_key = Some("claude-code".to_string());
    session.working_dir = working_dir;
    session.model = model;
    session.created_at = created_at;
    session.status = SessionStatus::Closed;

    // Convert messages
    for entry in ordered_entries {
        if let Some(ref msg) = entry.message {
            let role = match msg.role.as_str() {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => continue,
            };

            let content_blocks = convert_content_blocks(&msg.content);

            // Skip empty messages
            if content_blocks.is_empty() {
                continue;
            }

            // Generate message ID from uuid or create new
            let msg_id = entry
                .uuid
                .clone()
                .unwrap_or_else(|| crate::id::new_id("msg"));

            session.append_stored_message(StoredMessage {
                id: msg_id,
                role,
                content: content_blocks,
                display_role: None,
                timestamp: None,
                tool_duration_ms: None,
                token_usage: None,
            });
        }
    }

    // Save the session
    session.save()?;

    Ok(session)
}

fn collect_files_recursive(root: &Path, extension: &str) -> Vec<PathBuf> {
    fn walk(dir: &Path, extension: &str, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, extension, out);
            } else if path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case(extension))
                .unwrap_or(false)
            {
                out.push(path);
            }
        }
    }

    let mut files = Vec::new();
    walk(root, extension, &mut files);
    files.sort();
    files
}

fn parse_rfc3339(value: Option<&serde_json::Value>) -> Option<DateTime<Utc>> {
    value
        .and_then(|v| v.as_str())
        .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

fn extract_text_from_json_value(value: &serde_json::Value) -> String {
    fn visit(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::String(text) => {
                if !text.trim().is_empty() {
                    out.push(text.trim().to_string());
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    visit(item, out);
                }
            }
            serde_json::Value::Object(map) => {
                if let Some(text) = map.get("text").and_then(|v| v.as_str()) {
                    if !text.trim().is_empty() {
                        out.push(text.trim().to_string());
                    }
                    return;
                }
                if let Some(text) = map.get("title").and_then(|v| v.as_str()) {
                    if !text.trim().is_empty() {
                        out.push(text.trim().to_string());
                    }
                }
                for (key, nested) in map {
                    if key == "type" || key == "title" {
                        continue;
                    }
                    visit(nested, out);
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    visit(value, &mut out);
    out.join(" ")
}

fn codex_title_candidate(text: &str) -> Option<String> {
    let cleaned = text.replace("<environment_context>", "");
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return None;
    }
    if cleaned.starts_with("# AGENTS.md instructions")
        || cleaned.starts_with("<permissions instructions>")
        || cleaned.contains("\n<INSTRUCTIONS>")
    {
        return None;
    }
    Some(truncate_title(cleaned))
}

fn append_text_message(
    session: &mut Session,
    role: Role,
    text: String,
    timestamp: Option<DateTime<Utc>>,
) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    session.append_stored_message(StoredMessage {
        id: crate::id::new_id("msg"),
        role,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
            cache_control: None,
        }],
        display_role: None,
        timestamp,
        tool_duration_ms: None,
        token_usage: None,
    });
}

fn finalize_imported_session(
    mut session: Session,
    created_at: DateTime<Utc>,
    updated_at: Option<DateTime<Utc>>,
) -> Result<Session> {
    session.created_at = created_at;
    session.updated_at = updated_at.unwrap_or(created_at);
    session.last_active_at = updated_at.or(Some(created_at));
    session.status = SessionStatus::Closed;
    session.save()?;
    Ok(session)
}

fn find_codex_session_file(session_id: &str) -> Result<PathBuf> {
    let root = crate::storage::user_home_path(".codex/sessions")?;
    for path in collect_files_recursive(&root, "jsonl") {
        let Ok(file) = File::open(&path) else {
            continue;
        };
        let mut lines = BufReader::new(file).lines();
        let Some(Ok(first_line)) = lines.next() else {
            continue;
        };
        let Ok(header) = serde_json::from_str::<serde_json::Value>(&first_line) else {
            continue;
        };
        let meta = if header.get("type").and_then(|v| v.as_str()) == Some("session_meta") {
            header.get("payload").unwrap_or(&header)
        } else {
            &header
        };
        if meta.get("id").and_then(|v| v.as_str()) == Some(session_id) {
            return Ok(path);
        }
    }
    anyhow::bail!("Codex session {} not found", session_id)
}

pub fn import_codex_session(session_id: &str) -> Result<Session> {
    let path = find_codex_session_file(session_id)?;
    let file = File::open(&path)?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();
    let Some(first_line) = lines.next() else {
        anyhow::bail!("Codex session {} is empty", session_id)
    };
    let header: serde_json::Value = serde_json::from_str(&first_line?)?;
    let meta = if header.get("type").and_then(|v| v.as_str()) == Some("session_meta") {
        header.get("payload").unwrap_or(&header)
    } else {
        &header
    };

    let created_at = parse_rfc3339(meta.get("timestamp"))
        .or_else(|| parse_rfc3339(header.get("timestamp")))
        .unwrap_or_else(Utc::now);
    let mut updated_at = Some(created_at);
    let mut title: Option<String> = None;
    let mut working_dir = meta
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let mut model: Option<String> = None;
    let mut session = Session::create_with_id(imported_codex_session_id(session_id), None, None);
    session.provider_session_id = Some(session_id.to_string());
    session.provider_key = Some("openai-codex".to_string());

    for line in lines {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let line_type = value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let (role, content_value, timestamp_value, model_value) = if line_type == "message" {
            let Some(role) = value.get("role").and_then(|v| v.as_str()) else {
                continue;
            };
            (
                role,
                value.get("content").unwrap_or(&serde_json::Value::Null),
                value.get("timestamp"),
                value.get("model"),
            )
        } else if line_type == "response_item" {
            let Some(payload) = value.get("payload") else {
                continue;
            };
            if payload.get("type").and_then(|v| v.as_str()) != Some("message") {
                continue;
            }
            let Some(role) = payload.get("role").and_then(|v| v.as_str()) else {
                continue;
            };
            (
                role,
                payload.get("content").unwrap_or(&serde_json::Value::Null),
                value.get("timestamp").or_else(|| payload.get("timestamp")),
                payload.get("model"),
            )
        } else {
            continue;
        };

        let role = match role {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => continue,
        };
        let text = extract_text_from_json_value(content_value);
        if title.is_none() && role == Role::User {
            title = codex_title_candidate(&text);
        }
        if working_dir.is_none() {
            let cwd_text = extract_text_from_json_value(content_value);
            if let Some(cwd_line) = cwd_text.lines().find(|line| line.contains("<cwd>")) {
                let cwd = cwd_line
                    .replace("<cwd>", "")
                    .replace("</cwd>", "")
                    .trim()
                    .to_string();
                if !cwd.is_empty() {
                    working_dir = Some(cwd);
                }
            }
        }
        if model.is_none() {
            model = model_value.and_then(|v| v.as_str()).map(|s| s.to_string());
        }
        let timestamp = parse_rfc3339(timestamp_value);
        if timestamp.is_some() {
            updated_at = timestamp;
        }
        append_text_message(&mut session, role, text, timestamp);
    }

    session.title = title.or_else(|| Some(format!("Codex session {}", session_id)));
    session.working_dir = working_dir;
    session.model = model;
    finalize_imported_session(session, created_at, updated_at)
}

pub fn import_pi_session(session_path: &str) -> Result<Session> {
    let path = PathBuf::from(session_path);
    let file = File::open(&path)?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();
    let Some(first_line) = lines.next() else {
        anyhow::bail!("Pi session file is empty: {}", path.display())
    };
    let header: serde_json::Value = serde_json::from_str(&first_line?)?;
    if header.get("type").and_then(|v| v.as_str()) != Some("session") {
        anyhow::bail!("Invalid Pi session header in {}", path.display())
    }

    let provider_session_id = header
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let created_at = parse_rfc3339(header.get("timestamp")).unwrap_or_else(Utc::now);
    let mut updated_at = Some(created_at);
    let mut title: Option<String> = None;
    let mut model: Option<String> = None;
    let mut provider_key: Option<String> = Some("pi".to_string());
    let mut session = Session::create_with_id(imported_pi_session_id(session_path), None, None);
    session.provider_session_id = if provider_session_id.is_empty() {
        None
    } else {
        Some(provider_session_id)
    };
    session.working_dir = header
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    for line in lines {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let timestamp = parse_rfc3339(value.get("timestamp"));
        if timestamp.is_some() {
            updated_at = timestamp;
        }
        match value.get("type").and_then(|v| v.as_str()) {
            Some("model_change") => {
                provider_key = value
                    .get("provider")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or(provider_key);
                model = value
                    .get("modelId")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or(model);
            }
            Some("message") => {
                let Some(message) = value.get("message") else {
                    continue;
                };
                let role = match message.get("role").and_then(|v| v.as_str()) {
                    Some("user") => Role::User,
                    Some("assistant") => Role::Assistant,
                    _ => continue,
                };
                let text = extract_text_from_json_value(
                    message.get("content").unwrap_or(&serde_json::Value::Null),
                );
                if title.is_none() && role == Role::User && !text.trim().is_empty() {
                    title = Some(truncate_title(&text));
                }
                if model.is_none() {
                    model = message
                        .get("model")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
                append_text_message(&mut session, role, text, timestamp);
            }
            _ => {}
        }
    }

    session.title = title.or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .map(|stem| format!("Pi session {}", stem))
    });
    session.provider_key = provider_key;
    session.model = model;
    finalize_imported_session(session, created_at, updated_at)
}

fn find_opencode_session_file(session_id: &str) -> Result<PathBuf> {
    let root = crate::storage::user_home_path(".local/share/opencode/storage/session")?;
    for path in collect_files_recursive(&root, "json") {
        let Ok(value) = serde_json::from_reader::<_, serde_json::Value>(File::open(&path)?) else {
            continue;
        };
        if value.get("id").and_then(|v| v.as_str()) == Some(session_id) {
            return Ok(path);
        }
    }
    anyhow::bail!("OpenCode session {} not found", session_id)
}

pub fn import_opencode_session(session_id: &str) -> Result<Session> {
    let session_path = find_opencode_session_file(session_id)?;
    let value: serde_json::Value = serde_json::from_reader(File::open(&session_path)?)?;
    let created_at = value
        .get("time")
        .and_then(|time| time.get("created"))
        .and_then(|v| v.as_i64())
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .unwrap_or_else(Utc::now);
    let mut updated_at = value
        .get("time")
        .and_then(|time| time.get("updated"))
        .and_then(|v| v.as_i64())
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .or(Some(created_at));
    let mut session = Session::create_with_id(imported_opencode_session_id(session_id), None, None);
    session.provider_session_id = Some(session_id.to_string());
    session.provider_key = Some("opencode".to_string());
    session.working_dir = value
        .get("directory")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    session.title = value
        .get("title")
        .and_then(|v| v.as_str())
        .map(truncate_title);

    let messages_root = crate::storage::user_home_path(&format!(
        ".local/share/opencode/storage/message/{}",
        session_id
    ))?;
    let mut messages: Vec<(Option<DateTime<Utc>>, Role, String)> = Vec::new();
    let mut model: Option<String> = None;
    let mut provider_key = session.provider_key.clone();

    if messages_root.exists() {
        for msg_path in collect_files_recursive(&messages_root, "json") {
            let Ok(msg_value) =
                serde_json::from_reader::<_, serde_json::Value>(File::open(&msg_path)?)
            else {
                continue;
            };
            let role = match msg_value.get("role").and_then(|v| v.as_str()) {
                Some("user") => Role::User,
                Some("assistant") => Role::Assistant,
                _ => continue,
            };
            let text = msg_value
                .get("content")
                .map(extract_text_from_json_value)
                .filter(|text| !text.trim().is_empty())
                .or_else(|| msg_value.get("summary").map(extract_text_from_json_value))
                .unwrap_or_default();
            if model.is_none() {
                model = msg_value
                    .get("modelID")
                    .or_else(|| msg_value.get("model").and_then(|m| m.get("modelID")))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
            if provider_key.as_deref() == Some("opencode") {
                provider_key = msg_value
                    .get("providerID")
                    .or_else(|| msg_value.get("model").and_then(|m| m.get("providerID")))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or(provider_key);
            }
            let timestamp = msg_value
                .get("time")
                .and_then(|time| time.get("created"))
                .and_then(|v| v.as_i64())
                .and_then(DateTime::<Utc>::from_timestamp_millis);
            if timestamp.is_some() {
                updated_at = timestamp;
            }
            messages.push((timestamp, role, text));
        }
    }

    messages.sort_by_key(|(timestamp, _, _)| *timestamp);
    for (timestamp, role, text) in messages {
        append_text_message(&mut session, role, text, timestamp);
    }

    if session.title.is_none() {
        session.title = Some(format!("OpenCode session {}", session_id));
    }
    session.provider_key = provider_key;
    session.model = model;
    finalize_imported_session(session, created_at, updated_at)
}

/// Truncate a string to use as a title (first line, max 80 chars)
fn truncate_title(s: &str) -> String {
    let first_line = s.lines().next().unwrap_or(s);
    if first_line.len() > 80 {
        format!("{}...", &first_line[..77])
    } else {
        first_line.to_string()
    }
}

/// Print sessions in a formatted table
pub fn print_sessions_table(sessions: &[ClaudeCodeSessionInfo]) {
    if sessions.is_empty() {
        println!("No Claude Code sessions found.");
        return;
    }

    println!("Claude Code Sessions:\n");
    println!(
        "{:<36}  {:>5}  {:<20}  {}",
        "Session ID", "Msgs", "Modified", "First Prompt"
    );
    println!("{}", "-".repeat(100));

    for session in sessions {
        let modified = session
            .modified
            .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "Unknown".to_string());

        let prompt = if session.first_prompt.len() > 40 {
            format!("{}...", &session.first_prompt[..37])
        } else {
            session.first_prompt.clone()
        };

        println!(
            "{:<36}  {:>5}  {:<20}  {}",
            session.session_id, session.message_count, modified, prompt
        );
    }

    println!("\nTotal: {} sessions", sessions.len());
    println!("\nTo import a session:");
    println!("  jcode import session <session-id>");
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvVarGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set_path(key: &'static str, value: &std::path::Path) -> Self {
            let prev = std::env::var_os(key);
            crate::env::set_var(key, value);
            Self { key, prev }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(prev) = self.prev.take() {
                crate::env::set_var(self.key, prev);
            } else {
                crate::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn test_truncate_title() {
        assert_eq!(truncate_title("short"), "short");
        assert_eq!(truncate_title("line1\nline2"), "line1");

        let long = "a".repeat(100);
        let truncated = truncate_title(&long);
        assert!(truncated.ends_with("..."));
        assert!(truncated.len() <= 80);
    }

    #[test]
    fn test_convert_text_content() {
        let content = ClaudeCodeContent::Text("hello".to_string());
        let blocks = convert_content_blocks(&content);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::Text { text, .. } => assert_eq!(text, "hello"),
            _ => panic!("Expected text block"),
        }
    }

    #[test]
    fn test_convert_empty_content() {
        let content = ClaudeCodeContent::Empty;
        let blocks = convert_content_blocks(&content);
        assert!(blocks.is_empty());
    }

    #[test]
    fn test_convert_blocks_content() {
        let content = ClaudeCodeContent::Blocks(vec![
            ClaudeCodeContentBlock::Text {
                text: "hello".to_string(),
            },
            ClaudeCodeContentBlock::Thinking {
                thinking: "let me think".to_string(),
                _signature: None,
            },
            ClaudeCodeContentBlock::ToolUse {
                id: "tool1".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({"cmd": "ls"}),
            },
        ]);
        let blocks = convert_content_blocks(&content);
        assert_eq!(blocks.len(), 3);

        match &blocks[0] {
            ContentBlock::Text { text, .. } => assert_eq!(text, "hello"),
            _ => panic!("Expected text"),
        }
        match &blocks[1] {
            ContentBlock::Reasoning { text } => assert_eq!(text, "let me think"),
            _ => panic!("Expected reasoning"),
        }
        match &blocks[2] {
            ContentBlock::ToolUse { name, .. } => assert_eq!(name, "bash"),
            _ => panic!("Expected tool use"),
        }
    }

    #[test]
    fn test_discover_projects_uses_sandboxed_external_home() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().unwrap();
        let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());

        let project_dir = temp.path().join("external/.claude/projects/demo");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("sessions-index.json"),
            r#"{"version":1,"entries":[]}"#,
        )
        .unwrap();

        let projects = discover_projects().unwrap();
        assert_eq!(projects, vec![project_dir.join("sessions-index.json")]);
    }

    #[test]
    fn test_list_claude_code_sessions_uses_live_transcripts_when_index_is_stale() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().unwrap();
        let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());

        let project_dir = temp.path().join("external/.claude/projects/demo-project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let indexed_session_path = project_dir.join("live-session-1.jsonl");
        std::fs::write(
            &indexed_session_path,
            concat!(
                "{\"type\":\"user\",\"uuid\":\"u1\",\"sessionId\":\"live-session-1\",\"cwd\":\"/tmp/demo-project\",\"message\":{\"role\":\"user\",\"content\":\"Investigate the login bug\"},\"timestamp\":\"2026-04-04T12:00:00Z\"}\n",
                "{\"type\":\"assistant\",\"uuid\":\"a1\",\"parentUuid\":\"u1\",\"sessionId\":\"live-session-1\",\"cwd\":\"/tmp/demo-project\",\"message\":{\"role\":\"assistant\",\"model\":\"claude-sonnet-4-6\",\"content\":\"I can help with that.\"},\"timestamp\":\"2026-04-04T12:05:00Z\"}\n"
            ),
        )
        .unwrap();

        let orphan_session_path = project_dir.join("orphan-session-2.jsonl");
        std::fs::write(
            &orphan_session_path,
            concat!(
                "{\"type\":\"user\",\"uuid\":\"u2\",\"sessionId\":\"orphan-session-2\",\"cwd\":\"/tmp/demo-project\",\"message\":{\"role\":\"user\",\"content\":\"Summarize the deployment issue\"},\"timestamp\":\"2026-04-05T09:00:00Z\"}\n",
                "{\"type\":\"assistant\",\"uuid\":\"a2\",\"parentUuid\":\"u2\",\"sessionId\":\"orphan-session-2\",\"cwd\":\"/tmp/demo-project\",\"message\":{\"role\":\"assistant\",\"model\":\"claude-sonnet-4-6\",\"content\":\"Here is the deployment summary.\"},\"timestamp\":\"2026-04-05T09:01:00Z\"}\n"
            ),
        )
        .unwrap();

        std::fs::write(
            project_dir.join("sessions-index.json"),
            concat!(
                "{\"version\":1,\"entries\":[",
                "{\"sessionId\":\"live-session-1\",",
                "\"fullPath\":\"/missing/live-session-1.jsonl\",",
                "\"firstPrompt\":\"Investigate the login bug\",",
                "\"summary\":\"Investigate the login bug\",",
                "\"messageCount\":2,",
                "\"created\":\"2026-04-04T12:00:00Z\",",
                "\"modified\":\"2026-04-04T12:05:00Z\",",
                "\"projectPath\":\"/tmp/demo-project\"",
                "}] }"
            ),
        )
        .unwrap();

        let sessions = list_claude_code_sessions().unwrap();

        let indexed = sessions
            .iter()
            .find(|session| session.session_id == "live-session-1")
            .expect("indexed live transcript should be discovered");
        assert_eq!(indexed.full_path, indexed_session_path.to_string_lossy());
        assert_eq!(
            indexed.summary.as_deref(),
            Some("Investigate the login bug")
        );
        assert_eq!(indexed.project_path.as_deref(), Some("/tmp/demo-project"));

        let orphan = sessions
            .iter()
            .find(|session| session.session_id == "orphan-session-2")
            .expect("orphan live transcript should be discovered");
        assert_eq!(orphan.full_path, orphan_session_path.to_string_lossy());
        assert_eq!(orphan.first_prompt, "Summarize the deployment issue");
        assert_eq!(orphan.message_count, 2);
    }

    #[test]
    fn test_import_claude_session_uses_recovered_live_transcript() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().unwrap();
        let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());

        let project_dir = temp.path().join("external/.claude/projects/demo-project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let transcript_path = project_dir.join("live-session-1.jsonl");
        std::fs::write(
            &transcript_path,
            concat!(
                "{\"type\":\"user\",\"uuid\":\"u1\",\"sessionId\":\"live-session-1\",\"cwd\":\"/tmp/demo-project\",\"message\":{\"role\":\"user\",\"content\":\"Investigate the login bug\"},\"timestamp\":\"2026-04-04T12:00:00Z\"}\n",
                "{\"type\":\"assistant\",\"uuid\":\"a1\",\"parentUuid\":\"u1\",\"sessionId\":\"live-session-1\",\"cwd\":\"/tmp/demo-project\",\"message\":{\"role\":\"assistant\",\"model\":\"claude-sonnet-4-6\",\"content\":\"I can help with that.\"},\"timestamp\":\"2026-04-04T12:05:00Z\"}\n"
            ),
        )
        .unwrap();

        std::fs::write(
            project_dir.join("sessions-index.json"),
            concat!(
                "{\"version\":1,\"entries\":[",
                "{\"sessionId\":\"live-session-1\",",
                "\"fullPath\":\"/missing/live-session-1.jsonl\",",
                "\"firstPrompt\":\"Investigate the login bug\",",
                "\"summary\":\"Investigate the login bug\",",
                "\"messageCount\":2,",
                "\"created\":\"2026-04-04T12:00:00Z\",",
                "\"modified\":\"2026-04-04T12:05:00Z\",",
                "\"projectPath\":\"/tmp/demo-project\"",
                "}] }"
            ),
        )
        .unwrap();

        let imported = import_session("live-session-1").unwrap();
        assert_eq!(
            imported.id,
            imported_claude_code_session_id("live-session-1")
        );
        assert_eq!(imported.provider_key.as_deref(), Some("claude-code"));
        assert_eq!(imported.working_dir.as_deref(), Some("/tmp/demo-project"));
        assert_eq!(imported.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(imported.messages.len(), 2);
    }

    #[test]
    fn test_import_pi_session_creates_jcode_snapshot() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().unwrap();
        let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());

        let pi_dir = temp.path().join("external/.pi/agent/sessions/project");
        std::fs::create_dir_all(&pi_dir).unwrap();
        let session_path = pi_dir.join("session.jsonl");
        std::fs::write(
            &session_path,
            concat!(
                "{\"type\":\"session\",\"id\":\"pi-session-1\",\"timestamp\":\"2026-04-05T19:00:00Z\",\"cwd\":\"/tmp/pi-demo\"}\n",
                "{\"type\":\"model_change\",\"timestamp\":\"2026-04-05T19:00:01Z\",\"provider\":\"pi\",\"modelId\":\"pi-model\"}\n",
                "{\"type\":\"message\",\"timestamp\":\"2026-04-05T19:00:02Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hello pi\"}]}}\n",
                "{\"type\":\"message\",\"timestamp\":\"2026-04-05T19:00:03Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"hi back\"}]}}\n"
            ),
        )
        .unwrap();

        let imported = import_pi_session(&session_path.to_string_lossy()).unwrap();
        assert_eq!(
            imported.id,
            imported_pi_session_id(&session_path.to_string_lossy())
        );
        assert_eq!(imported.provider_key.as_deref(), Some("pi"));
        assert_eq!(imported.model.as_deref(), Some("pi-model"));
        assert_eq!(imported.working_dir.as_deref(), Some("/tmp/pi-demo"));
        assert_eq!(imported.messages.len(), 2);
    }

    #[test]
    fn test_import_opencode_session_creates_jcode_snapshot() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().unwrap();
        let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());

        let session_dir = temp
            .path()
            .join("external/.local/share/opencode/storage/session/global");
        let message_dir = temp
            .path()
            .join("external/.local/share/opencode/storage/message/ses_test_opencode");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::create_dir_all(&message_dir).unwrap();

        std::fs::write(
            session_dir.join("ses_test_opencode.json"),
            concat!(
                "{",
                "\"id\":\"ses_test_opencode\",",
                "\"directory\":\"/tmp/opencode-demo\",",
                "\"title\":\"OpenCode imported\",",
                "\"time\":{\"created\":1775415600000,\"updated\":1775415605000}",
                "}"
            ),
        )
        .unwrap();

        std::fs::write(
            message_dir.join("msg-user.json"),
            concat!(
                "{",
                "\"role\":\"user\",",
                "\"time\":{\"created\":1775415601000},",
                "\"summary\":{\"title\":\"Investigate provider routing\"},",
                "\"model\":{\"providerID\":\"opencode\",\"modelID\":\"big-pickle\"}",
                "}"
            ),
        )
        .unwrap();

        std::fs::write(
            message_dir.join("msg-assistant.json"),
            concat!(
                "{",
                "\"role\":\"assistant\",",
                "\"time\":{\"created\":1775415602000},",
                "\"summary\":{\"title\":\"Found the bad provider switch\"},",
                "\"providerID\":\"opencode\",",
                "\"modelID\":\"big-pickle\"",
                "}"
            ),
        )
        .unwrap();

        let imported = import_opencode_session("ses_test_opencode").unwrap();
        assert_eq!(
            imported.id,
            imported_opencode_session_id("ses_test_opencode")
        );
        assert_eq!(imported.provider_key.as_deref(), Some("opencode"));
        assert_eq!(imported.model.as_deref(), Some("big-pickle"));
        assert_eq!(imported.working_dir.as_deref(), Some("/tmp/opencode-demo"));
        assert_eq!(imported.messages.len(), 2);
    }

    #[test]
    fn test_resolve_resume_target_to_jcode_imports_codex_session() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().unwrap();
        let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());

        let codex_dir = temp.path().join("external/.codex/sessions/2026/04/05");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("rollout.jsonl"),
            concat!(
                "{\"timestamp\":\"2026-04-05T19:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-resolve-test\",\"timestamp\":\"2026-04-05T18:59:00Z\",\"cwd\":\"/tmp/codex-resolve\"}}\n",
                "{\"timestamp\":\"2026-04-05T19:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"Fix codex resume\"}]}}\n",
                "{\"timestamp\":\"2026-04-05T19:00:02Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Done\"}]}}\n"
            ),
        )
        .unwrap();

        let resolved = resolve_resume_target_to_jcode(
            &crate::tui::session_picker::ResumeTarget::CodexSession {
                session_id: "codex-resolve-test".to_string(),
            },
        )
        .unwrap();

        assert_eq!(
            resolved,
            crate::tui::session_picker::ResumeTarget::JcodeSession {
                session_id: imported_codex_session_id("codex-resolve-test"),
            }
        );
        let loaded = Session::load(&imported_codex_session_id("codex-resolve-test")).unwrap();
        assert_eq!(loaded.messages.len(), 2);
    }
}
