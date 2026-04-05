use super::server_has_newer_binary;
use crate::agent::Agent;
use crate::protocol::{ServerEvent, encode_event};
use crate::provider::Provider;
use crate::transport::WriteHalf;
use anyhow::Result;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HistoryPayloadMode {
    Full,
    MetadataOnly,
}
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, RwLock};

pub(super) async fn handle_get_state(
    id: u64,
    client_session_id: &str,
    client_is_processing: bool,
    sessions: &Arc<RwLock<HashMap<String, Arc<Mutex<Agent>>>>>,
    writer: &Arc<Mutex<WriteHalf>>,
) -> Result<()> {
    let session_count = {
        let sessions_guard = sessions.read().await;
        sessions_guard.len()
    };

    write_event(
        writer,
        &ServerEvent::State {
            id,
            session_id: client_session_id.to_string(),
            message_count: session_count,
            is_processing: client_is_processing,
        },
    )
    .await
}

pub(super) async fn handle_get_history(
    id: u64,
    client_session_id: &str,
    agent: &Arc<Mutex<Agent>>,
    provider: &Arc<dyn Provider>,
    sessions: &Arc<RwLock<HashMap<String, Arc<Mutex<Agent>>>>>,
    client_count: &Arc<RwLock<usize>>,
    writer: &Arc<Mutex<WriteHalf>>,
    server_name: &str,
    server_icon: &str,
) -> Result<()> {
    let history_start = Instant::now();
    send_history(
        id,
        client_session_id,
        agent,
        sessions,
        client_count,
        writer,
        server_name,
        server_icon,
        None,
        HistoryPayloadMode::Full,
    )
    .await?;
    let send_history_ms = history_start.elapsed().as_millis();

    let prefetch_start = Instant::now();
    spawn_model_prefetch_update(Arc::clone(provider), Arc::clone(agent), Arc::clone(writer));
    crate::logging::info(&format!(
        "[TIMING] handle_get_history: session={}, send_history={}ms, prefetch_spawn={}ms, total={}ms",
        client_session_id,
        send_history_ms,
        prefetch_start.elapsed().as_millis(),
        history_start.elapsed().as_millis(),
    ));
    Ok(())
}

pub(super) async fn send_history(
    id: u64,
    session_id: &str,
    agent: &Arc<Mutex<Agent>>,
    sessions: &Arc<RwLock<HashMap<String, Arc<Mutex<Agent>>>>>,
    client_count: &Arc<RwLock<usize>>,
    writer: &Arc<Mutex<WriteHalf>>,
    server_name: &str,
    server_icon: &str,
    was_interrupted: Option<bool>,
    payload_mode: HistoryPayloadMode,
) -> Result<()> {
    let history_start = Instant::now();
    let agent_lock_start = Instant::now();
    let (
        messages,
        images,
        is_canary,
        provider_name,
        provider_model,
        subagent_model,
        autoreview_enabled,
        autojudge_enabled,
        available_models,
        available_model_routes,
        skills,
        tool_names,
        upstream_provider,
        connection_type,
        status_detail,
        reasoning_effort,
        service_tier,
        compaction_mode,
        agent_lock_ms,
        history_snapshot_ms,
        image_render_ms,
        tool_names_ms,
        available_models_ms,
        model_routes_ms,
        skills_ms,
        provider_meta_ms,
        compaction_mode_ms,
    ) = {
        let agent_guard = agent.lock().await;
        let agent_lock_ms = agent_lock_start.elapsed().as_millis();
        let provider = agent_guard.provider_handle();

        let (messages, history_snapshot_ms) = match payload_mode {
            HistoryPayloadMode::Full => {
                let history_snapshot_start = Instant::now();
                let messages = agent_guard.get_history();
                (messages, history_snapshot_start.elapsed().as_millis())
            }
            HistoryPayloadMode::MetadataOnly => (Vec::new(), 0),
        };

        let (images, image_render_ms) = match payload_mode {
            HistoryPayloadMode::Full => {
                let image_render_start = Instant::now();
                let images = agent_guard.get_rendered_images();
                (images, image_render_start.elapsed().as_millis())
            }
            HistoryPayloadMode::MetadataOnly => (Vec::new(), 0),
        };

        let tool_names_start = Instant::now();
        let tool_names = agent_guard.tool_names().await;
        let tool_names_ms = tool_names_start.elapsed().as_millis();

        let available_models_start = Instant::now();
        let available_models = agent_guard.available_models_display();
        let available_models_ms = available_models_start.elapsed().as_millis();

        // Model-route expansion can be relatively expensive (provider/account routing,
        // endpoint cache reads, etc.). The TUI already supports later
        // AvailableModelsUpdated events, so keep the initial History payload fast and
        // let the background refresh populate detailed routes asynchronously.
        let available_model_routes = Vec::new();
        let model_routes_ms = 0;

        let skills_start = Instant::now();
        let skills = agent_guard.available_skill_names();
        let skills_ms = skills_start.elapsed().as_millis();

        let provider_meta_start = Instant::now();
        let reasoning_effort = provider.reasoning_effort();
        let service_tier = provider.service_tier();
        let provider_meta_ms = provider_meta_start.elapsed().as_millis();

        let compaction_mode_start = Instant::now();
        let compaction_mode = agent_guard.compaction_mode().await;
        let compaction_mode_ms = compaction_mode_start.elapsed().as_millis();

        (
            messages,
            images,
            agent_guard.is_canary(),
            agent_guard.provider_name(),
            agent_guard.provider_model(),
            agent_guard.subagent_model(),
            agent_guard.autoreview_enabled(),
            agent_guard.autojudge_enabled(),
            available_models,
            available_model_routes,
            skills,
            tool_names,
            agent_guard.last_upstream_provider(),
            agent_guard.last_connection_type(),
            agent_guard.last_status_detail(),
            reasoning_effort,
            service_tier,
            compaction_mode,
            agent_lock_ms,
            history_snapshot_ms,
            image_render_ms,
            tool_names_ms,
            available_models_ms,
            model_routes_ms,
            skills_ms,
            provider_meta_ms,
            compaction_mode_ms,
        )
    };

    let side_panel_start = Instant::now();
    let side_panel = crate::side_panel::snapshot_for_session(session_id).unwrap_or_default();
    let side_panel_ms = side_panel_start.elapsed().as_millis();

    let mut mcp_map: BTreeMap<String, usize> = BTreeMap::new();
    for name in &tool_names {
        if let Some(rest) = name.strip_prefix("mcp__") {
            if let Some((server, _tool)) = rest.split_once("__") {
                *mcp_map.entry(server.to_string()).or_default() += 1;
            }
        }
    }
    let mcp_servers: Vec<String> = mcp_map
        .into_iter()
        .map(|(name, count)| format!("{name}:{count}"))
        .collect();

    let (all_sessions, current_client_count) = {
        let sessions_snapshot_start = Instant::now();
        let sessions_guard = sessions.read().await;
        let all: Vec<String> = sessions_guard.keys().cloned().collect();
        let count = *client_count.read().await;
        let sessions_snapshot_ms = sessions_snapshot_start.elapsed().as_millis();
        crate::logging::info(&format!(
            "[TIMING] send_history prep: session={}, mode={:?}, messages={}, images={}, mcp_servers={}, agent_lock={}ms, history={}ms, images={}ms, tool_names={}ms, models={}ms, routes={}ms, skills={}ms, provider_meta={}ms, compaction={}ms, side_panel={}ms, sessions={}ms, total={}ms",
            session_id,
            payload_mode,
            messages.len(),
            images.len(),
            mcp_servers.len(),
            agent_lock_ms,
            history_snapshot_ms,
            image_render_ms,
            tool_names_ms,
            available_models_ms,
            model_routes_ms,
            skills_ms,
            provider_meta_ms,
            compaction_mode_ms,
            side_panel_ms,
            sessions_snapshot_ms,
            history_start.elapsed().as_millis(),
        ));
        (all, count)
    };

    let history_event = ServerEvent::History {
        id,
        session_id: session_id.to_string(),
        messages,
        images,
        provider_name: Some(provider_name),
        provider_model: Some(provider_model),
        subagent_model,
        autoreview_enabled,
        autojudge_enabled,
        available_models,
        available_model_routes,
        mcp_servers,
        skills,
        total_tokens: None,
        all_sessions,
        client_count: Some(current_client_count),
        is_canary: Some(is_canary),
        server_version: Some(env!("JCODE_VERSION").to_string()),
        server_name: Some(server_name.to_string()),
        server_icon: Some(server_icon.to_string()),
        server_has_update: Some(server_has_newer_binary()),
        was_interrupted,
        connection_type,
        status_detail,
        upstream_provider,
        reasoning_effort,
        service_tier,
        compaction_mode,
        side_panel,
    };
    let encode_start = Instant::now();
    let json = encode_event(&history_event);
    let encode_ms = encode_start.elapsed().as_millis();
    let writer_lock_start = Instant::now();
    let mut writer_guard = writer.lock().await;
    let writer_lock_ms = writer_lock_start.elapsed().as_millis();
    let write_start = Instant::now();
    let result = writer_guard.write_all(json.as_bytes()).await;
    let write_ms = write_start.elapsed().as_millis();

    crate::logging::info(&format!(
        "[TIMING] send_history write: session={}, bytes={}, encode={}ms, writer_lock={}ms, write={}ms, total={}ms",
        session_id,
        json.len(),
        encode_ms,
        writer_lock_ms,
        write_ms,
        history_start.elapsed().as_millis(),
    ));

    result.map_err(Into::into)
}

async fn write_event(writer: &Arc<Mutex<WriteHalf>>, event: &ServerEvent) -> Result<()> {
    let json = encode_event(event);
    let mut writer = writer.lock().await;
    writer.write_all(json.as_bytes()).await?;
    Ok(())
}

pub(super) fn spawn_model_prefetch_update(
    provider: Arc<dyn Provider>,
    agent: Arc<Mutex<Agent>>,
    writer: Arc<Mutex<WriteHalf>>,
) {
    tokio::spawn(async move {
        let initial = {
            let agent_guard = agent.lock().await;
            (
                agent_guard.available_models_display(),
                agent_guard.model_routes(),
            )
        };

        let _ = write_event(
            &writer,
            &ServerEvent::AvailableModelsUpdated {
                available_models: initial.0.clone(),
                available_model_routes: initial.1.clone(),
            },
        )
        .await;

        if provider.prefetch_models().await.is_err() {
            return;
        }

        let refreshed = {
            let agent_guard = agent.lock().await;
            (
                agent_guard.available_models_display(),
                agent_guard.model_routes(),
            )
        };

        if refreshed == initial {
            return;
        }

        let _ = write_event(
            &writer,
            &ServerEvent::AvailableModelsUpdated {
                available_models: refreshed.0,
                available_model_routes: refreshed.1,
            },
        )
        .await;
    });
}
