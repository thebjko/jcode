use super::{App, SendAction};

pub(super) fn paste_image_from_clipboard(app: &mut App) {
    if let Some((media_type, base64_data)) = super::clipboard_image() {
        attach_image(app, media_type, base64_data);
        return;
    }

    if let Ok(mut clipboard) = arboard::Clipboard::new() {
        if let Ok(text) = clipboard.get_text() {
            if let Some(url) = super::extract_image_url(&text) {
                app.set_status_notice("Downloading image...");
                if let Some((media_type, base64_data)) = super::download_image_url(&url) {
                    attach_image(app, media_type, base64_data);
                } else {
                    app.set_status_notice("Failed to download image");
                }
            } else {
                handle_paste(app, text);
            }
            return;
        }
    }

    app.set_status_notice("No image in clipboard");
}

pub(super) fn handle_paste(app: &mut App, text: String) {
    // Note: clipboard_image() is NOT checked here. Bracketed paste events from the
    // terminal always deliver text. Checking clipboard_image() here caused a bug where
    // text pastes were misidentified as images when the clipboard also had image data
    // (common on Wayland where apps advertise multiple MIME types). Image pasting is
    // handled by paste_image_from_clipboard() (Ctrl+V / Alt+V) instead.
    if let Some(url) = super::extract_image_url(&text) {
        crate::logging::info(&format!("Downloading image from pasted URL: {}", url));
        app.set_status_notice("Downloading image...");
        if let Some((media_type, base64_data)) = super::download_image_url(&url) {
            attach_image(app, media_type, base64_data);
            return;
        }
        app.set_status_notice("Failed to download image");
    }

    crate::logging::info(&format!(
        "Text paste: {} chars, {} lines",
        text.len(),
        text.lines().count()
    ));

    let line_count = text.lines().count().max(1);
    if line_count < 5 {
        app.input.insert_str(app.cursor_pos, &text);
        app.cursor_pos += text.len();
    } else {
        app.pasted_contents.push(text);
        let placeholder = format!(
            "[pasted {} line{}]",
            line_count,
            if line_count == 1 { "" } else { "s" }
        );
        app.input.insert_str(app.cursor_pos, &placeholder);
        app.cursor_pos += placeholder.len();
    }
    app.sync_model_picker_preview_from_input();
}

pub(super) fn expand_paste_placeholders(app: &mut App, input: &str) -> String {
    let mut result = input.to_string();
    for content in app.pasted_contents.iter().rev() {
        let placeholder = paste_placeholder(content);
        if let Some(pos) = result.rfind(&placeholder) {
            result.replace_range(pos..pos + placeholder.len(), content);
        }
    }
    result
}

pub(super) fn queue_message(app: &mut App) {
    let content = std::mem::take(&mut app.input);
    let expanded = expand_paste_placeholders(app, &content);
    app.pasted_contents.clear();
    app.pending_images.clear();
    app.cursor_pos = 0;
    app.queued_messages.push(expanded);
}

pub(super) fn retrieve_pending_message_for_edit(app: &mut App) -> bool {
    if !app.input.is_empty() {
        return false;
    }

    let mut parts: Vec<String> = Vec::new();
    let mut had_pending = false;

    if !app.pending_soft_interrupts.is_empty() {
        parts.extend(std::mem::take(&mut app.pending_soft_interrupts));
        had_pending = true;
    }
    if let Some(msg) = app.interleave_message.take() {
        if !msg.is_empty() {
            parts.push(msg);
        }
    }
    parts.extend(std::mem::take(&mut app.queued_messages));

    if !parts.is_empty() {
        app.input = parts.join("\n\n");
        app.cursor_pos = app.input.len();
        let count = parts.len();
        app.set_status_notice(&format!(
            "Retrieved {} pending message{} for editing",
            count,
            if count == 1 { "" } else { "s" }
        ));
    }

    had_pending
}

pub(super) fn send_action(app: &App, shift: bool) -> SendAction {
    if !app.is_processing {
        return SendAction::Submit;
    }
    if app.input.trim().starts_with('/') {
        return SendAction::Submit;
    }
    if shift {
        if app.queue_mode {
            SendAction::Interleave
        } else {
            SendAction::Queue
        }
    } else if app.queue_mode {
        SendAction::Queue
    } else {
        SendAction::Interleave
    }
}

fn attach_image(app: &mut App, media_type: String, base64_data: String) {
    let size_kb = base64_data.len() / 1024;
    app.pending_images.push((media_type.clone(), base64_data));
    let placeholder = format!("[image {}]", app.pending_images.len());
    app.input.insert_str(app.cursor_pos, &placeholder);
    app.cursor_pos += placeholder.len();
    app.sync_model_picker_preview_from_input();
    app.set_status_notice(&format!("Pasted {} ({} KB)", media_type, size_kb));
}

fn paste_placeholder(content: &str) -> String {
    let line_count = content.lines().count().max(1);
    format!(
        "[pasted {} line{}]",
        line_count,
        if line_count == 1 { "" } else { "s" }
    )
}
