use super::*;

impl App {
    pub(in crate::tui::app) fn debug_memory_profile(&self) -> serde_json::Value {
        let process = crate::process_memory::snapshot_with_source("client:memory");
        let markdown = crate::tui::markdown::debug_memory_profile();
        let mermaid = crate::tui::mermaid::debug_memory_profile();
        let visual_debug = crate::tui::visual_debug::debug_memory_profile();
        let mcp = self
            .mcp_manager
            .try_read()
            .map(|manager| manager.debug_memory_profile())
            .ok();
        let (provider_view_source, materialized_provider_messages) =
            if self.is_remote || !self.messages.is_empty() {
                ("resident_ui", self.messages.clone())
            } else {
                (
                    "session_materialized",
                    self.session.messages_for_provider_uncached(),
                )
            };
        let transcript_memory = crate::tui::transcript_memory_profile(
            &self.session,
            &self.messages,
            &materialized_provider_messages,
            provider_view_source,
            &self.display_messages,
            &self.side_panel,
        );

        let provider_messages_json_bytes: usize = self
            .messages
            .iter()
            .map(crate::process_memory::estimate_json_bytes)
            .sum();
        let mut provider_message_memory = ProviderMessageMemoryStats::default();
        for message in &self.messages {
            provider_message_memory.record_message(message);
        }
        let display_messages_bytes: usize = self
            .display_messages
            .iter()
            .map(estimate_display_message_bytes)
            .sum();
        let mut display_message_memory = DisplayMessageMemoryStats::default();
        for message in &self.display_messages {
            display_message_memory.record_message(message);
        }
        let streaming_tool_calls_json_bytes: usize = self
            .streaming_tool_calls
            .iter()
            .map(crate::process_memory::estimate_json_bytes)
            .sum();
        let remote_model_options_json_bytes: usize = self
            .remote_model_options
            .iter()
            .map(crate::process_memory::estimate_json_bytes)
            .sum();
        let remote_total_tokens_json_bytes = self
            .remote_total_tokens
            .as_ref()
            .map(crate::process_memory::estimate_json_bytes)
            .unwrap_or(0);

        serde_json::json!({
            "process": process,
            "session": self.session.debug_memory_profile(),
            "markdown": markdown,
            "mermaid": mermaid,
            "visual_debug": visual_debug,
            "ui": {
                "provider_messages": {
                    "count": self.messages.len(),
                    "json_bytes": provider_messages_json_bytes,
                    "content_blocks": provider_message_memory.content_blocks,
                    "payload_text_bytes": provider_message_memory.payload_text_bytes(),
                    "text_bytes": provider_message_memory.text_bytes,
                    "reasoning_bytes": provider_message_memory.reasoning_bytes,
                    "tool_use_input_json_bytes": provider_message_memory.tool_use_input_json_bytes,
                    "tool_result_bytes": provider_message_memory.tool_result_bytes,
                    "image_data_bytes": provider_message_memory.image_data_bytes,
                    "openai_compaction_bytes": provider_message_memory.openai_compaction_bytes,
                    "large_blob_count": provider_message_memory.large_blob_count,
                    "large_blob_bytes": provider_message_memory.large_blob_bytes,
                    "large_tool_result_count": provider_message_memory.large_tool_result_count,
                    "large_tool_result_bytes": provider_message_memory.large_tool_result_bytes,
                    "max_block_bytes": provider_message_memory.max_block_bytes,
                },
                "display_messages": {
                    "count": self.display_messages.len(),
                    "estimate_bytes": display_messages_bytes,
                    "role_bytes": display_message_memory.role_bytes,
                    "content_bytes": display_message_memory.content_bytes,
                    "tool_call_text_bytes": display_message_memory.tool_call_text_bytes,
                    "title_bytes": display_message_memory.title_bytes,
                    "tool_data_json_bytes": display_message_memory.tool_data_json_bytes,
                    "large_content_count": display_message_memory.large_content_count,
                    "large_content_bytes": display_message_memory.large_content_bytes,
                    "max_content_bytes": display_message_memory.max_content_bytes,
                },
                "transcript_memory": transcript_memory,
                "input": {
                    "text_bytes": self.input.len(),
                    "cursor_pos": self.cursor_pos,
                },
                "streaming": {
                    "streaming_text_bytes": self.streaming_text.len(),
                    "thinking_buffer_bytes": self.thinking_buffer.len(),
                    "stream_buffer": self.stream_buffer.debug_memory_profile(),
                    "streaming_tool_calls_count": self.streaming_tool_calls.len(),
                    "streaming_tool_calls_json_bytes": streaming_tool_calls_json_bytes,
                },
                "queued_messages": {
                    "visible_count": self.queued_messages.len(),
                    "visible_text_bytes": estimate_string_vec_bytes(&self.queued_messages),
                    "hidden_count": self.hidden_queued_system_messages.len(),
                    "hidden_text_bytes": estimate_string_vec_bytes(&self.hidden_queued_system_messages),
                    "current_turn_system_reminder_bytes": self.current_turn_system_reminder.as_ref().map(|value| value.len()).unwrap_or(0),
                },
                "clipboard_and_input_media": {
                    "pasted_contents_count": self.pasted_contents.len(),
                    "pasted_contents_bytes": estimate_string_vec_bytes(&self.pasted_contents),
                    "pending_images_count": self.pending_images.len(),
                    "pending_images_bytes": estimate_pending_images_bytes(&self.pending_images),
                },
                "remote_state": {
                    "available_entries_count": self.remote_available_entries.len(),
                    "available_entries_bytes": estimate_string_vec_bytes(&self.remote_available_entries),
                    "model_options_count": self.remote_model_options.len(),
                    "model_options_json_bytes": remote_model_options_json_bytes,
                    "skills_count": self.remote_skills.len(),
                    "skills_bytes": estimate_string_vec_bytes(&self.remote_skills),
                    "mcp_servers_count": self.remote_mcp_servers.len(),
                    "mcp_servers_bytes": estimate_string_vec_bytes(&self.remote_mcp_servers),
                    "mcp_server_names_count": self.mcp_server_names.len(),
                    "mcp_server_names_bytes": estimate_pair_vec_bytes(&self.mcp_server_names),
                    "remote_total_tokens_json_bytes": remote_total_tokens_json_bytes,
                },
                "skills": {
                    "available_count": self.skills.list().len(),
                },
                "mcp": mcp,
            },
            "history": crate::process_memory::history(64),
        })
    }
}
