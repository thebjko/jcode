use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SingleSessionTextKey {
    pub(crate) size: (u32, u32),
    pub(crate) fresh_welcome_visible: bool,
    pub(crate) title: String,
    pub(crate) version: String,
    pub(crate) welcome_hero: String,
    pub(crate) welcome_hint: Vec<SingleSessionStyledLine>,
    pub(crate) activity_active: bool,
    pub(crate) welcome_handoff_visible: bool,
    pub(crate) body: Vec<SingleSessionStyledLine>,
    pub(crate) draft: String,
    pub(crate) status: String,
}

#[cfg(test)]
pub(crate) fn build_single_session_vertices(
    app: &SingleSessionApp,
    size: PhysicalSize<u32>,
    focus_pulse: f32,
    spinner_tick: u64,
) -> Vec<Vertex> {
    build_single_session_vertices_with_scroll(app, size, focus_pulse, spinner_tick, 0.0)
}

pub(crate) fn build_single_session_vertices_with_scroll(
    app: &SingleSessionApp,
    size: PhysicalSize<u32>,
    focus_pulse: f32,
    spinner_tick: u64,
    smooth_scroll_lines: f32,
) -> Vec<Vertex> {
    let width = size.width as f32;
    let height = size.height as f32;
    let mut vertices = Vec::new();

    push_gradient_rect(
        &mut vertices,
        Rect {
            x: 0.0,
            y: 0.0,
            width,
            height,
        },
        BACKGROUND_TOP_LEFT,
        BACKGROUND_BOTTOM_LEFT,
        BACKGROUND_BOTTOM_RIGHT,
        BACKGROUND_TOP_RIGHT,
        size,
    );

    let rect = Rect {
        x: 0.0,
        y: 0.0,
        width: width.max(1.0),
        height: height.max(1.0),
    };
    let surface = single_session_surface(app.session.as_ref());
    push_surface(
        &mut vertices,
        rect,
        surface.color_index,
        true,
        focus_pulse,
        size,
    );

    if app.has_activity_indicator() {
        push_native_activity_spinner(&mut vertices, size, spinner_tick);
    }
    push_single_session_transcript_cards(
        &mut vertices,
        app,
        size,
        spinner_tick,
        smooth_scroll_lines,
    );
    push_single_session_streaming_shimmer(
        &mut vertices,
        app,
        size,
        spinner_tick,
        smooth_scroll_lines,
    );
    push_single_session_selection(&mut vertices, app, size);
    push_single_session_scrollbar(&mut vertices, app, size, spinner_tick, smooth_scroll_lines);

    vertices
}

pub(crate) fn push_native_activity_spinner(
    vertices: &mut Vec<Vertex>,
    size: PhysicalSize<u32>,
    tick: u64,
) {
    let typography = single_session_typography();
    let draft_top = single_session_draft_top(size);
    let center = [
        size.width as f32 - PANEL_TITLE_LEFT_PADDING - 12.0,
        draft_top - SINGLE_SESSION_STATUS_GAP + 7.0,
    ];
    let radius = (typography.meta_size * 0.54).clamp(5.0, 9.0);
    let thickness = 2.4;
    let segments = 12;
    let phase = (tick as usize) % segments;
    for segment in 0..segments {
        let age = (segment + segments - phase) % segments;
        let alpha_scale = if age == 0 {
            1.0
        } else {
            0.18 + (segments - age) as f32 / segments as f32 * 0.52
        };
        let mut color = if age == 0 {
            NATIVE_SPINNER_HEAD_COLOR
        } else {
            NATIVE_SPINNER_TRACK_COLOR
        };
        color[3] = (color[3] * alpha_scale).clamp(0.08, 1.0);
        let start =
            -std::f32::consts::FRAC_PI_2 + segment as f32 / segments as f32 * std::f32::consts::TAU;
        let end = start + std::f32::consts::TAU / segments as f32 * 0.64;
        push_spinner_segment(vertices, center, radius, thickness, start, end, color, size);
    }
}

fn push_spinner_segment(
    vertices: &mut Vec<Vertex>,
    center: [f32; 2],
    radius: f32,
    thickness: f32,
    start: f32,
    end: f32,
    color: [f32; 4],
    size: PhysicalSize<u32>,
) {
    let inner_radius = (radius - thickness).max(1.0);
    let outer_start = [
        center[0] + radius * start.cos(),
        center[1] + radius * start.sin(),
    ];
    let outer_end = [
        center[0] + radius * end.cos(),
        center[1] + radius * end.sin(),
    ];
    let inner_start = [
        center[0] + inner_radius * start.cos(),
        center[1] + inner_radius * start.sin(),
    ];
    let inner_end = [
        center[0] + inner_radius * end.cos(),
        center[1] + inner_radius * end.sin(),
    ];
    push_pixel_triangle(vertices, outer_start, outer_end, inner_end, color, size);
    push_pixel_triangle(vertices, outer_start, inner_end, inner_start, color, size);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SingleSessionTranscriptCardRun {
    pub(crate) line: usize,
    pub(crate) line_count: usize,
    pub(crate) style: SingleSessionLineStyle,
}

fn push_single_session_transcript_cards(
    vertices: &mut Vec<Vertex>,
    app: &SingleSessionApp,
    size: PhysicalSize<u32>,
    tick: u64,
    smooth_scroll_lines: f32,
) {
    let typography = single_session_typography();
    let line_height = typography.body_size * typography.body_line_height;
    let viewport = single_session_body_viewport_for_tick(app, size, tick, smooth_scroll_lines);
    let width = (size.width as f32 - PANEL_TITLE_LEFT_PADDING * 2.0 + 12.0).max(1.0);
    let body_top = single_session_body_top_for_app(app, size);
    let body_bottom = single_session_body_bottom_for_app(app, size);

    for run in single_session_transcript_card_runs(&viewport.lines) {
        let Some(color) = single_session_line_card_color(run.style) else {
            continue;
        };
        let rect = Rect {
            x: PANEL_TITLE_LEFT_PADDING - 6.0,
            y: body_top + viewport.top_offset_pixels + run.line as f32 * line_height + 3.0,
            width,
            height: (run.line_count as f32 * line_height - 6.0).max(1.0),
        };
        let Some(rect) = clip_rect_to_vertical_bounds(rect, body_top, body_bottom) else {
            continue;
        };
        push_rounded_rect(vertices, rect, 7.0, color, size);
    }
}

pub(crate) fn push_single_session_streaming_shimmer(
    vertices: &mut Vec<Vertex>,
    app: &SingleSessionApp,
    size: PhysicalSize<u32>,
    tick: u64,
    smooth_scroll_lines: f32,
) {
    let Some(shimmer) =
        single_session_streaming_shimmer_with_scroll(app, size, tick, smooth_scroll_lines)
    else {
        return;
    };

    push_rect(
        vertices,
        shimmer.soft_rect,
        STREAMING_SHIMMER_SOFT_COLOR,
        size,
    );
    push_rect(
        vertices,
        shimmer.core_rect,
        STREAMING_SHIMMER_CORE_COLOR,
        size,
    );
}

#[derive(Clone, Copy)]
pub(crate) struct SingleSessionStreamingShimmer {
    pub(crate) soft_rect: Rect,
    pub(crate) core_rect: Rect,
}

#[cfg(test)]
pub(crate) fn single_session_streaming_shimmer(
    app: &SingleSessionApp,
    size: PhysicalSize<u32>,
    tick: u64,
) -> Option<SingleSessionStreamingShimmer> {
    single_session_streaming_shimmer_with_scroll(app, size, tick, 0.0)
}

fn single_session_streaming_shimmer_with_scroll(
    app: &SingleSessionApp,
    size: PhysicalSize<u32>,
    tick: u64,
    smooth_scroll_lines: f32,
) -> Option<SingleSessionStreamingShimmer> {
    if app.streaming_response.trim().is_empty() {
        return None;
    }

    let viewport = single_session_body_viewport_for_tick(app, size, tick, smooth_scroll_lines);
    let line_index = viewport.lines.iter().rposition(is_shimmer_anchor_line)?;

    let typography = single_session_typography();
    let line_height = typography.body_size * typography.body_line_height;
    let body_top = single_session_body_top_for_app(app, size);
    let text_columns = viewport.lines[line_index].text.chars().count().max(8) as f32;
    let text_width = (text_columns * single_session_body_char_width())
        .min((size.width as f32 - PANEL_TITLE_LEFT_PADDING * 2.0).max(1.0));
    let lane_width = (text_width + 56.0).max(120.0);
    let shimmer_width = lane_width.min(180.0).max(72.0);
    let phase = (tick % 48) as f32 / 48.0;
    let travel = lane_width + shimmer_width;
    let head_x = PANEL_TITLE_LEFT_PADDING - shimmer_width + phase * travel;
    let y = body_top
        + viewport.top_offset_pixels
        + line_index as f32 * line_height
        + line_height * 0.12;
    let height = line_height * 0.76;

    let soft_rect = Rect {
        x: head_x,
        y,
        width: shimmer_width,
        height,
    };
    let core_rect = Rect {
        x: head_x + shimmer_width * 0.34,
        y,
        width: shimmer_width * 0.32,
        height,
    };
    Some(SingleSessionStreamingShimmer {
        soft_rect,
        core_rect,
    })
}

fn push_single_session_scrollbar(
    vertices: &mut Vec<Vertex>,
    app: &SingleSessionApp,
    size: PhysicalSize<u32>,
    tick: u64,
    smooth_scroll_lines: f32,
) {
    let Some(metrics) = single_session_body_scroll_metrics(app, size, tick) else {
        return;
    };
    let track_top = PANEL_BODY_TOP_PADDING + 4.0;
    let track_bottom = single_session_body_bottom(size) - 4.0;
    let track_height = (track_bottom - track_top).max(1.0);
    let x = size.width as f32 - PANEL_TITLE_LEFT_PADDING - 4.0;
    let thumb_height = (metrics.visible_lines as f32 / metrics.total_lines as f32 * track_height)
        .clamp(28.0, track_height);
    let travel = (track_height - thumb_height).max(0.0);
    let smooth_scroll_lines = (metrics.scroll_lines as f32 + smooth_scroll_lines)
        .clamp(0.0, metrics.max_scroll_lines as f32);
    let scroll_fraction = smooth_scroll_lines / metrics.max_scroll_lines.max(1) as f32;
    let thumb_y = track_top + (1.0 - scroll_fraction.clamp(0.0, 1.0)) * travel;

    push_rounded_rect(
        vertices,
        Rect {
            x,
            y: track_top,
            width: 3.0,
            height: track_height,
        },
        2.0,
        [0.040, 0.055, 0.090, 0.075],
        size,
    );
    push_rounded_rect(
        vertices,
        Rect {
            x: x - 0.5,
            y: thumb_y,
            width: 4.0,
            height: thumb_height,
        },
        2.0,
        [0.035, 0.065, 0.145, 0.34],
        size,
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SingleSessionBodyScrollMetrics {
    pub(crate) total_lines: usize,
    pub(crate) visible_lines: usize,
    pub(crate) scroll_lines: usize,
    pub(crate) max_scroll_lines: usize,
}

pub(crate) fn single_session_body_scroll_metrics(
    app: &SingleSessionApp,
    size: PhysicalSize<u32>,
    tick: u64,
) -> Option<SingleSessionBodyScrollMetrics> {
    let typography = single_session_typography();
    let line_height = typography.body_size * typography.body_line_height;
    let body_top = single_session_body_top_for_app(app, size);
    let body_bottom = single_session_body_bottom_for_app(app, size);
    let available_height = (body_bottom - body_top).max(line_height);
    let visible_lines = ((available_height / line_height).floor() as usize).max(1);
    let total_lines = app.body_styled_lines_for_tick(tick).len();
    let max_scroll_lines = total_lines.saturating_sub(visible_lines);
    (max_scroll_lines > 0).then_some(SingleSessionBodyScrollMetrics {
        total_lines,
        visible_lines,
        scroll_lines: app.body_scroll_lines.min(max_scroll_lines),
        max_scroll_lines,
    })
}

fn is_shimmer_anchor_line(line: &SingleSessionStyledLine) -> bool {
    !line.text.trim().is_empty() && is_assistant_rendered_style(line.style)
}

fn is_assistant_rendered_style(style: SingleSessionLineStyle) -> bool {
    matches!(
        style,
        SingleSessionLineStyle::Assistant
            | SingleSessionLineStyle::AssistantHeading
            | SingleSessionLineStyle::AssistantQuote
            | SingleSessionLineStyle::AssistantTable
            | SingleSessionLineStyle::AssistantLink
            | SingleSessionLineStyle::Code
    )
}

pub(crate) fn single_session_transcript_card_runs(
    lines: &[SingleSessionStyledLine],
) -> Vec<SingleSessionTranscriptCardRun> {
    let mut runs = Vec::new();
    let mut current: Option<SingleSessionTranscriptCardRun> = None;

    for (line, styled_line) in lines.iter().enumerate() {
        if single_session_line_card_color(styled_line.style).is_none() {
            if let Some(run) = current.take() {
                runs.push(run);
            }
            continue;
        }

        match &mut current {
            Some(run) if run.style == styled_line.style && run.line + run.line_count == line => {
                run.line_count += 1;
            }
            Some(run) => {
                runs.push(*run);
                current = Some(SingleSessionTranscriptCardRun {
                    line,
                    line_count: 1,
                    style: styled_line.style,
                });
            }
            None => {
                current = Some(SingleSessionTranscriptCardRun {
                    line,
                    line_count: 1,
                    style: styled_line.style,
                });
            }
        }
    }

    if let Some(run) = current {
        runs.push(run);
    }
    runs
}

fn single_session_line_card_color(style: SingleSessionLineStyle) -> Option<[f32; 4]> {
    match style {
        SingleSessionLineStyle::Code => Some(CODE_BLOCK_BACKGROUND_COLOR),
        SingleSessionLineStyle::AssistantQuote => Some(QUOTE_CARD_BACKGROUND_COLOR),
        SingleSessionLineStyle::AssistantTable => Some(TABLE_CARD_BACKGROUND_COLOR),
        SingleSessionLineStyle::Tool => Some(TOOL_CARD_BACKGROUND_COLOR),
        SingleSessionLineStyle::Error => Some(ERROR_CARD_BACKGROUND_COLOR),
        SingleSessionLineStyle::OverlaySelection => Some(OVERLAY_SELECTION_BACKGROUND_COLOR),
        _ => None,
    }
}

fn push_single_session_selection(
    vertices: &mut Vec<Vertex>,
    app: &SingleSessionApp,
    size: PhysicalSize<u32>,
) {
    let typography = single_session_typography();
    let line_height = typography.body_size * typography.body_line_height;
    let char_width = single_session_body_char_width();
    let visible_lines = single_session_visible_body(app, size);
    let body_top = single_session_body_top_for_app(app, size);
    for segment in app.selection_segments(&visible_lines) {
        let selected_columns = segment
            .end_column
            .saturating_sub(segment.start_column)
            .max(1);
        push_rect(
            vertices,
            Rect {
                x: PANEL_TITLE_LEFT_PADDING - 2.0 + segment.start_column as f32 * char_width,
                y: body_top + segment.line as f32 * line_height,
                width: selected_columns as f32 * char_width + 4.0,
                height: line_height,
            },
            SELECTION_HIGHLIGHT_COLOR,
            size,
        );
    }
}

pub(crate) fn push_single_session_caret(
    vertices: &mut Vec<Vertex>,
    app: &SingleSessionApp,
    size: PhysicalSize<u32>,
    draft_buffer: Option<&Buffer>,
) {
    let caret = draft_buffer
        .and_then(|buffer| glyphon_draft_caret_position(app, buffer, size))
        .unwrap_or_else(|| approximate_draft_caret_position(app, size));

    push_rect(
        vertices,
        Rect {
            x: caret.x,
            y: caret.y,
            width: SINGLE_SESSION_CARET_WIDTH,
            height: caret.height,
        },
        SINGLE_SESSION_CARET_COLOR,
        size,
    );
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CaretPosition {
    pub(crate) x: f32,
    pub(crate) y: f32,
    height: f32,
}

pub(crate) fn glyphon_draft_caret_position(
    app: &SingleSessionApp,
    draft_buffer: &Buffer,
    size: PhysicalSize<u32>,
) -> Option<CaretPosition> {
    let typography = single_session_typography();
    let target = app.composer_cursor_line_byte_index();
    let target_line = target.0;
    let target_index = target.1;
    let mut fallback = None;

    for run in draft_buffer.layout_runs() {
        if run.line_i != target_line {
            continue;
        }
        let y = single_session_draft_top_for_app(app, size) + run.line_top;
        let height = typography.code_size * 1.12;
        if run.glyphs.is_empty() {
            return Some(CaretPosition {
                x: PANEL_TITLE_LEFT_PADDING,
                y,
                height,
            });
        }

        let first = run.glyphs.first()?;
        let last = run.glyphs.last()?;
        let mut run_position = CaretPosition {
            x: PANEL_TITLE_LEFT_PADDING + last.x + last.w,
            y,
            height,
        };
        if target_index <= first.start {
            run_position.x = PANEL_TITLE_LEFT_PADDING + first.x;
            return Some(run_position);
        }
        for glyph in run.glyphs {
            if target_index <= glyph.start {
                run_position.x = PANEL_TITLE_LEFT_PADDING + glyph.x;
                return Some(run_position);
            }
            if target_index <= glyph.end {
                run_position.x = PANEL_TITLE_LEFT_PADDING + glyph.x + glyph.w;
                return Some(run_position);
            }
        }
        if target_index >= first.start && target_index >= last.end {
            fallback = Some(run_position);
        }
    }

    fallback
}

fn approximate_draft_caret_position(
    app: &SingleSessionApp,
    size: PhysicalSize<u32>,
) -> CaretPosition {
    let typography = single_session_typography();
    let line_height = typography.code_size * typography.code_line_height;
    let draft_top = single_session_draft_top_for_app(app, size);
    let (cursor_line, cursor_column) = app.draft_cursor_line_col();
    let char_width = typography.code_size * 0.58;
    let prompt_column = if cursor_line == 0 {
        app.composer_prompt().chars().count()
    } else {
        0
    };
    let x = PANEL_TITLE_LEFT_PADDING
        + ((prompt_column + cursor_column) as f32 * char_width)
            .min((size.width as f32 - PANEL_TITLE_LEFT_PADDING * 2.0).max(0.0));
    let y = draft_top + cursor_line as f32 * line_height;
    CaretPosition {
        x,
        y,
        height: typography.code_size * 1.12,
    }
}

pub(crate) fn single_session_draft_top(size: PhysicalSize<u32>) -> f32 {
    (size.height as f32 - SINGLE_SESSION_DRAFT_TOP_OFFSET).max(112.0)
}

pub(crate) fn single_session_draft_top_for_app(
    app: &SingleSessionApp,
    size: PhysicalSize<u32>,
) -> f32 {
    let _ = app;
    single_session_draft_top(size)
}

pub(crate) fn single_session_draft_top_for_fresh_state(
    size: PhysicalSize<u32>,
    fresh_welcome_visible: bool,
) -> f32 {
    let _ = fresh_welcome_visible;
    single_session_draft_top(size)
}

#[cfg(test)]
pub(crate) fn single_session_text_buffers(
    app: &SingleSessionApp,
    size: PhysicalSize<u32>,
    font_system: &mut FontSystem,
) -> Vec<Buffer> {
    let key = single_session_text_key(app, size);
    single_session_text_buffers_from_key(&key, size, font_system)
}

#[cfg(test)]
pub(crate) fn single_session_text_key(
    app: &SingleSessionApp,
    size: PhysicalSize<u32>,
) -> SingleSessionTextKey {
    single_session_text_key_for_tick(app, size, 0)
}

#[cfg(test)]
pub(crate) fn single_session_text_key_for_tick(
    app: &SingleSessionApp,
    size: PhysicalSize<u32>,
    tick: u64,
) -> SingleSessionTextKey {
    single_session_text_key_for_tick_with_scroll(app, size, tick, 0.0)
}

pub(crate) fn single_session_text_key_for_tick_with_scroll(
    app: &SingleSessionApp,
    size: PhysicalSize<u32>,
    tick: u64,
    smooth_scroll_lines: f32,
) -> SingleSessionTextKey {
    let welcome_handoff_visible = false;
    let welcome_chrome_visible = false;
    let welcome_input_visible = true;
    let body = single_session_body_viewport_for_tick(app, size, tick, smooth_scroll_lines).lines;
    let welcome_hero = String::new();
    let welcome_hint = Vec::new();
    SingleSessionTextKey {
        size: (size.width, size.height),
        fresh_welcome_visible: welcome_chrome_visible,
        title: if welcome_chrome_visible {
            String::new()
        } else {
            app.header_title()
        },
        version: if welcome_chrome_visible {
            if welcome_input_visible {
                fresh_welcome_version_label()
            } else {
                String::new()
            }
        } else {
            desktop_header_version_label()
        },
        welcome_hero,
        welcome_hint,
        activity_active: app.has_activity_indicator(),
        welcome_handoff_visible,
        body,
        draft: if welcome_input_visible {
            visualize_composer_whitespace(&app.composer_text())
        } else {
            String::new()
        },
        status: if welcome_chrome_visible {
            String::new()
        } else {
            app.composer_status_line_for_tick(tick)
        },
    }
}

pub(crate) fn single_session_text_buffers_from_key(
    key: &SingleSessionTextKey,
    size: PhysicalSize<u32>,
    font_system: &mut FontSystem,
) -> Vec<Buffer> {
    let typography = single_session_typography();
    let content_width = (size.width as f32 - PANEL_TITLE_LEFT_PADDING * 2.0).max(1.0);

    let draft_top = single_session_draft_top_for_fresh_state(size, key.fresh_welcome_visible);
    let prompt_height = (size.height as f32 - draft_top - SINGLE_SESSION_STATUS_GAP - 18.0)
        .max(typography.code_size * typography.code_line_height * 2.0);
    let hero_font_size = welcome_hero_font_size(&key.welcome_hero, size);
    let version_font_size = if key.fresh_welcome_visible {
        fresh_welcome_version_font_size()
    } else {
        typography.meta_size
    };

    vec![
        single_session_text_buffer(
            font_system,
            &key.title,
            typography.title_size,
            typography.title_size * typography.meta_line_height,
            content_width,
            48.0,
        ),
        single_session_styled_text_buffer(
            font_system,
            &key.body,
            typography.body_size,
            typography.body_size * typography.body_line_height,
            content_width,
            (size.height as f32 - 150.0).max(1.0),
        ),
        single_session_text_buffer(
            font_system,
            &key.draft,
            typography.code_size,
            typography.code_size * typography.code_line_height,
            content_width,
            prompt_height,
        ),
        single_session_text_buffer(
            font_system,
            &key.status,
            typography.meta_size,
            typography.meta_size * typography.meta_line_height,
            content_width,
            28.0,
        ),
        single_session_text_buffer(
            font_system,
            &key.version,
            version_font_size,
            version_font_size * typography.meta_line_height,
            content_width,
            24.0,
        ),
        single_session_nowrap_text_buffer(
            font_system,
            key.welcome_hero.trim(),
            hero_font_size,
            hero_font_size * 1.08,
            size.width as f32 * 0.64,
            hero_font_size * 1.25,
        ),
    ]
}

fn welcome_hero_font_size(hero: &str, size: PhysicalSize<u32>) -> f32 {
    let width = size.width as f32;
    let height = size.height as f32;
    let chars = hero.trim().chars().count().max(1) as f32;
    let target_width = width * 0.50;
    (target_width / (chars * 0.56)).clamp(42.0, height * 0.18)
}

pub(crate) fn single_session_visible_body(
    app: &SingleSessionApp,
    size: PhysicalSize<u32>,
) -> Vec<String> {
    single_session_visible_styled_body(app, size)
        .into_iter()
        .map(|line| line.text)
        .collect()
}

pub(crate) fn single_session_visible_styled_body(
    app: &SingleSessionApp,
    size: PhysicalSize<u32>,
) -> Vec<SingleSessionStyledLine> {
    single_session_visible_styled_body_for_tick(app, size, 0)
}

pub(crate) fn single_session_visible_styled_body_for_tick(
    app: &SingleSessionApp,
    size: PhysicalSize<u32>,
    tick: u64,
) -> Vec<SingleSessionStyledLine> {
    single_session_body_viewport_for_tick(app, size, tick, 0.0).lines
}

#[derive(Clone, Debug)]
pub(crate) struct SingleSessionBodyViewport {
    pub(crate) lines: Vec<SingleSessionStyledLine>,
    pub(crate) top_offset_pixels: f32,
}

pub(crate) fn single_session_body_viewport_for_tick(
    app: &SingleSessionApp,
    size: PhysicalSize<u32>,
    tick: u64,
    smooth_scroll_lines: f32,
) -> SingleSessionBodyViewport {
    let typography = single_session_typography();
    let line_height = typography.body_size * typography.body_line_height;
    let body_top = single_session_body_top_for_app(app, size);
    let body_bottom = single_session_body_bottom_for_app(app, size);
    let available_height = (body_bottom - body_top).max(line_height);
    let visible_lines = ((available_height / line_height).floor() as usize).max(1);
    let mut lines = app.body_styled_lines_for_tick(tick);
    if app.is_fresh_welcome_visible() {
        lines = center_fresh_startup_lines(lines, size, visible_lines);
    }
    if lines.len() <= visible_lines {
        return SingleSessionBodyViewport {
            lines,
            top_offset_pixels: 0.0,
        };
    }

    let max_scroll = lines.len().saturating_sub(visible_lines);
    let scroll = (app.body_scroll_lines as f32 + smooth_scroll_lines).clamp(0.0, max_scroll as f32);
    let bottom_line = lines.len() as f32 - scroll;
    let top_line = bottom_line - visible_lines as f32;
    let start = top_line.floor().max(0.0) as usize;
    let end = bottom_line.ceil().min(lines.len() as f32) as usize;
    let top_offset_pixels = (start as f32 - top_line) * line_height;
    SingleSessionBodyViewport {
        lines: lines[start..end.max(start)].to_vec(),
        top_offset_pixels,
    }
}

fn center_fresh_startup_lines(
    lines: Vec<SingleSessionStyledLine>,
    size: PhysicalSize<u32>,
    visible_lines: usize,
) -> Vec<SingleSessionStyledLine> {
    let top_padding = visible_lines.saturating_sub(lines.len()) / 3;
    let indent = fresh_startup_indent(size);
    let mut centered = Vec::with_capacity(top_padding + lines.len());
    centered.extend((0..top_padding).map(|_| SingleSessionStyledLine {
        text: String::new(),
        style: SingleSessionLineStyle::Blank,
    }));
    centered.extend(lines.into_iter().map(|mut line| {
        if !line.text.is_empty() {
            line.text = format!("{indent}{}", line.text);
        }
        line
    }));
    centered
}

fn fresh_startup_indent(size: PhysicalSize<u32>) -> String {
    let typography = single_session_typography();
    let approximate_char_width = typography.body_size * 0.58;
    let content_width = (size.width as f32 - PANEL_TITLE_LEFT_PADDING * 2.0).max(1.0);
    let columns = (content_width / approximate_char_width).floor().max(0.0) as usize;
    let target_text_width = 36;
    " ".repeat(columns.saturating_sub(target_text_width) / 2)
}

pub(crate) fn single_session_body_line_at_y(size: PhysicalSize<u32>, y: f32) -> Option<usize> {
    let typography = single_session_typography();
    let line_height = typography.body_size * typography.body_line_height;
    if y < PANEL_BODY_TOP_PADDING || y >= single_session_body_bottom(size) {
        return None;
    }
    Some(((y - PANEL_BODY_TOP_PADDING) / line_height).floor() as usize)
}

pub(crate) fn single_session_body_point_at_position(
    size: PhysicalSize<u32>,
    x: f32,
    y: f32,
    lines: &[String],
) -> Option<SelectionPoint> {
    let line = single_session_body_line_at_y(size, y)?;
    let text = lines.get(line)?;
    Some(SelectionPoint {
        line,
        column: single_session_body_column_at_x(x, text),
    })
}

pub(crate) fn single_session_body_column_at_x(x: f32, line: &str) -> usize {
    let char_count = line.chars().count();
    if x <= PANEL_TITLE_LEFT_PADDING {
        return 0;
    }
    let raw = ((x - PANEL_TITLE_LEFT_PADDING) / single_session_body_char_width()).round();
    raw.max(0.0).min(char_count as f32) as usize
}

pub(crate) fn single_session_body_char_width() -> f32 {
    let typography = single_session_typography();
    typography.body_size * 0.58
}

fn single_session_body_top_for_app(app: &SingleSessionApp, size: PhysicalSize<u32>) -> f32 {
    let _ = (app, size);
    PANEL_BODY_TOP_PADDING
}

fn single_session_body_bottom_for_app(app: &SingleSessionApp, size: PhysicalSize<u32>) -> f32 {
    let _ = app;
    single_session_body_bottom(size)
}

pub(crate) fn single_session_body_bottom(size: PhysicalSize<u32>) -> f32 {
    single_session_draft_top(size) - SINGLE_SESSION_STATUS_GAP - 12.0
}

fn clip_rect_to_vertical_bounds(rect: Rect, top: f32, bottom: f32) -> Option<Rect> {
    let clipped_y = rect.y.max(top);
    let clipped_bottom = (rect.y + rect.height).min(bottom);
    (clipped_bottom > clipped_y).then_some(Rect {
        y: clipped_y,
        height: clipped_bottom - clipped_y,
        ..rect
    })
}

fn single_session_text_buffer(
    font_system: &mut FontSystem,
    text: &str,
    font_size: f32,
    line_height: f32,
    width: f32,
    height: f32,
) -> Buffer {
    let mut buffer = Buffer::new(font_system, Metrics::new(font_size, line_height));
    buffer.set_size(font_system, width, height);
    buffer.set_wrap(font_system, Wrap::Word);
    buffer.set_text(
        font_system,
        text,
        Attrs::new().family(Family::Name(SINGLE_SESSION_FONT_FAMILY)),
        Shaping::Basic,
    );
    buffer.shape_until_scroll(font_system);
    buffer
}

fn single_session_nowrap_text_buffer(
    font_system: &mut FontSystem,
    text: &str,
    font_size: f32,
    line_height: f32,
    width: f32,
    height: f32,
) -> Buffer {
    let mut buffer = Buffer::new(font_system, Metrics::new(font_size, line_height));
    buffer.set_size(font_system, width, height);
    buffer.set_wrap(font_system, Wrap::None);
    buffer.set_text(
        font_system,
        text,
        Attrs::new().family(Family::Name(SINGLE_SESSION_FONT_FAMILY)),
        Shaping::Basic,
    );
    buffer.shape_until_scroll(font_system);
    buffer
}

fn single_session_styled_text_buffer(
    font_system: &mut FontSystem,
    lines: &[SingleSessionStyledLine],
    font_size: f32,
    line_height: f32,
    width: f32,
    height: f32,
) -> Buffer {
    let mut buffer = Buffer::new(font_system, Metrics::new(font_size, line_height));
    buffer.set_size(font_system, width, height);
    let segments = single_session_styled_text_segments(lines);
    buffer.set_rich_text(
        font_system,
        segments
            .iter()
            .map(|(text, color)| (text.as_str(), single_session_color_attrs(*color))),
        Shaping::Basic,
    );
    buffer.shape_until_scroll(font_system);
    buffer
}

pub(crate) fn single_session_styled_text_segments(
    lines: &[SingleSessionStyledLine],
) -> Vec<(String, TextColor)> {
    let mut segments = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if !line.text.is_empty() {
            if line.style == SingleSessionLineStyle::User {
                push_user_prompt_segments(&mut segments, &line.text);
            } else {
                segments.push((line.text.clone(), single_session_line_color(line.style)));
            }
        }
        if index + 1 < lines.len() {
            segments.push((
                "\n".to_string(),
                single_session_line_color(SingleSessionLineStyle::Blank),
            ));
        }
    }
    if segments.is_empty() {
        segments.push((
            String::new(),
            single_session_line_color(SingleSessionLineStyle::Blank),
        ));
    }
    segments
}

fn push_user_prompt_segments(segments: &mut Vec<(String, TextColor)>, line: &str) {
    let Some((number, text)) = line.split_once("  ") else {
        segments.push((
            line.to_string(),
            single_session_line_color(SingleSessionLineStyle::User),
        ));
        return;
    };
    let Ok(turn) = number.parse::<usize>() else {
        segments.push((
            line.to_string(),
            single_session_line_color(SingleSessionLineStyle::User),
        ));
        return;
    };

    segments.push((number.to_string(), user_prompt_number_color(turn)));
    segments.push(("› ".to_string(), text_color(USER_PROMPT_ACCENT_COLOR)));
    segments.push((
        text.to_string(),
        single_session_line_color(SingleSessionLineStyle::User),
    ));
}

fn single_session_color_attrs(color: TextColor) -> Attrs<'static> {
    Attrs::new()
        .family(Family::Name(SINGLE_SESSION_FONT_FAMILY))
        .color(color)
}

pub(crate) fn user_prompt_number_color(turn: usize) -> TextColor {
    let index = turn.saturating_sub(1) % USER_PROMPT_NUMBER_COLORS.len();
    text_color(USER_PROMPT_NUMBER_COLORS[index])
}

pub(crate) fn single_session_line_color(style: SingleSessionLineStyle) -> TextColor {
    text_color(single_session_line_rgba(style))
}

fn single_session_line_rgba(style: SingleSessionLineStyle) -> [f32; 4] {
    match style {
        SingleSessionLineStyle::Assistant => ASSISTANT_TEXT_COLOR,
        SingleSessionLineStyle::AssistantHeading => ASSISTANT_HEADING_TEXT_COLOR,
        SingleSessionLineStyle::AssistantQuote => ASSISTANT_QUOTE_TEXT_COLOR,
        SingleSessionLineStyle::AssistantTable => ASSISTANT_TABLE_TEXT_COLOR,
        SingleSessionLineStyle::AssistantLink => ASSISTANT_LINK_TEXT_COLOR,
        SingleSessionLineStyle::Code => CODE_TEXT_COLOR,
        SingleSessionLineStyle::User => USER_TEXT_COLOR,
        SingleSessionLineStyle::UserContinuation => USER_CONTINUATION_TEXT_COLOR,
        SingleSessionLineStyle::Tool => TOOL_TEXT_COLOR,
        SingleSessionLineStyle::Meta | SingleSessionLineStyle::Blank => META_TEXT_COLOR,
        SingleSessionLineStyle::Status => STATUS_TEXT_ACCENT_COLOR,
        SingleSessionLineStyle::Error => ERROR_TEXT_COLOR,
        SingleSessionLineStyle::OverlayTitle => PANEL_TITLE_COLOR,
        SingleSessionLineStyle::Overlay => OVERLAY_TEXT_COLOR,
        SingleSessionLineStyle::OverlaySelection => OVERLAY_SELECTION_TEXT_COLOR,
    }
}

pub(crate) fn single_session_text_areas(
    buffers: &[Buffer],
    size: PhysicalSize<u32>,
) -> Vec<TextArea<'_>> {
    single_session_text_areas_for_fresh_state(buffers, size, false)
}

#[cfg(test)]
pub(crate) fn single_session_text_areas_for_app<'a>(
    app: &SingleSessionApp,
    buffers: &'a [Buffer],
    size: PhysicalSize<u32>,
) -> Vec<TextArea<'a>> {
    single_session_text_areas_for_app_with_scroll(app, buffers, size, 0, 0.0)
}

pub(crate) fn single_session_text_areas_for_app_with_scroll<'a>(
    app: &SingleSessionApp,
    buffers: &'a [Buffer],
    size: PhysicalSize<u32>,
    tick: u64,
    smooth_scroll_lines: f32,
) -> Vec<TextArea<'a>> {
    let body_top_offset_pixels =
        single_session_body_viewport_for_tick(app, size, tick, smooth_scroll_lines)
            .top_offset_pixels;
    single_session_text_areas_for_state(buffers, size, false, false, body_top_offset_pixels)
}

pub(crate) fn single_session_text_areas_for_fresh_state(
    buffers: &[Buffer],
    size: PhysicalSize<u32>,
    fresh_welcome_visible: bool,
) -> Vec<TextArea<'_>> {
    single_session_text_areas_for_state(buffers, size, fresh_welcome_visible, false, 0.0)
}

pub(crate) fn single_session_text_areas_for_state(
    buffers: &[Buffer],
    size: PhysicalSize<u32>,
    welcome_chrome_visible: bool,
    welcome_handoff_visible: bool,
    body_top_offset_pixels: f32,
) -> Vec<TextArea<'_>> {
    if buffers.len() < 5 {
        return Vec::new();
    }

    let left = PANEL_TITLE_LEFT_PADDING;
    let right = size.width.saturating_sub(PANEL_TITLE_LEFT_PADDING as u32) as i32;
    let bottom = size.height.saturating_sub(PANEL_TITLE_TOP_PADDING as u32) as i32;
    let draft_top = single_session_draft_top_for_fresh_state(size, welcome_chrome_visible);
    let body_top = if welcome_handoff_visible {
        draft_top
    } else {
        PANEL_BODY_TOP_PADDING
    };
    let body_bottom = if welcome_handoff_visible {
        bottom
    } else {
        single_session_body_bottom(size) as i32
    };
    let version_label = fresh_welcome_version_label();
    let version_font_size = fresh_welcome_version_font_size();
    let version_left = if welcome_chrome_visible {
        fresh_welcome_version_left(&version_label, size, version_font_size)
    } else {
        (size.width as f32 * 0.42).max(left + 220.0)
    };
    let version_top = if welcome_chrome_visible {
        fresh_welcome_version_top(size)
    } else {
        PANEL_TITLE_TOP_PADDING + 3.0
    };
    let version_bounds_top = if welcome_chrome_visible {
        version_top as i32
    } else {
        0
    };
    let version_bounds_bottom = if welcome_chrome_visible {
        (version_top + version_font_size * 1.4) as i32
    } else {
        64
    };

    let mut areas = vec![
        TextArea {
            buffer: &buffers[0],
            left,
            top: PANEL_TITLE_TOP_PADDING,
            scale: 1.0,
            bounds: TextBounds {
                left: 0,
                top: 0,
                right,
                bottom: 64,
            },
            default_color: text_color(PANEL_TITLE_COLOR),
        },
        TextArea {
            buffer: &buffers[4],
            left: version_left,
            top: version_top,
            scale: 1.0,
            bounds: TextBounds {
                left: 0,
                top: version_bounds_top,
                right,
                bottom: version_bounds_bottom,
            },
            default_color: text_color(META_TEXT_COLOR),
        },
        TextArea {
            buffer: &buffers[1],
            left,
            top: body_top + body_top_offset_pixels,
            scale: 1.0,
            bounds: TextBounds {
                left: 0,
                top: body_top as i32,
                right,
                bottom: body_bottom,
            },
            default_color: text_color(ASSISTANT_TEXT_COLOR),
        },
    ];

    if !welcome_chrome_visible {
        areas.push(TextArea {
            buffer: &buffers[3],
            left,
            top: draft_top - SINGLE_SESSION_STATUS_GAP,
            scale: 1.0,
            bounds: TextBounds {
                left: 0,
                top: (draft_top - SINGLE_SESSION_STATUS_GAP) as i32,
                right,
                bottom: draft_top as i32,
            },
            default_color: text_color(PANEL_SECTION_COLOR),
        });
    }

    if !welcome_handoff_visible {
        areas.push(TextArea {
            buffer: &buffers[2],
            left,
            top: draft_top,
            scale: 1.0,
            bounds: TextBounds {
                left: 0,
                top: draft_top as i32,
                right,
                bottom,
            },
            default_color: text_color(PANEL_SECTION_COLOR),
        });
    }

    areas
}

fn visualize_composer_whitespace(text: &str) -> String {
    text.to_string()
}

pub(crate) fn desktop_header_version_label() -> String {
    let version = option_env!("JCODE_DESKTOP_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
    let binary = std::env::current_exe()
        .ok()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "unknown binary".to_string());
    format!("{binary} · {version}")
}

pub(crate) fn fresh_welcome_version_label() -> String {
    let version = option_env!("JCODE_PRODUCT_VERSION")
        .or(option_env!("JCODE_DESKTOP_VERSION"))
        .unwrap_or(env!("CARGO_PKG_VERSION"));
    format!("jcode {version}")
}

fn fresh_welcome_version_font_size() -> f32 {
    (single_session_typography().meta_size * 0.58).clamp(11.0, 14.0)
}

fn fresh_welcome_version_top(_size: PhysicalSize<u32>) -> f32 {
    PANEL_TITLE_TOP_PADDING + 3.0
}

fn fresh_welcome_version_left(label: &str, size: PhysicalSize<u32>, font_size: f32) -> f32 {
    let estimated_width = label.chars().count() as f32 * font_size * 0.58;
    ((size.width as f32 - estimated_width) * 0.5).max(PANEL_TITLE_LEFT_PADDING)
}

pub(crate) fn text_color(color: [f32; 4]) -> TextColor {
    TextColor::rgba(
        (color[0].clamp(0.0, 1.0) * 255.0).round() as u8,
        (color[1].clamp(0.0, 1.0) * 255.0).round() as u8,
        (color[2].clamp(0.0, 1.0) * 255.0).round() as u8,
        (color[3].clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}
