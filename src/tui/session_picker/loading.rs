use crate::id::{extract_session_name, session_icon};
use crate::message::Role;
use crate::registry::{self, ServerInfo};
use crate::session::{self, CrashedSessionsInfo, Session, SessionStatus};
use crate::storage;
use anyhow::Result;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use super::{
    DEFAULT_SESSION_SCAN_LIMIT, MAX_SESSION_SCAN_LIMIT, MIN_SESSION_SCAN_LIMIT, PreviewMessage,
    SEARCH_CONTENT_BUDGET_BYTES, ServerGroup, SessionInfo,
};

fn session_scan_limit() -> usize {
    std::env::var("JCODE_SESSION_PICKER_MAX_SESSIONS")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .map(|n| n.clamp(MIN_SESSION_SCAN_LIMIT, MAX_SESSION_SCAN_LIMIT))
        .unwrap_or(DEFAULT_SESSION_SCAN_LIMIT)
}

fn push_with_byte_budget(dst: &mut String, src: &str, budget: &mut usize) {
    if *budget == 0 || src.is_empty() {
        return;
    }

    let mut end = src.len().min(*budget);
    while end > 0 && !src.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 {
        return;
    }

    dst.push_str(&src[..end]);
    *budget = budget.saturating_sub(end);
}

#[cfg(test)]
pub(super) fn build_search_index(
    id: &str,
    short_name: &str,
    title: &str,
    working_dir: Option<&str>,
    save_label: Option<&str>,
    messages_preview: &[PreviewMessage],
) -> String {
    let mut combined = String::new();
    combined.push_str(title);
    combined.push(' ');
    combined.push_str(short_name);
    combined.push(' ');
    combined.push_str(id);

    if let Some(dir) = working_dir {
        combined.push(' ');
        combined.push_str(dir);
    }

    if let Some(label) = save_label {
        combined.push(' ');
        combined.push_str(label);
    }

    let mut budget = SEARCH_CONTENT_BUDGET_BYTES;
    for msg in messages_preview {
        let content = msg.content.trim();
        if content.is_empty() {
            continue;
        }
        combined.push(' ');
        push_with_byte_budget(&mut combined, content, &mut budget);
        if budget == 0 {
            break;
        }
    }

    combined.to_lowercase()
}

fn build_search_index_from_summary(
    id: &str,
    short_name: &str,
    title: &str,
    working_dir: Option<&str>,
    save_label: Option<&str>,
    messages: &[SessionMessageSummary],
) -> String {
    let mut combined = String::new();
    combined.push_str(title);
    combined.push(' ');
    combined.push_str(short_name);
    combined.push(' ');
    combined.push_str(id);

    if let Some(dir) = working_dir {
        combined.push(' ');
        combined.push_str(dir);
    }

    if let Some(label) = save_label {
        combined.push(' ');
        combined.push_str(label);
    }

    let mut budget = SEARCH_CONTENT_BUDGET_BYTES;
    for msg in messages {
        let content = msg.content.trim();
        if content.is_empty() {
            continue;
        }
        combined.push(' ');
        push_with_byte_budget(&mut combined, content, &mut budget);
        if budget == 0 {
            break;
        }
    }

    combined.to_lowercase()
}

fn session_sort_key(stem: &str) -> u64 {
    for part in stem.split('_') {
        if part.len() == 13 && part.as_bytes().iter().all(|b| b.is_ascii_digit()) {
            if let Ok(ts) = part.parse::<u64>() {
                return ts;
            }
        }
    }

    stem.split('_')
        .rev()
        .find_map(|part| part.parse::<u64>().ok())
        .unwrap_or(0)
}

fn is_empty_session_file(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return true;
    };
    let mut buf = [0u8; 300];
    let n = match file.take(300).read(&mut buf) {
        Ok(n) => n,
        Err(_) => return true,
    };
    let head = &buf[..n];
    head.windows(13).any(|w| w == b"\"messages\":[]")
}

pub(super) fn collect_recent_session_stems(
    sessions_dir: &Path,
    scan_limit: usize,
) -> Result<Vec<String>> {
    let mut candidates: Vec<(u64, String)> = Vec::new();

    for entry in std::fs::read_dir(sessions_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.extension().map(|e| e == "json").unwrap_or(false) {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        candidates.push((session_sort_key(stem), stem.to_string()));
    }

    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

    let mut recent = Vec::with_capacity(scan_limit);
    for (_, stem) in candidates {
        let path = sessions_dir.join(format!("{stem}.json"));
        if is_empty_session_file(&path) {
            continue;
        }
        recent.push(stem);
        if recent.len() >= scan_limit {
            break;
        }
    }

    Ok(recent)
}

#[derive(Deserialize)]
struct SessionSummary {
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    last_active_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    messages: Vec<SessionMessageSummary>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    short_name: Option<String>,
    #[serde(default)]
    provider_key: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    is_canary: bool,
    #[serde(default)]
    is_debug: bool,
    #[serde(default)]
    saved: bool,
    #[serde(default)]
    save_label: Option<String>,
    #[serde(default)]
    status: SessionStatus,
}

#[derive(Deserialize)]
struct SessionMessageSummary {
    role: Role,
    #[serde(default, deserialize_with = "deserialize_content_text")]
    content: String,
    #[serde(default)]
    token_usage: Option<SessionTokenUsageSummary>,
}

fn deserialize_content_text<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct ContentVisitor;

    impl<'de> de::Visitor<'de> for ContentVisitor {
        type Value = String;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string or array of content blocks")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<String, E> {
            Ok(v.to_string())
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<String, E> {
            Ok(v)
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<String, A::Error> {
            let mut text = String::new();
            while let Some(block) = seq.next_element::<serde_json::Value>()? {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if block_type == "text" || block_type == "" {
                        if !text.is_empty() {
                            text.push(' ');
                        }
                        text.push_str(t);
                    }
                }
            }
            Ok(text)
        }

        fn visit_unit<E: de::Error>(self) -> Result<String, E> {
            Ok(String::new())
        }

        fn visit_none<E: de::Error>(self) -> Result<String, E> {
            Ok(String::new())
        }
    }

    deserializer.deserialize_any(ContentVisitor)
}

#[derive(Deserialize)]
struct SessionTokenUsageSummary {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}

impl SessionTokenUsageSummary {
    fn total_tokens(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_read_input_tokens.unwrap_or(0)
            + self.cache_creation_input_tokens.unwrap_or(0)
    }
}

#[derive(Deserialize)]
struct SessionJournalSummaryMeta {
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    updated_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    short_name: Option<String>,
    #[serde(default)]
    provider_key: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    is_canary: bool,
    #[serde(default)]
    is_debug: bool,
    #[serde(default)]
    saved: bool,
    #[serde(default)]
    save_label: Option<String>,
    #[serde(default)]
    status: SessionStatus,
    #[serde(default)]
    last_active_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Deserialize)]
struct SessionJournalSummaryEntry {
    meta: SessionJournalSummaryMeta,
    #[serde(default)]
    append_messages: Vec<SessionMessageSummary>,
}

fn load_session_summary(path: &Path) -> Result<SessionSummary> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut summary: SessionSummary = serde_json::from_reader(reader)?;

    let journal_path = session::session_journal_path_from_snapshot(path);
    if journal_path.exists() {
        let file = File::open(&journal_path)?;
        let reader = BufReader::new(file);
        for (line_idx, line) in reader.lines().enumerate() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            match serde_json::from_str::<SessionJournalSummaryEntry>(trimmed) {
                Ok(entry) => {
                    summary.parent_id = entry.meta.parent_id;
                    summary.title = entry.meta.title;
                    summary.updated_at = entry.meta.updated_at;
                    summary.last_active_at = entry.meta.last_active_at;
                    summary.working_dir = entry.meta.working_dir;
                    summary.short_name = entry.meta.short_name;
                    summary.provider_key = entry.meta.provider_key;
                    summary.model = entry.meta.model;
                    summary.is_canary = entry.meta.is_canary;
                    summary.is_debug = entry.meta.is_debug;
                    summary.saved = entry.meta.saved;
                    summary.save_label = entry.meta.save_label;
                    summary.status = entry.meta.status;
                    summary.messages.extend(entry.append_messages);
                }
                Err(err) => {
                    crate::logging::warn(&format!(
                        "Session picker journal parse failed at {} line {}: {}",
                        journal_path.display(),
                        line_idx + 1,
                        err
                    ));
                    break;
                }
            }
        }
    }

    Ok(summary)
}

pub(super) fn build_messages_preview(session: &Session) -> Vec<PreviewMessage> {
    session::render_messages(session)
        .into_iter()
        .rev()
        .take(20)
        .rev()
        .map(|msg| PreviewMessage {
            role: msg.role,
            content: msg.content,
            tool_calls: msg.tool_calls,
            tool_data: msg.tool_data,
            timestamp: None,
        })
        .collect()
}

pub(super) fn crashed_sessions_from_all_sessions(
    sessions: &[SessionInfo],
) -> Option<CrashedSessionsInfo> {
    let recovered_parents: HashSet<&str> = sessions
        .iter()
        .filter(|s| s.id.starts_with("session_recovery_"))
        .filter_map(|s| s.parent_id.as_deref())
        .collect();

    let mut crashed: Vec<&SessionInfo> = sessions
        .iter()
        .filter(|s| matches!(s.status, SessionStatus::Crashed { .. }))
        .filter(|s| !recovered_parents.contains(s.id.as_str()))
        .collect();
    if crashed.is_empty() {
        return None;
    }

    let crash_timestamp =
        |session: &SessionInfo| session.last_active_at.unwrap_or(session.last_message_time);
    let most_recent = crashed
        .iter()
        .map(|session| crash_timestamp(session))
        .max()?;
    let crash_window = chrono::Duration::seconds(60);
    crashed.retain(|s| {
        let delta = most_recent.signed_duration_since(crash_timestamp(s));
        delta >= chrono::Duration::zero() && delta <= crash_window
    });
    if crashed.is_empty() {
        return None;
    }

    crashed.sort_by(|a, b| b.last_message_time.cmp(&a.last_message_time));

    Some(CrashedSessionsInfo {
        session_ids: crashed.iter().map(|s| s.id.clone()).collect(),
        display_names: crashed.iter().map(|s| s.short_name.clone()).collect(),
        most_recent_crash: most_recent,
    })
}

pub fn load_sessions() -> Result<Vec<SessionInfo>> {
    let sessions_dir = storage::jcode_dir()?.join("sessions");

    if !sessions_dir.exists() {
        return Ok(Vec::new());
    }

    let scan_limit = session_scan_limit();
    let candidates = collect_recent_session_stems(&sessions_dir, scan_limit)?;
    let mut sessions: Vec<SessionInfo> = Vec::new();

    for stem in candidates {
        let path = sessions_dir.join(format!("{stem}.json"));
        if let Ok(session) = load_session_summary(&path) {
            let short_name = session
                .short_name
                .clone()
                .or_else(|| extract_session_name(&stem).map(|s| s.to_string()))
                .unwrap_or_else(|| stem.clone());
            let icon = session_icon(&short_name);

            let mut user_message_count = 0;
            let mut assistant_message_count = 0;
            let mut estimated_tokens: usize = 0;

            for msg in &session.messages {
                match msg.role {
                    Role::User => user_message_count += 1,
                    Role::Assistant => assistant_message_count += 1,
                }
                if let Some(usage) = &msg.token_usage {
                    estimated_tokens =
                        estimated_tokens.saturating_add(usage.total_tokens() as usize);
                }
            }

            let status = session.status.clone();
            let needs_catchup = crate::catchup::needs_catchup(&stem, session.updated_at, &status);

            if session.messages.is_empty() {
                continue;
            }

            let title = session.title.unwrap_or_else(|| "Untitled".to_string());
            let messages_preview: Vec<PreviewMessage> = Vec::new();
            let search_index = build_search_index_from_summary(
                &stem,
                &short_name,
                &title,
                session.working_dir.as_deref(),
                session.save_label.as_deref(),
                &session.messages,
            );

            sessions.push(SessionInfo {
                id: stem.to_string(),
                parent_id: session.parent_id,
                short_name,
                icon: icon.to_string(),
                title,
                message_count: session.messages.len(),
                user_message_count,
                assistant_message_count,
                created_at: session.created_at,
                last_message_time: session.updated_at,
                last_active_at: session.last_active_at,
                working_dir: session.working_dir,
                model: session.model,
                provider_key: session.provider_key,
                is_canary: session.is_canary,
                is_debug: session.is_debug,
                saved: session.saved,
                save_label: session.save_label,
                status,
                needs_catchup,
                estimated_tokens,
                messages_preview,
                search_index,
                server_name: None,
                server_icon: None,
            });
        }
    }

    sessions.sort_by(|a, b| b.last_message_time.cmp(&a.last_message_time));

    Ok(sessions)
}

pub fn load_servers() -> Vec<ServerInfo> {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| {
            handle.block_on(async { registry::list_servers().await.unwrap_or_default() })
        })
    } else {
        tokio::runtime::Runtime::new()
            .map(|rt| rt.block_on(async { registry::list_servers().await.unwrap_or_default() }))
            .unwrap_or_default()
    }
}

pub fn load_sessions_grouped() -> Result<(Vec<ServerGroup>, Vec<SessionInfo>)> {
    let all_sessions = load_sessions()?;
    let servers = load_servers();

    let mut session_to_server: HashMap<String, &ServerInfo> = HashMap::new();
    for server in &servers {
        for session_name in &server.sessions {
            session_to_server.insert(session_name.clone(), server);
        }
    }

    let mut server_sessions: HashMap<String, Vec<SessionInfo>> = HashMap::new();
    let mut orphan_sessions: Vec<SessionInfo> = Vec::new();

    for mut session in all_sessions {
        if let Some(server) = session_to_server.get(&session.short_name) {
            session.server_name = Some(server.name.clone());
            session.server_icon = Some(server.icon.clone());
            server_sessions
                .entry(server.name.clone())
                .or_default()
                .push(session);
        } else {
            orphan_sessions.push(session);
        }
    }

    let mut groups: Vec<ServerGroup> = servers
        .iter()
        .map(|server| {
            let sessions = server_sessions.remove(&server.name).unwrap_or_default();
            ServerGroup {
                name: server.name.clone(),
                icon: server.icon.clone(),
                version: server.version.clone(),
                git_hash: server.git_hash.clone(),
                is_running: true,
                sessions,
            }
        })
        .collect();

    groups.sort_by(|a, b| {
        let a_latest = a.sessions.iter().map(|s| s.last_message_time).max();
        let b_latest = b.sessions.iter().map(|s| s.last_message_time).max();
        b_latest.cmp(&a_latest)
    });

    Ok((groups, orphan_sessions))
}
