use super::{App, DisplayMessage, ProcessingStatus};
use crate::message::{ContentBlock, Message, Role};
use crate::session::Session;
use std::time::Instant;

pub(super) fn reset_current_session(app: &mut App) {
    app.clear_provider_messages();
    app.clear_display_messages();
    app.queued_messages.clear();
    app.pasted_contents.clear();
    app.pending_images.clear();
    app.active_skill = None;
    let mut session = Session::create(None, None);
    session.model = Some(app.provider.model());
    app.session = session;
    app.provider_session_id = None;
}

pub(super) fn handle_help_command(app: &mut App, trimmed: &str) -> bool {
    if let Some(topic) = trimmed
        .strip_prefix("/help ")
        .or_else(|| trimmed.strip_prefix("/? "))
    {
        if let Some(help) = app.command_help(topic) {
            app.push_display_message(DisplayMessage::system(help));
        } else {
            app.push_display_message(DisplayMessage::error(format!(
                "Unknown command '{}'. Use `/help` to list commands.",
                topic.trim()
            )));
        }
        return true;
    }

    if trimmed == "/help" || trimmed == "/?" || trimmed == "/commands" {
        app.help_scroll = Some(0);
        return true;
    }

    false
}

pub(super) fn handle_session_command(app: &mut App, trimmed: &str) -> bool {
    if trimmed == "/clear" {
        reset_current_session(app);
        return true;
    }

    if trimmed == "/save" || trimmed.starts_with("/save ") {
        let label = trimmed.strip_prefix("/save").unwrap().trim();
        let label = if label.is_empty() {
            None
        } else {
            Some(label.to_string())
        };
        app.session.mark_saved(label.clone());
        if let Err(e) = app.session.save() {
            app.push_display_message(DisplayMessage::error(format!(
                "Failed to save session: {}",
                e
            )));
            return true;
        }
        let name = app.session.display_name().to_string();
        let msg = if let Some(ref lbl) = app.session.save_label {
            format!(
                "📌 Session **{}** saved as \"**{}**\". It will appear at the top of `/resume`.",
                name, lbl,
            )
        } else {
            format!(
                "📌 Session **{}** saved. It will appear at the top of `/resume`.",
                name,
            )
        };
        app.push_display_message(DisplayMessage::system(msg));
        app.set_status_notice("Session saved");
        return true;
    }

    if trimmed == "/unsave" {
        app.session.unmark_saved();
        if let Err(e) = app.session.save() {
            app.push_display_message(DisplayMessage::error(format!(
                "Failed to save session: {}",
                e
            )));
            return true;
        }
        let name = app.session.display_name().to_string();
        app.push_display_message(DisplayMessage::system(format!(
            "Removed bookmark from session **{}**.",
            name,
        )));
        app.set_status_notice("Bookmark removed");
        return true;
    }

    if trimmed == "/memory status" {
        let default_enabled = crate::config::config().features.memory;
        app.push_display_message(DisplayMessage::system(format!(
            "Memory feature: **{}** (config default: {})",
            if app.memory_enabled {
                "enabled"
            } else {
                "disabled"
            },
            if default_enabled {
                "enabled"
            } else {
                "disabled"
            }
        )));
        return true;
    }

    if trimmed == "/memory" {
        let new_state = !app.memory_enabled;
        app.set_memory_feature_enabled(new_state);
        let label = if new_state { "ON" } else { "OFF" };
        app.set_status_notice(&format!("Memory: {}", label));
        app.push_display_message(DisplayMessage::system(format!(
            "Memory feature {} for this session.",
            if new_state { "enabled" } else { "disabled" }
        )));
        return true;
    }

    if trimmed == "/memory on" {
        app.set_memory_feature_enabled(true);
        app.set_status_notice("Memory: ON");
        app.push_display_message(DisplayMessage::system(
            "Memory feature enabled for this session.".to_string(),
        ));
        return true;
    }

    if trimmed == "/memory off" {
        app.set_memory_feature_enabled(false);
        app.set_status_notice("Memory: OFF");
        app.push_display_message(DisplayMessage::system(
            "Memory feature disabled for this session.".to_string(),
        ));
        return true;
    }

    if trimmed.starts_with("/memory ") {
        app.push_display_message(DisplayMessage::error(
            "Usage: `/memory [on|off|status]`".to_string(),
        ));
        return true;
    }

    if trimmed == "/swarm" || trimmed == "/swarm status" {
        let default_enabled = crate::config::config().features.swarm;
        app.push_display_message(DisplayMessage::system(format!(
            "Swarm feature: **{}** (config default: {})",
            if app.swarm_enabled {
                "enabled"
            } else {
                "disabled"
            },
            if default_enabled {
                "enabled"
            } else {
                "disabled"
            }
        )));
        return true;
    }

    if trimmed == "/swarm on" {
        app.set_swarm_feature_enabled(true);
        app.set_status_notice("Swarm: ON");
        app.push_display_message(DisplayMessage::system(
            "Swarm feature enabled for this session.".to_string(),
        ));
        return true;
    }

    if trimmed == "/swarm off" {
        app.set_swarm_feature_enabled(false);
        app.set_status_notice("Swarm: OFF");
        app.push_display_message(DisplayMessage::system(
            "Swarm feature disabled for this session.".to_string(),
        ));
        return true;
    }

    if trimmed.starts_with("/swarm ") {
        app.push_display_message(DisplayMessage::error(
            "Usage: `/swarm [on|off|status]`".to_string(),
        ));
        return true;
    }

    if trimmed == "/rewind" {
        if app.session.messages.is_empty() {
            app.push_display_message(DisplayMessage::system(
                "No messages in conversation.".to_string(),
            ));
            return true;
        }

        let mut history = String::from("**Conversation history:**\n\n");
        for (i, msg) in app.session.messages.iter().enumerate() {
            let role_str = match msg.role {
                Role::User => "👤 User",
                Role::Assistant => "🤖 Assistant",
            };
            let content = msg.content_preview();
            let preview = crate::util::truncate_str(&content, 80);
            history.push_str(&format!("  `{}` {} - {}\n", i + 1, role_str, preview));
        }
        history.push_str("\nUse `/rewind N` to rewind to message N (removes all messages after).");

        app.push_display_message(DisplayMessage::system(history));
        return true;
    }

    if let Some(num_str) = trimmed.strip_prefix("/rewind ") {
        let num_str = num_str.trim();
        match num_str.parse::<usize>() {
            Ok(n) if n > 0 && n <= app.session.messages.len() => {
                let removed = app.session.messages.len() - n;
                app.session.messages.truncate(n);
                app.replace_provider_messages(app.session.messages_for_provider());
                app.session.updated_at = chrono::Utc::now();

                app.clear_display_messages();
                for rendered in crate::session::render_messages(&app.session) {
                    app.push_display_message(DisplayMessage {
                        role: rendered.role,
                        content: rendered.content,
                        tool_calls: rendered.tool_calls,
                        duration_secs: None,
                        title: None,
                        tool_data: rendered.tool_data,
                    });
                }

                app.provider_session_id = None;
                app.session.provider_session_id = None;
                let _ = app.session.save();

                app.push_display_message(DisplayMessage::system(format!(
                    "✓ Rewound to message {}. Removed {} message{}.",
                    n,
                    removed,
                    if removed == 1 { "" } else { "s" }
                )));
            }
            Ok(n) => {
                app.push_display_message(DisplayMessage::error(format!(
                    "Invalid message number: {}. Valid range: 1-{}",
                    n,
                    app.session.messages.len()
                )));
            }
            Err(_) => {
                app.push_display_message(DisplayMessage::error(format!(
                    "Usage: `/rewind N` where N is a message number (1-{})",
                    app.session.messages.len()
                )));
            }
        }
        return true;
    }

    if trimmed == "/poke" {
        if app.is_processing {
            app.push_display_message(DisplayMessage::system(
                "Model is currently running. Wait for it to finish before poking.".to_string(),
            ));
            return true;
        }

        let session_id = app.session.id.clone();
        let todos = crate::todo::load_todos(&session_id).unwrap_or_default();
        let incomplete: Vec<_> = todos
            .iter()
            .filter(|t| t.status != "completed" && t.status != "cancelled")
            .collect();

        if incomplete.is_empty() {
            app.push_display_message(DisplayMessage::system(
                "No incomplete todos found. Nothing to poke about.".to_string(),
            ));
            return true;
        }

        let mut todo_list = String::new();
        for t in &incomplete {
            let status_icon = match t.status.as_str() {
                "in_progress" => "🔄",
                _ => "⬜",
            };
            todo_list.push_str(&format!(
                "  {} [{}] {}\n",
                status_icon, t.priority, t.content
            ));
        }

        let poke_msg = format!(
            "Your todo list has {} incomplete item{}:\n\n{}\n\
            Please continue your work. Either:\n\
            1. Keep working and complete the remaining tasks\n\
            2. Update the todo list with `todo_write` if items are already done or no longer needed\n\
            3. If you genuinely need user input to proceed, say so clearly and specifically — \
            but only if truly blocked (this should be rare; prefer making reasonable assumptions)",
            incomplete.len(),
            if incomplete.len() == 1 { "" } else { "s" },
            todo_list,
        );

        app.push_display_message(DisplayMessage::system(format!(
            "👉 Poking model with {} incomplete todo{}...",
            incomplete.len(),
            if incomplete.len() == 1 { "" } else { "s" },
        )));

        app.add_provider_message(Message::user(&poke_msg));
        app.session.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: poke_msg,
                cache_control: None,
            }],
        );
        let _ = app.session.save();

        app.is_processing = true;
        app.status = ProcessingStatus::Sending;
        app.clear_streaming_render_state();
        app.stream_buffer.clear();
        app.thought_line_inserted = false;
        app.thinking_prefix_emitted = false;
        app.thinking_buffer.clear();
        app.streaming_tool_calls.clear();
        app.streaming_input_tokens = 0;
        app.streaming_output_tokens = 0;
        app.streaming_cache_read_tokens = None;
        app.streaming_cache_creation_tokens = None;
        app.upstream_provider = None;
        app.streaming_tps_start = None;
        app.streaming_tps_elapsed = std::time::Duration::ZERO;
        app.streaming_total_output_tokens = 0;
        app.processing_started = Some(Instant::now());
        app.pending_turn = true;

        return true;
    }

    false
}

pub(super) fn handle_config_command(app: &mut App, trimmed: &str) -> bool {
    use crate::bus::{Bus, BusEvent};

    if trimmed == "/compact" {
        if !app.provider.supports_compaction() {
            app.push_display_message(DisplayMessage::system(
                "Manual compaction is not available for this provider.".to_string(),
            ));
            return true;
        }
        let compaction = app.registry.compaction();
        match compaction.try_write() {
            Ok(mut manager) => {
                let stats = manager.stats_with(&app.messages);
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

                match manager.force_compact_with(&app.messages, app.provider.clone()) {
                    Ok(()) => {
                        app.push_display_message(DisplayMessage {
                            role: "system".to_string(),
                            content: format!(
                                "{}\n\n✓ **Compaction started** - summarizing older messages in background.\n\
                                The summary will be applied automatically when ready.\n\
                                Use `/help compact` for details.",
                                status_msg
                            ),
                            tool_calls: vec![],
                            duration_secs: None,
                            title: None,
                            tool_data: None,
                        });
                    }
                    Err(reason) => {
                        app.push_display_message(DisplayMessage {
                            role: "system".to_string(),
                            content: format!(
                                "{}\n\n⚠ **Cannot compact:** {}\n\
                                Try `/fix` for emergency recovery.",
                                status_msg, reason
                            ),
                            tool_calls: vec![],
                            duration_secs: None,
                            title: None,
                            tool_data: None,
                        });
                    }
                }
            }
            Err(_) => {
                app.push_display_message(DisplayMessage {
                    role: "system".to_string(),
                    content: "⚠ Cannot access compaction manager (lock held)".to_string(),
                    tool_calls: vec![],
                    duration_secs: None,
                    title: None,
                    tool_data: None,
                });
            }
        }
        return true;
    }

    if trimmed == "/fix" {
        app.run_fix_command();
        return true;
    }

    if trimmed == "/usage" {
        app.push_display_message(DisplayMessage::system(
            "Fetching usage limits from all providers...".to_string(),
        ));
        tokio::spawn(async move {
            let results = crate::usage::fetch_all_provider_usage().await;
            Bus::global().publish(BusEvent::UsageReport(results));
        });
        return true;
    }

    if trimmed == "/remember" {
        if !app.memory_enabled {
            app.push_display_message(DisplayMessage::system(
                "Memory feature is disabled. Use `/memory on` to enable it.".to_string(),
            ));
            return true;
        }

        use crate::tui::info_widget::{MemoryEventKind, MemoryState};

        let context = crate::memory::format_context_for_relevance(&app.messages);
        if context.len() < 100 {
            app.push_display_message(DisplayMessage {
                role: "system".to_string(),
                content: "Not enough conversation to extract memories from.".to_string(),
                tool_calls: vec![],
                duration_secs: None,
                title: None,
                tool_data: None,
            });
            return true;
        }

        app.push_display_message(DisplayMessage {
            role: "system".to_string(),
            content: "🧠 Extracting memories from conversation...".to_string(),
            tool_calls: vec![],
            duration_secs: None,
            title: None,
            tool_data: None,
        });

        crate::memory::set_state(MemoryState::Extracting {
            reason: "manual".to_string(),
        });
        crate::memory::add_event(MemoryEventKind::ExtractionStarted {
            reason: "/remember command".to_string(),
        });

        let context_owned = context.clone();
        tokio::spawn(async move {
            let sidecar = crate::sidecar::Sidecar::new();
            match sidecar.extract_memories(&context_owned).await {
                Ok(extracted) if !extracted.is_empty() => {
                    let manager = crate::memory::MemoryManager::new();
                    let mut stored_count = 0;

                    for mem in extracted {
                        let category = crate::memory::MemoryCategory::from_extracted(&mem.category);

                        let trust = match mem.trust.as_str() {
                            "high" => crate::memory::TrustLevel::High,
                            "low" => crate::memory::TrustLevel::Low,
                            _ => crate::memory::TrustLevel::Medium,
                        };

                        let entry = crate::memory::MemoryEntry::new(category, &mem.content)
                            .with_source("manual")
                            .with_trust(trust);

                        if manager.remember_project(entry).is_ok() {
                            stored_count += 1;
                        }
                    }

                    crate::logging::info(&format!(
                        "/remember: extracted {} memories",
                        stored_count
                    ));
                    crate::memory::add_event(MemoryEventKind::ExtractionComplete {
                        count: stored_count,
                    });
                    crate::memory::set_state(MemoryState::Idle);
                }
                Ok(_) => {
                    crate::logging::info("/remember: no memories extracted");
                    crate::memory::set_state(MemoryState::Idle);
                }
                Err(e) => {
                    crate::logging::error(&format!("/remember failed: {}", e));
                    crate::memory::add_event(MemoryEventKind::Error {
                        message: e.to_string(),
                    });
                    crate::memory::set_state(MemoryState::Idle);
                }
            }
        });

        return true;
    }

    if trimmed == "/config" {
        use crate::config::config;
        app.push_display_message(DisplayMessage {
            role: "system".to_string(),
            content: config().display_string(),
            tool_calls: vec![],
            duration_secs: None,
            title: None,
            tool_data: None,
        });
        return true;
    }

    if trimmed == "/config init" || trimmed == "/config create" {
        use crate::config::Config;
        match Config::create_default_config_file() {
            Ok(path) => {
                app.push_display_message(DisplayMessage {
                    role: "system".to_string(),
                    content: format!(
                        "Created default config file at:\n`{}`\n\nEdit this file to customize your keybindings and settings.",
                        path.display()
                    ),
                    tool_calls: vec![],
                    duration_secs: None,
                    title: None,
                    tool_data: None,
                });
            }
            Err(e) => {
                app.push_display_message(DisplayMessage {
                    role: "system".to_string(),
                    content: format!("Failed to create config file: {}", e),
                    tool_calls: vec![],
                    duration_secs: None,
                    title: None,
                    tool_data: None,
                });
            }
        }
        return true;
    }

    if trimmed == "/config edit" {
        use crate::config::Config;
        if let Some(path) = Config::path() {
            if !path.exists() {
                if let Err(e) = Config::create_default_config_file() {
                    app.push_display_message(DisplayMessage {
                        role: "system".to_string(),
                        content: format!("Failed to create config file: {}", e),
                        tool_calls: vec![],
                        duration_secs: None,
                        title: None,
                        tool_data: None,
                    });
                    return true;
                }
            }

            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nano".to_string());
            app.push_display_message(DisplayMessage {
                role: "system".to_string(),
                content: format!(
                    "Opening config in editor...\n`{} {}`\n\n*Restart jcode after editing for changes to take effect.*",
                    editor,
                    path.display()
                ),
                tool_calls: vec![],
                duration_secs: None,
                title: None,
                tool_data: None,
            });

            let _ = std::process::Command::new(&editor).arg(&path).spawn();
        }
        return true;
    }

    if trimmed.starts_with("/config ") {
        app.push_display_message(DisplayMessage::error(
            "Usage: `/config` (show), `/config init` (create), `/config edit` (open in editor)"
                .to_string(),
        ));
        return true;
    }

    false
}

pub(super) fn handle_debug_command(app: &mut App, trimmed: &str) -> bool {
    if trimmed == "/debug-visual" || trimmed == "/debug-visual on" {
        use crate::tui::visual_debug;
        visual_debug::enable();
        app.push_display_message(DisplayMessage {
            role: "system".to_string(),
            content: "Visual debugging enabled. Frames are being captured.\n\
                     Use `/debug-visual dump` to write captured frames to file.\n\
                     Use `/debug-visual off` to disable."
                .to_string(),
            tool_calls: vec![],
            duration_secs: None,
            title: None,
            tool_data: None,
        });
        app.set_status_notice("Visual debug: ON");
        return true;
    }

    if trimmed == "/debug-visual off" {
        use crate::tui::visual_debug;
        visual_debug::disable();
        app.push_display_message(DisplayMessage {
            role: "system".to_string(),
            content: "Visual debugging disabled.".to_string(),
            tool_calls: vec![],
            duration_secs: None,
            title: None,
            tool_data: None,
        });
        app.set_status_notice("Visual debug: OFF");
        return true;
    }

    if trimmed == "/debug-visual dump" {
        use crate::tui::visual_debug;
        match visual_debug::dump_to_file() {
            Ok(path) => {
                app.push_display_message(DisplayMessage {
                    role: "system".to_string(),
                    content: format!(
                        "Visual debug dump written to:\n`{}`\n\n\
                         This file contains frame captures with:\n\
                         - Layout computations\n\
                         - State snapshots\n\
                         - Rendered text content\n\
                         - Any detected anomalies",
                        path.display()
                    ),
                    tool_calls: vec![],
                    duration_secs: None,
                    title: None,
                    tool_data: None,
                });
            }
            Err(e) => {
                app.push_display_message(DisplayMessage {
                    role: "error".to_string(),
                    content: format!("Failed to write visual debug dump: {}", e),
                    tool_calls: vec![],
                    duration_secs: None,
                    title: None,
                    tool_data: None,
                });
            }
        }
        return true;
    }

    if trimmed.starts_with("/debug-visual ") {
        app.push_display_message(DisplayMessage::error(
            "Usage: `/debug-visual` (on), `/debug-visual off`, `/debug-visual dump`".to_string(),
        ));
        return true;
    }

    if trimmed == "/screenshot-mode" || trimmed == "/screenshot-mode on" {
        use crate::tui::screenshot;
        screenshot::enable();
        app.push_display_message(DisplayMessage {
            role: "system".to_string(),
            content: "Screenshot mode enabled.\n\n\
                     Run the watcher in another terminal:\n\
                     ```bash\n\
                     ./scripts/screenshot_watcher.sh\n\
                     ```\n\n\
                     Use `/screenshot <state>` to trigger a capture.\n\
                     Use `/screenshot-mode off` to disable."
                .to_string(),
            tool_calls: vec![],
            duration_secs: None,
            title: None,
            tool_data: None,
        });
        return true;
    }

    if trimmed == "/screenshot-mode off" {
        use crate::tui::screenshot;
        screenshot::disable();
        screenshot::clear_all_signals();
        app.push_display_message(DisplayMessage {
            role: "system".to_string(),
            content: "Screenshot mode disabled.".to_string(),
            tool_calls: vec![],
            duration_secs: None,
            title: None,
            tool_data: None,
        });
        return true;
    }

    if trimmed.starts_with("/screenshot ") {
        use crate::tui::screenshot;
        let state_name = trimmed.strip_prefix("/screenshot ").unwrap_or("").trim();
        if !state_name.is_empty() {
            screenshot::signal_ready(
                state_name,
                serde_json::json!({
                    "manual_trigger": true,
                }),
            );
            app.push_display_message(DisplayMessage {
                role: "system".to_string(),
                content: format!("Screenshot signal sent: {}", state_name),
                tool_calls: vec![],
                duration_secs: None,
                title: None,
                tool_data: None,
            });
        }
        return true;
    }

    if trimmed == "/record" || trimmed == "/record start" {
        use crate::tui::test_harness;
        test_harness::start_recording();
        app.push_display_message(DisplayMessage {
            role: "system".to_string(),
            content: "🎬 Recording started.\n\n\
                     All your keystrokes are now being recorded.\n\
                     Use `/record stop` to stop and save.\n\
                     Use `/record cancel` to discard."
                .to_string(),
            tool_calls: vec![],
            duration_secs: None,
            title: None,
            tool_data: None,
        });
        return true;
    }

    if trimmed == "/record stop" {
        use crate::tui::test_harness;
        test_harness::stop_recording();
        let json = test_harness::get_recorded_events_json();
        let event_count = json.matches("\"type\"").count();

        let recording_dir = dirs::config_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("jcode")
            .join("recordings");
        let _ = std::fs::create_dir_all(&recording_dir);

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let filename = format!("recording_{}.json", timestamp);
        let filepath = recording_dir.join(&filename);

        if let Ok(mut file) = std::fs::File::create(&filepath) {
            use std::io::Write;
            let _ = file.write_all(json.as_bytes());
        }

        app.push_display_message(DisplayMessage {
            role: "system".to_string(),
            content: format!(
                "🎬 Recording stopped.\n\n\
                 **Events recorded:** {}\n\
                 **Saved to:** `{}`\n\n\
                 To replay as video, run:\n\
                 ```bash\n\
                 ./scripts/replay_recording.sh {}\n\
                 ```",
                event_count,
                filepath.display(),
                filepath.display()
            ),
            tool_calls: vec![],
            duration_secs: None,
            title: None,
            tool_data: None,
        });
        return true;
    }

    if trimmed == "/record cancel" {
        use crate::tui::test_harness;
        test_harness::stop_recording();
        app.push_display_message(DisplayMessage {
            role: "system".to_string(),
            content: "🎬 Recording cancelled.".to_string(),
            tool_calls: vec![],
            duration_secs: None,
            title: None,
            tool_data: None,
        });
        return true;
    }

    if trimmed.starts_with("/record ") {
        app.push_display_message(DisplayMessage::error(
            "Usage: `/record` (start), `/record stop`, `/record cancel`".to_string(),
        ));
        return true;
    }

    false
}

pub(super) fn handle_model_command(app: &mut App, trimmed: &str) -> bool {
    if trimmed == "/model" || trimmed == "/models" {
        app.open_model_picker();
        return true;
    }

    if let Some(model_name) = trimmed.strip_prefix("/model ") {
        let model_name = model_name.trim();
        match app.provider.set_model(model_name) {
            Ok(()) => {
                app.provider_session_id = None;
                app.session.provider_session_id = None;
                app.upstream_provider = None;
                app.connection_type = None;
                let active_model = app.provider.model();
                app.update_context_limit_for_model(&active_model);
                app.session.model = Some(active_model.clone());
                let _ = app.session.save();
                app.push_display_message(DisplayMessage {
                    role: "system".to_string(),
                    content: format!("✓ Switched to model: {}", active_model),
                    tool_calls: vec![],
                    duration_secs: None,
                    title: None,
                    tool_data: None,
                });
                app.set_status_notice(format!("Model → {}", model_name));
            }
            Err(e) => {
                app.push_display_message(DisplayMessage {
                    role: "error".to_string(),
                    content: format!("Failed to switch model: {}", e),
                    tool_calls: vec![],
                    duration_secs: None,
                    title: None,
                    tool_data: None,
                });
                app.set_status_notice("Model switch failed");
            }
        }
        return true;
    }

    if trimmed == "/effort" {
        let current = app.provider.reasoning_effort();
        let efforts = app.provider.available_efforts();
        if efforts.is_empty() {
            app.push_display_message(DisplayMessage::system(
                "Reasoning effort not available for this provider.".to_string(),
            ));
        } else {
            let current_label = current
                .as_deref()
                .map(super::effort_display_label)
                .unwrap_or("default");
            let list: Vec<String> = efforts
                .iter()
                .map(|e| {
                    if Some(e.to_string()) == current {
                        format!("**{}** ← current", super::effort_display_label(e))
                    } else {
                        super::effort_display_label(e).to_string()
                    }
                })
                .collect();
            app.push_display_message(DisplayMessage::system(format!(
                "Reasoning effort: {}\nAvailable: {}\nUse `/effort <level>` or Alt+←/→ to change.",
                current_label,
                list.join(" · ")
            )));
        }
        return true;
    }

    if let Some(level) = trimmed.strip_prefix("/effort ") {
        let level = level.trim();
        match app.provider.set_reasoning_effort(level) {
            Ok(()) => {
                let new_effort = app.provider.reasoning_effort();
                let label = new_effort
                    .as_deref()
                    .map(super::effort_display_label)
                    .unwrap_or("default");
                app.push_display_message(DisplayMessage::system(format!(
                    "✓ Reasoning effort → {}",
                    label
                )));
                let efforts = app.provider.available_efforts();
                let idx = new_effort
                    .as_ref()
                    .and_then(|e| efforts.iter().position(|x| *x == e.as_str()))
                    .unwrap_or(0);
                let bar = super::effort_bar(idx, efforts.len());
                app.set_status_notice(format!("Effort: {} {}", label, bar));
            }
            Err(e) => {
                app.push_display_message(DisplayMessage::error(format!(
                    "Failed to set effort: {}",
                    e
                )));
            }
        }
        return true;
    }

    false
}

pub(super) fn handle_info_command(app: &mut App, trimmed: &str) -> bool {
    if trimmed == "/version" {
        let version = env!("JCODE_VERSION");
        let is_canary = if app.session.is_canary {
            " (canary/self-dev)"
        } else {
            ""
        };
        app.push_display_message(DisplayMessage {
            role: "system".to_string(),
            content: format!("jcode {}{}", version, is_canary),
            tool_calls: vec![],
            duration_secs: None,
            title: None,
            tool_data: None,
        });
        return true;
    }

    if trimmed == "/changelog" {
        app.changelog_scroll = Some(0);
        return true;
    }

    if trimmed == "/cache" || trimmed.starts_with("/cache ") {
        let arg = trimmed.strip_prefix("/cache").unwrap_or("").trim();
        match arg {
            "1h" | "1hour" | "extended" => {
                crate::provider::anthropic::set_cache_ttl_1h(true);
                app.push_display_message(DisplayMessage::system(
                    "Cache TTL set to 1 hour. Cache writes cost 2x base input tokens.".to_string(),
                ));
            }
            "5m" | "5min" | "default" | "reset" => {
                crate::provider::anthropic::set_cache_ttl_1h(false);
                app.push_display_message(DisplayMessage::system(
                    "Cache TTL set to 5 minutes (default).".to_string(),
                ));
            }
            "" => {
                let current = crate::provider::anthropic::is_cache_ttl_1h();
                let new_state = !current;
                crate::provider::anthropic::set_cache_ttl_1h(new_state);
                let msg = if new_state {
                    "Cache TTL toggled to 1 hour. Cache writes cost 2x base input tokens.\nUse `/cache 5m` to revert."
                } else {
                    "Cache TTL toggled to 5 minutes (default).\nUse `/cache 1h` to extend."
                };
                app.push_display_message(DisplayMessage::system(msg.to_string()));
            }
            _ => {
                app.push_display_message(DisplayMessage::error(
                    "Usage: `/cache` (toggle), `/cache 1h` (1 hour), `/cache 5m` (default)"
                        .to_string(),
                ));
            }
        }
        return true;
    }

    if trimmed == "/info" {
        let version = env!("JCODE_VERSION");
        let terminal_size = crossterm::terminal::size()
            .map(|(w, h)| format!("{}x{}", w, h))
            .unwrap_or_else(|_| "unknown".to_string());
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown".to_string());

        let turn_count = app
            .display_messages
            .iter()
            .filter(|m| m.role == "user")
            .count();

        let session_duration = chrono::Utc::now().signed_duration_since(app.session.created_at);
        let duration_str = if session_duration.num_hours() > 0 {
            format!(
                "{}h {}m",
                session_duration.num_hours(),
                session_duration.num_minutes() % 60
            )
        } else if session_duration.num_minutes() > 0 {
            format!("{}m", session_duration.num_minutes())
        } else {
            format!("{}s", session_duration.num_seconds())
        };

        let mut info = String::new();
        info.push_str(&format!("**Version:** {}\n", version));
        info.push_str(&format!(
            "**Session:** {} ({})\n",
            app.session.short_name.as_deref().unwrap_or("unnamed"),
            &app.session.id[..8]
        ));
        info.push_str(&format!(
            "**Duration:** {} ({} turns)\n",
            duration_str, turn_count
        ));
        info.push_str(&format!(
            "**Tokens:** ↑{} ↓{}\n",
            app.total_input_tokens, app.total_output_tokens
        ));
        info.push_str(&format!("**Terminal:** {}\n", terminal_size));
        info.push_str(&format!("**CWD:** {}\n", cwd));
        info.push_str(&format!(
            "**Features:** memory={}, swarm={}\n",
            if app.memory_enabled { "on" } else { "off" },
            if app.swarm_enabled { "on" } else { "off" }
        ));

        if let Some(ref model) = app.remote_provider_model {
            info.push_str(&format!("**Model:** {}\n", model));
        }
        if let Some(ref provider_id) = app.provider_session_id {
            info.push_str(&format!(
                "**Provider Session:** {}...\n",
                &provider_id[..provider_id.len().min(16)]
            ));
        }

        if app.session.is_canary {
            info.push_str("\n**Self-Dev Mode:** enabled\n");
            if let Some(ref build) = app.session.testing_build {
                info.push_str(&format!("**Testing Build:** {}\n", build));
            }
        }

        if app.is_remote {
            info.push_str(&format!("\n**Remote Mode:** connected\n"));
            if let Some(count) = app.remote_client_count {
                info.push_str(&format!("**Connected Clients:** {}\n", count));
            }
        }

        app.push_display_message(DisplayMessage {
            role: "system".to_string(),
            content: info,
            tool_calls: vec![],
            duration_secs: None,
            title: None,
            tool_data: None,
        });
        return true;
    }

    false
}

pub(super) fn handle_auth_command(app: &mut App, trimmed: &str) -> bool {
    if trimmed == "/auth" {
        app.show_auth_status();
        return true;
    }

    if trimmed == "/login" {
        app.show_interactive_login();
        return true;
    }

    if let Some(provider) = trimmed
        .strip_prefix("/login ")
        .or_else(|| trimmed.strip_prefix("/auth "))
    {
        let providers = crate::provider_catalog::tui_login_providers();
        if let Some(provider) =
            crate::provider_catalog::resolve_login_selection(provider, &providers)
        {
            app.start_login_provider(provider);
        } else {
            let valid = providers
                .iter()
                .map(|provider| provider.id)
                .collect::<Vec<_>>()
                .join(", ");
            app.push_display_message(DisplayMessage::error(format!(
                "Unknown provider '{}'. Use: {}",
                provider.trim(),
                valid
            )));
        }
        return true;
    }

    if trimmed == "/account" || trimmed == "/accounts" {
        app.show_accounts();
        return true;
    }

    if let Some(sub) = trimmed.strip_prefix("/account ") {
        let parts: Vec<&str> = sub.trim().splitn(2, ' ').collect();
        match parts[0] {
            "list" | "ls" => app.show_accounts(),
            "switch" | "use" => {
                if let Some(label) = parts.get(1) {
                    app.switch_account(label.trim());
                } else {
                    app.push_display_message(DisplayMessage::error(
                        "Usage: `/account switch <label>`".to_string(),
                    ));
                }
            }
            "add" | "login" => {
                let label = parts.get(1).map(|s| s.trim()).unwrap_or("default");
                app.start_claude_login_for_account(label);
            }
            "remove" | "rm" | "delete" => {
                if let Some(label) = parts.get(1) {
                    app.remove_account(label.trim());
                } else {
                    app.push_display_message(DisplayMessage::error(
                        "Usage: `/account remove <label>`".to_string(),
                    ));
                }
            }
            other => {
                let accounts = crate::auth::claude::list_accounts().unwrap_or_default();
                if accounts.iter().any(|a| a.label == other) {
                    app.switch_account(other);
                } else {
                    app.push_display_message(DisplayMessage::error(format!(
                        "Unknown subcommand '{}'. Use: list, switch, add, remove",
                        other
                    )));
                }
            }
        }
        return true;
    }

    false
}

pub(super) fn handle_dev_command(app: &mut App, trimmed: &str) -> bool {
    if trimmed == "/reload" {
        if !app.has_newer_binary() {
            app.push_display_message(DisplayMessage {
                role: "system".to_string(),
                content: "No newer binary found. Nothing to reload.\nUse /rebuild to build a new version.".to_string(),
                tool_calls: vec![],
                duration_secs: None,
                title: None,
                tool_data: None,
            });
            return true;
        }
        app.push_display_message(DisplayMessage {
            role: "system".to_string(),
            content: "Reloading with newer binary...".to_string(),
            tool_calls: vec![],
            duration_secs: None,
            title: None,
            tool_data: None,
        });
        app.session.provider_session_id = app.provider_session_id.clone();
        app.session
            .set_status(crate::session::SessionStatus::Reloaded);
        let _ = app.session.save();
        app.save_input_for_reload(&app.session.id.clone());
        app.reload_requested = Some(app.session.id.clone());
        app.should_quit = true;
        return true;
    }

    if trimmed == "/rebuild" {
        app.push_display_message(DisplayMessage {
            role: "system".to_string(),
            content: "Rebuilding jcode (git pull + cargo build + tests)...".to_string(),
            tool_calls: vec![],
            duration_secs: None,
            title: None,
            tool_data: None,
        });
        app.session.provider_session_id = app.provider_session_id.clone();
        app.session
            .set_status(crate::session::SessionStatus::Reloaded);
        let _ = app.session.save();
        app.rebuild_requested = Some(app.session.id.clone());
        app.should_quit = true;
        return true;
    }

    if trimmed == "/update" {
        app.push_display_message(DisplayMessage::system(
            "Checking for updates...".to_string(),
        ));
        app.session.provider_session_id = app.provider_session_id.clone();
        app.session
            .set_status(crate::session::SessionStatus::Reloaded);
        let _ = app.session.save();
        app.update_requested = Some(app.session.id.clone());
        app.should_quit = true;
        return true;
    }

    if trimmed == "/z" || trimmed == "/zz" || trimmed == "/zzz" || trimmed == "/zstatus" {
        use crate::provider::copilot::PremiumMode;
        let current = app.provider.premium_mode();

        if trimmed == "/zstatus" {
            let label = match current {
                PremiumMode::Normal => "normal",
                PremiumMode::OnePerSession => "one premium per session",
                PremiumMode::Zero => "zero premium requests",
            };
            let env = std::env::var("JCODE_COPILOT_PREMIUM").ok();
            let env_label = match env.as_deref() {
                Some("0") => "0 (zero)",
                Some("1") => "1 (one per session)",
                _ => "unset (normal)",
            };
            app.push_display_message(DisplayMessage::system(format!(
                "Premium mode: **{}**\nEnv JCODE_COPILOT_PREMIUM: {}",
                label, env_label,
            )));
            return true;
        }

        if trimmed == "/z" {
            app.provider.set_premium_mode(PremiumMode::Normal);
            let _ = crate::config::Config::set_copilot_premium(None);
            app.set_status_notice("Premium: normal");
            app.push_display_message(DisplayMessage::system(
                "Premium request mode reset to normal. (saved to config)".to_string(),
            ));
            return true;
        }

        let mode = if trimmed == "/zzz" {
            PremiumMode::Zero
        } else {
            PremiumMode::OnePerSession
        };
        if current == mode {
            app.provider.set_premium_mode(PremiumMode::Normal);
            let _ = crate::config::Config::set_copilot_premium(None);
            app.set_status_notice("Premium: normal");
            app.push_display_message(DisplayMessage::system(
                "Premium request mode reset to normal. (saved to config)".to_string(),
            ));
        } else {
            app.provider.set_premium_mode(mode);
            let config_val = match mode {
                PremiumMode::Zero => "zero",
                PremiumMode::OnePerSession => "one",
                PremiumMode::Normal => "normal",
            };
            let _ = crate::config::Config::set_copilot_premium(Some(config_val));
            let label = match mode {
                PremiumMode::OnePerSession => "one premium per session",
                PremiumMode::Zero => "zero premium requests",
                PremiumMode::Normal => "normal",
            };
            app.set_status_notice(&format!("Premium: {}", label));
            app.push_display_message(DisplayMessage::system(format!(
                "Premium mode: **{}**. Toggle off with `/z`. (saved to config)",
                label,
            )));
        }
        return true;
    }

    false
}
