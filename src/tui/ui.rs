#![allow(dead_code)]
#![cfg_attr(
    test,
    allow(
        clippy::bind_instead_of_map,
        clippy::clone_on_copy,
        clippy::collapsible_if,
        clippy::if_same_then_else,
        clippy::implicit_saturating_sub,
        clippy::items_after_test_module,
        clippy::large_enum_variant,
        clippy::let_and_return,
        clippy::manual_abs_diff,
        clippy::manual_div_ceil,
        clippy::manual_find,
        clippy::manual_is_multiple_of,
        clippy::manual_pattern_char_comparison,
        clippy::manual_repeat_n,
        clippy::manual_strip,
        clippy::map_entry,
        clippy::missing_const_for_thread_local,
        clippy::needless_borrow,
        clippy::needless_borrows_for_generic_args,
        clippy::needless_lifetimes,
        clippy::needless_range_loop,
        clippy::needless_return,
        clippy::question_mark,
        clippy::redundant_closure,
        clippy::too_many_arguments,
        clippy::type_complexity,
        clippy::unnecessary_cast,
        clippy::unnecessary_lazy_evaluations,
        clippy::unnecessary_map_or,
        clippy::unwrap_or_default,
        clippy::while_let_loop
    )
)]

use super::info_widget;
use super::markdown;
use super::ui_diff::{
    DiffLineKind, ParsedDiffLine, collect_diff_lines, diff_add_color, diff_change_counts_for_tool,
    diff_del_color, generate_diff_lines_from_tool_input, tint_span_with_diff_color,
};
use super::visual_debug::{
    self, FrameCaptureBuilder, ImageRegionCapture, InfoWidgetCapture, MarginsCapture,
    MessageCapture, RenderTimingCapture,
};
use super::{DisplayMessage, ProcessingStatus, TuiState, is_unexpected_cache_miss};
use crate::message::ToolCall;
use ratatui::{prelude::*, widgets::Paragraph};
use regex::Regex;
#[cfg(test)]
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque, hash_map::DefaultHasher};
use std::hash::Hash;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

#[path = "ui_animations.rs"]
mod animations;
#[path = "ui_box.rs"]
mod box_utils;
#[path = "ui_changelog.rs"]
mod changelog;
#[path = "ui_debug_capture.rs"]
mod debug_capture;
#[path = "ui_diagram_pane.rs"]
mod diagram_pane;
#[path = "ui_file_diff.rs"]
mod file_diff_ui;
#[path = "ui_header.rs"]
mod header;
#[path = "ui_inline_interactive.rs"]
mod inline_interactive_ui;
#[path = "ui_inline.rs"]
mod inline_ui;
#[path = "ui_input.rs"]
pub(crate) mod input_ui;
#[path = "ui_memory.rs"]
mod memory_ui;
#[path = "ui_messages.rs"]
mod messages;
#[path = "ui_overlays.rs"]
mod overlays;
#[path = "ui_pinned.rs"]
mod pinned_ui;
#[path = "ui_prepare.rs"]
mod prepare;
#[path = "ui_tools.rs"]
pub(crate) mod tools_ui;
#[path = "ui_viewport.rs"]
mod viewport;

#[cfg(test)]
use box_utils::truncate_line_to_width;
use box_utils::{line_plain_text, render_rounded_box, truncate_line_with_ellipsis_to_width};
use changelog::get_grouped_changelog;
#[cfg(test)]
use changelog::{ChangelogEntry, group_changelog_entries, parse_changelog_from};
use debug_capture::{
    build_info_widget_summary, capture_widget_placements, rect_within_bounds, rects_overlap,
};
#[cfg(test)]
use diagram_pane::{
    div_ceil_u32, estimate_pinned_diagram_pane_width_with_font, is_diagram_poor_fit,
    vcenter_fitted_image_with_font,
};
use diagram_pane::{
    draw_pinned_diagram, estimate_pinned_diagram_pane_height, estimate_pinned_diagram_pane_width,
};
use file_diff_ui::active_file_diff_context;
use file_diff_ui::draw_file_diff_view;
#[cfg(test)]
use file_diff_ui::{
    FileDiffCacheKey, FileDiffViewCacheEntry, file_content_signature, file_diff_cache,
};
pub(crate) use header::capitalize;
use inline_ui::{draw_inline_ui, inline_ui_height};
#[cfg(test)]
use memory_ui::{
    MemoryTileItem, choose_memory_tile_span, parse_memory_display_entries, plan_memory_tile,
};
use memory_ui::{group_into_tiles, render_memory_tiles, split_by_display_width};
use messages::get_cached_message_lines;
pub(crate) use messages::{
    render_assistant_message, render_background_task_message, render_swarm_message,
    render_system_message, render_tool_message,
};
pub use pinned_ui::SidePanelDebugStats;
pub(crate) use pinned_ui::{
    clear_side_panel_render_caches, prewarm_focused_side_panel, reset_side_panel_debug_stats,
    side_panel_debug_stats,
};
use pinned_ui::{
    collect_pinned_content_cached, draw_pinned_content_cached, draw_side_panel_markdown,
};
use tools_ui::summarize_batch_running_tools_compact;
#[cfg(test)]
use viewport::compute_visible_margins;
use viewport::draw_messages;
/// Last known max scroll value from the renderer. Updated each frame.
/// Scroll handlers use this to clamp scroll_offset and prevent overshoot.
static LAST_MAX_SCROLL: AtomicUsize = AtomicUsize::new(0);
/// Number of recovered panics while rendering the frame.
static DRAW_PANIC_COUNT: AtomicUsize = AtomicUsize::new(0);
/// Total line count in the pinned diff/content pane (set during render).
static PINNED_PANE_TOTAL_LINES: AtomicUsize = AtomicUsize::new(0);
/// Effective scroll position of the side pane after render-time clamping.
static LAST_DIFF_PANE_EFFECTIVE_SCROLL: AtomicUsize = AtomicUsize::new(0);
/// Wrapped line indices where each user prompt starts (updated each render frame).
/// Used by prompt-jump keybindings (Ctrl+5..9, Ctrl+[/]) for accurate positioning.
static LAST_USER_PROMPT_POSITIONS: OnceLock<Mutex<Vec<usize>>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static TEST_LAST_MAX_SCROLL: Cell<usize> = const { Cell::new(0) };
    static TEST_PINNED_PANE_TOTAL_LINES: Cell<usize> = const { Cell::new(0) };
    static TEST_LAST_DIFF_PANE_EFFECTIVE_SCROLL: Cell<usize> = const { Cell::new(0) };
    static TEST_LAST_USER_PROMPT_POSITIONS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
    static TEST_LAST_LAYOUT: RefCell<Option<LayoutSnapshot>> = const { RefCell::new(None) };
    static TEST_VISIBLE_COPY_TARGETS: RefCell<Vec<VisibleCopyTarget>> = RefCell::new(Vec::new());
    static TEST_PROMPT_VIEWPORT_STATE: RefCell<PromptViewportState> = RefCell::new(PromptViewportState::default());
    static TEST_COPY_VIEWPORT: RefCell<CopyViewportSnapshots> = RefCell::new(CopyViewportSnapshots::default());
}

/// Get the last known max scroll value (from the most recent render frame).
/// Returns 0 if no frame has been rendered yet.
pub fn last_max_scroll() -> usize {
    #[cfg(test)]
    {
        return TEST_LAST_MAX_SCROLL.with(Cell::get);
    }
    #[cfg(not(test))]
    {
        LAST_MAX_SCROLL.load(Ordering::Relaxed)
    }
}

/// Get the total line count from the pinned diff/content pane (set during render).
pub fn pinned_pane_total_lines() -> usize {
    #[cfg(test)]
    {
        return TEST_PINNED_PANE_TOTAL_LINES.with(Cell::get);
    }
    #[cfg(not(test))]
    {
        PINNED_PANE_TOTAL_LINES.load(Ordering::Relaxed)
    }
}

pub fn last_diff_pane_effective_scroll() -> usize {
    #[cfg(test)]
    {
        return TEST_LAST_DIFF_PANE_EFFECTIVE_SCROLL.with(Cell::get);
    }
    #[cfg(not(test))]
    {
        LAST_DIFF_PANE_EFFECTIVE_SCROLL.load(Ordering::Relaxed)
    }
}

/// Get the last known user prompt line positions (from the most recent render frame).
/// Returns positions as wrapped line indices from the top of content.
pub fn last_user_prompt_positions() -> Vec<usize> {
    #[cfg(test)]
    {
        return TEST_LAST_USER_PROMPT_POSITIONS.with(|v| v.borrow().clone());
    }
    #[cfg(not(test))]
    {
        LAST_USER_PROMPT_POSITIONS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .map(|v| v.clone())
            .unwrap_or_default()
    }
}

fn update_user_prompt_positions(positions: &[usize]) {
    #[cfg(test)]
    {
        TEST_LAST_USER_PROMPT_POSITIONS.with(|v| {
            let mut v = v.borrow_mut();
            v.clear();
            v.extend_from_slice(positions);
        });
        return;
    }
    #[cfg(not(test))]
    {
        let mutex = LAST_USER_PROMPT_POSITIONS.get_or_init(|| Mutex::new(Vec::new()));
        if let Ok(mut v) = mutex.lock() {
            v.clear();
            v.extend_from_slice(positions);
        }
    }
}

pub(crate) fn set_last_max_scroll(value: usize) {
    #[cfg(test)]
    {
        TEST_LAST_MAX_SCROLL.with(|cell| cell.set(value));
        return;
    }
    #[cfg(not(test))]
    {
        LAST_MAX_SCROLL.store(value, Ordering::Relaxed);
    }
}

pub(crate) fn set_pinned_pane_total_lines(value: usize) {
    #[cfg(test)]
    {
        TEST_PINNED_PANE_TOTAL_LINES.with(|cell| cell.set(value));
        return;
    }
    #[cfg(not(test))]
    {
        PINNED_PANE_TOTAL_LINES.store(value, Ordering::Relaxed);
    }
}

pub(crate) fn set_last_diff_pane_effective_scroll(value: usize) {
    #[cfg(test)]
    {
        TEST_LAST_DIFF_PANE_EFFECTIVE_SCROLL.with(|cell| cell.set(value));
        return;
    }
    #[cfg(not(test))]
    {
        LAST_DIFF_PANE_EFFECTIVE_SCROLL.store(value, Ordering::Relaxed);
    }
}

pub(super) fn hash_text_for_cache(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    std::hash::Hasher::finish(&hasher)
}

use super::color_support::rgb;

fn clear_area(frame: &mut Frame, area: Rect) {
    super::color_support::clear_buf(area, frame.buffer_mut());
}

pub(crate) fn left_aligned_content_inset(width: u16, centered: bool) -> u16 {
    if centered || width <= 1 { 0 } else { 1 }
}

pub(crate) fn centered_content_block_width(width: u16, max_width: usize) -> usize {
    (width as usize).min(max_width).max(1)
}

pub(crate) fn left_pad_lines_to_block_width(
    lines: &mut [Line<'static>],
    width: u16,
    block_width: usize,
) {
    let block_width = block_width.min(width as usize);
    let pad = (width as usize).saturating_sub(block_width) / 2;
    for line in lines {
        if pad > 0 {
            line.spans.insert(0, Span::raw(" ".repeat(pad)));
        }
        line.alignment = Some(ratatui::layout::Alignment::Left);
    }
}

const RIGHT_RAIL_HEADER_HEIGHT: u16 = 1;

fn right_rail_border_style(focused: bool, focus_color: Color) -> Style {
    let border_color = if focused { focus_color } else { dim_color() };
    Style::default().fg(border_color)
}

fn right_rail_inner(area: Rect) -> Rect {
    ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::LEFT)
        .inner(area)
}

fn right_rail_content_area(area: Rect) -> Option<Rect> {
    let inner = right_rail_inner(area);
    if inner.width == 0 || inner.height <= RIGHT_RAIL_HEADER_HEIGHT {
        return None;
    }

    Some(Rect {
        x: inner.x,
        y: inner.y + RIGHT_RAIL_HEADER_HEIGHT,
        width: inner.width,
        height: inner.height - RIGHT_RAIL_HEADER_HEIGHT,
    })
}

fn draw_right_rail_chrome(
    frame: &mut Frame,
    area: Rect,
    title: Line<'static>,
    border_style: Style,
) -> Option<Rect> {
    let inner = right_rail_inner(area);
    let content_area = right_rail_content_area(area)?;

    let block = ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::LEFT)
        .border_style(border_style);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(title),
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: RIGHT_RAIL_HEADER_HEIGHT,
        },
    );

    Some(content_area)
}

fn user_color() -> Color {
    rgb(138, 180, 248)
}
fn ai_color() -> Color {
    rgb(129, 199, 132)
}
fn tool_color() -> Color {
    rgb(120, 120, 120)
}
fn file_link_color() -> Color {
    rgb(180, 200, 255)
}
fn dim_color() -> Color {
    rgb(80, 80, 80)
}
fn accent_color() -> Color {
    rgb(186, 139, 255)
}
fn system_message_color() -> Color {
    rgb(255, 170, 220)
}
fn queued_color() -> Color {
    rgb(255, 193, 7)
}
fn asap_color() -> Color {
    rgb(110, 210, 255)
}
fn pending_color() -> Color {
    rgb(140, 140, 140)
}
fn user_text() -> Color {
    rgb(245, 245, 255)
}
fn user_bg() -> Color {
    rgb(35, 40, 50)
}
fn ai_text() -> Color {
    rgb(220, 220, 215)
}
fn header_icon_color() -> Color {
    rgb(120, 210, 230)
}
fn header_name_color() -> Color {
    rgb(190, 210, 235)
}
fn header_session_color() -> Color {
    rgb(255, 255, 255)
}

// Spinner frames for animated status
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const STATIC_ACTIVITY_INDICATOR: &str = "•";

pub(super) fn spinner_frame_index(elapsed: f32, fps: f32) -> usize {
    ((elapsed * fps) as usize) % SPINNER_FRAMES.len()
}

pub(super) fn spinner_frame(elapsed: f32, fps: f32) -> &'static str {
    SPINNER_FRAMES[spinner_frame_index(elapsed, fps)]
}

pub(super) fn activity_indicator_frame_index(elapsed: f32, fps: f32) -> usize {
    if crate::perf::tui_policy().enable_decorative_animations {
        spinner_frame_index(elapsed, fps)
    } else {
        0
    }
}

pub(super) fn activity_indicator(elapsed: f32, fps: f32) -> &'static str {
    if crate::perf::tui_policy().enable_decorative_animations {
        spinner_frame(elapsed, fps)
    } else {
        STATIC_ACTIVITY_INDICATOR
    }
}

// Keep the picker spacious on tall terminals without crowding the chat pane.
const MODEL_PICKER_MAX_HEIGHT: u16 = 16;
const MODEL_PICKER_MIN_MESSAGES_HEIGHT: u16 = 3;

/// Duration of the startup fade-in animation in seconds
const HEADER_ANIM_DURATION: f32 = 1.5;

/// Speed of the continuous chroma wave (lower = slower)
const CHROMA_SPEED: f32 = 0.15;

/// Convert HSL to RGB (h in 0-360, s and l in 0-1)
fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h_prime = h / 60.0;
    let x = c * (1.0 - (h_prime % 2.0 - 1.0).abs());
    let m = l - c / 2.0;

    let (r1, g1, b1) = match h_prime as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };

    (
        ((r1 + m) * 255.0).clamp(0.0, 255.0) as u8,
        ((g1 + m) * 255.0).clamp(0.0, 255.0) as u8,
        ((b1 + m) * 255.0).clamp(0.0, 255.0) as u8,
    )
}

/// Chroma color based on position and time - creates flowing rainbow wave
fn chroma_color(pos: f32, elapsed: f32, saturation: f32, lightness: f32) -> Color {
    // Hue shifts over time and varies by position
    // pos: 0.0-1.0 position in the text
    // Creates a wave that flows across the text
    let hue = ((pos * 60.0) + (elapsed * CHROMA_SPEED * 360.0)) % 360.0;
    let (r, g, b) = hsl_to_rgb(hue, saturation, lightness);
    rgb(r, g, b)
}

/// Calculate chroma color with fade-in from dim during startup
fn header_chroma_color(pos: f32, elapsed: f32) -> Color {
    let fade = ((elapsed / HEADER_ANIM_DURATION).clamp(0.0, 1.0)).powf(0.5);

    // During fade-in, transition from dim gray to full chroma
    let saturation = 0.75 * fade;
    let lightness = 0.3 + 0.35 * fade; // Start darker (0.3), end bright (0.65)

    chroma_color(pos, elapsed, saturation, lightness)
}

/// Calculate smooth animated color for the header (single color, no position)
fn header_animation_color(elapsed: f32) -> Color {
    header_chroma_color(0.5, elapsed)
}

fn header_fade_t(elapsed: f32, offset: f32) -> f32 {
    let t = ((elapsed - offset) / HEADER_ANIM_DURATION).clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

fn header_fade_color(target: Color, elapsed: f32, offset: f32) -> Color {
    blend_color(dim_color(), target, header_fade_t(elapsed, offset))
}

fn color_to_floats(c: Color, fallback: (f32, f32, f32)) -> (f32, f32, f32) {
    match c {
        Color::Rgb(r, g, b) => (r as f32, g as f32, b as f32),
        Color::Indexed(n) => {
            let (r, g, b) = super::color_support::indexed_to_rgb(n);
            (r as f32, g as f32, b as f32)
        }
        _ => fallback,
    }
}

fn blend_color(from: Color, to: Color, t: f32) -> Color {
    let (fr, fg, fb) = color_to_floats(from, (80.0, 80.0, 80.0));
    let (tr, tg, tb) = color_to_floats(to, (200.0, 200.0, 200.0));
    let r = fr + (tr - fr) * t;
    let g = fg + (tg - fg) * t;
    let b = fb + (tb - fb) * t;
    rgb(
        r.clamp(0.0, 255.0) as u8,
        g.clamp(0.0, 255.0) as u8,
        b.clamp(0.0, 255.0) as u8,
    )
}

/// Chrome-style sweep highlight across header text.
fn header_chrome_color(base: Color, pos: f32, elapsed: f32, intensity: f32) -> Color {
    let highlight_c: Color = rgb(235, 245, 255);
    let shadow_c: Color = rgb(70, 80, 95);
    const SPEED: f32 = 0.12;
    const WIDTH: f32 = 0.22;

    let center = (elapsed * SPEED) % 1.0;
    let mut dist = (pos - center).abs();
    dist = dist.min(1.0 - dist);
    let shine = (1.0 - (dist / WIDTH).clamp(0.0, 1.0)).powf(2.4);

    let micro = ((pos * 12.0 + elapsed * 2.6).sin() * 0.5 + 0.5) * 0.12;
    let shimmer = (shine * 0.9 + micro).clamp(0.0, 1.0) * intensity;

    let shadow_center = (center + 0.5) % 1.0;
    let mut shadow_dist = (pos - shadow_center).abs();
    shadow_dist = shadow_dist.min(1.0 - shadow_dist);
    let shadow_t =
        (1.0 - (shadow_dist / (WIDTH * 1.2)).clamp(0.0, 1.0)).powf(2.0) * 0.16 * intensity;

    let darkened = blend_color(base, shadow_c, shadow_t);
    blend_color(darkened, highlight_c, shimmer)
}

/// Set alignment on a line only if it doesn't already have one set.
/// This allows markdown rendering to mark code blocks as left-aligned while
/// other content inherits the default alignment (e.g., centered mode).
pub(crate) fn align_if_unset(line: Line<'static>, align: Alignment) -> Line<'static> {
    if line.alignment.is_some() {
        line
    } else {
        line.alignment(align)
    }
}

/// Extract semantic version for UI display/grouping.
fn semver() -> &'static str {
    static SEMVER: OnceLock<String> = OnceLock::new();
    SEMVER.get_or_init(|| format!("v{}", env!("JCODE_SEMVER")))
}

/// True when this process is running from the stable release binary path.
/// Only matches the explicit ~/.jcode/builds/stable/jcode path, NOT
/// ~/.local/bin/jcode launcher path (which now points to current).
fn is_running_stable_release() -> bool {
    static IS_STABLE: OnceLock<bool> = OnceLock::new();
    *IS_STABLE.get_or_init(|| {
        // Use the raw symlink target (read_link), not canonicalize, to
        // check whether we're on the stable channel link.
        let current_exe = match std::env::current_exe().ok() {
            Some(path) => path,
            None => return false,
        };

        // Check if we were launched via the stable symlink
        if let Ok(stable_path) = crate::build::stable_binary_path() {
            // Compare the symlink target (not canonical) to distinguish
            // direct stable-channel execution from launcher/current links.
            let stable_target =
                std::fs::read_link(&stable_path).unwrap_or_else(|_| stable_path.clone());
            let current_target =
                std::fs::read_link(&current_exe).unwrap_or_else(|_| current_exe.clone());
            if stable_target == current_target {
                return true;
            }
            // Also check canonical paths for when launched directly
            if let (Ok(stable_canon), Ok(current_canon)) = (
                std::fs::canonicalize(&stable_path),
                std::fs::canonicalize(&current_exe),
            ) && stable_canon == current_canon
                && !current_exe.to_string_lossy().contains("target/release")
            {
                return true;
            }
        }

        false
    })
}

/// Render context window as vertical list with smart grouping
/// Items < 5% are grouped by category (docs, msgs, etc.)
fn render_context_bar(
    info: &crate::prompt::ContextInfo,
    max_width: usize,
    context_limit: usize,
) -> Vec<Line<'static>> {
    let sys_c: Color = rgb(100, 140, 200);
    let docs_c: Color = rgb(200, 160, 100);
    let tools_c: Color = rgb(100, 200, 200);
    let msgs_c: Color = rgb(138, 180, 248);
    let tool_io_c: Color = rgb(255, 183, 77);
    let other_c: Color = rgb(150, 150, 150);
    let empty_c: Color = rgb(50, 50, 50);

    const THRESHOLD: f64 = 5.0;
    let limit = context_limit.max(1);

    // Collect raw: (icon, label, tokens, color, category)
    let mut raw: Vec<(&str, String, usize, Color, &str)> = Vec::new();

    let sys = info.system_prompt_chars / 4;
    if sys > 0 {
        raw.push(("⚙", "system".into(), sys, sys_c, "system"));
    }

    if info.has_project_agents_md {
        raw.push((
            "📋",
            "AGENTS.md".into(),
            info.project_agents_md_chars / 4,
            docs_c,
            "docs",
        ));
    }
    if info.has_global_agents_md {
        raw.push((
            "📋",
            "~/.AGENTS".into(),
            info.global_agents_md_chars / 4,
            docs_c,
            "docs",
        ));
    }

    if info.env_context_chars > 0 {
        raw.push((
            "🌍",
            "env".into(),
            info.env_context_chars / 4,
            other_c,
            "other",
        ));
    }
    if info.skills_chars > 0 {
        raw.push((
            "🔧",
            "skills".into(),
            info.skills_chars / 4,
            other_c,
            "other",
        ));
    }
    if info.selfdev_chars > 0 {
        raw.push((
            "🛠",
            "selfdev".into(),
            info.selfdev_chars / 4,
            other_c,
            "other",
        ));
    }
    if info.prompt_overlay_chars > 0 {
        raw.push((
            "🧩",
            "overlay".into(),
            info.prompt_overlay_chars / 4,
            other_c,
            "other",
        ));
    }

    if info.tool_defs_chars > 0 {
        let lbl = if info.tool_defs_count > 0 {
            format!("tools ({})", info.tool_defs_count)
        } else {
            "tools".into()
        };
        raw.push(("🔨", lbl, info.tool_defs_chars / 4, tools_c, "tools"));
    }
    if info.user_messages_chars > 0 {
        let lbl = if info.user_messages_count > 0 {
            format!("user ({})", info.user_messages_count)
        } else {
            "user".into()
        };
        raw.push(("👤", lbl, info.user_messages_chars / 4, msgs_c, "msgs"));
    }
    if info.assistant_messages_chars > 0 {
        let lbl = if info.assistant_messages_count > 0 {
            format!("assistant ({})", info.assistant_messages_count)
        } else {
            "assistant".into()
        };
        raw.push(("🤖", lbl, info.assistant_messages_chars / 4, msgs_c, "msgs"));
    }
    if info.tool_calls_chars > 0 {
        let lbl = if info.tool_calls_count > 0 {
            format!("calls ({})", info.tool_calls_count)
        } else {
            "calls".into()
        };
        raw.push(("⚡", lbl, info.tool_calls_chars / 4, tool_io_c, "tool_io"));
    }
    if info.tool_results_chars > 0 {
        let lbl = if info.tool_results_count > 0 {
            format!("results ({})", info.tool_results_count)
        } else {
            "results".into()
        };
        raw.push(("📤", lbl, info.tool_results_chars / 4, tool_io_c, "tool_io"));
    }

    // Smart grouping
    let mut final_segs: Vec<(String, String, usize, Color)> = Vec::new();
    let mut grouped: std::collections::HashMap<&str, (usize, Vec<String>)> =
        std::collections::HashMap::new();

    for (icon, label, tokens, color, cat) in &raw {
        let pct = (*tokens as f64 / limit as f64) * 100.0;
        if pct >= THRESHOLD || *cat == "system" {
            final_segs.push((icon.to_string(), label.clone(), *tokens, *color));
        } else {
            let e = grouped.entry(*cat).or_insert((0, Vec::new()));
            e.0 += tokens;
            e.1.push(label.clone());
        }
    }

    for (cat, icon, color) in [
        ("docs", "📄", docs_c),
        ("msgs", "💬", msgs_c),
        ("tools", "🔨", tools_c),
        ("tool_io", "⚡", tool_io_c),
        ("other", "📦", other_c),
    ] {
        if let Some((tokens, items)) = grouped.get(cat)
            && *tokens > 0
        {
            let lbl = if items.len() == 1 {
                items[0].clone()
            } else {
                format!("{} ({})", cat, items.len())
            };
            final_segs.push((icon.to_string(), lbl, *tokens, color));
        }
    }

    final_segs.sort_by(|a, b| b.2.cmp(&a.2));

    let mut lines: Vec<Line<'static>> = Vec::new();
    let total: usize = final_segs.iter().map(|(_, _, t, _)| *t).sum();

    // Summary bar (top)
    let total_str = if total >= 1000 {
        format!("{}k", total / 1000)
    } else {
        format!("{}", total)
    };
    let limit_str = if limit >= 1000 {
        format!("{}k", limit / 1000)
    } else {
        format!("{}", limit)
    };
    let tail = format!("{}/{}", total_str, limit_str);
    let tail_len = tail.chars().count();

    let max_bar = max_width.saturating_sub(tail_len + 3); // "[" + bar + "] " + tail
    let sum_w = 36.min(max_bar).max(10);
    let used_w = ((total as f64 / limit as f64) * sum_w as f64)
        .ceil()
        .max(if total > 0 { 1.0 } else { 0.0 })
        .min(sum_w as f64) as usize;
    let empty_w = sum_w.saturating_sub(used_w);

    let mut bar: Vec<Span<'static>> = vec![Span::styled("[", Style::default().fg(dim_color()))];
    let mut rem = used_w;
    for (_, _, t, c) in &final_segs {
        if rem == 0 || total == 0 {
            break;
        }
        let w = ((*t as f64 / total as f64) * used_w as f64)
            .round()
            .min(rem as f64) as usize;
        if w > 0 {
            bar.push(Span::styled("█".repeat(w), Style::default().fg(*c)));
            rem -= w;
        }
    }
    if rem > 0
        && let Some(last_seg) = final_segs.last()
    {
        bar.push(Span::styled(
            "█".repeat(rem),
            Style::default().fg(last_seg.3),
        ));
    }
    if empty_w > 0 {
        bar.push(Span::styled(
            "░".repeat(empty_w),
            Style::default().fg(empty_c),
        ));
    }
    bar.push(Span::styled("] ", Style::default().fg(dim_color())));
    bar.push(Span::styled(tail, Style::default().fg(dim_color())));
    lines.push(Line::from(bar));

    // Detail list with dot leaders
    let max_label_len = final_segs
        .iter()
        .map(|(_, l, _, _)| l.chars().count())
        .max()
        .unwrap_or(8);
    let label_w = max_label_len.clamp(10, 18);
    let line_w = max_width;

    for (icon, label, tokens, color) in &final_segs {
        let pct = (*tokens as f64 / limit as f64 * 100.0).round() as usize;
        let token_str = if *tokens >= 1000 {
            format!("{}k", tokens / 1000)
        } else {
            format!("{}", tokens)
        };
        let tail = format!("{}  {}%", token_str, pct);
        let label_text = format!("{} {}", icon, label);
        let label_len = label_text.chars().count();
        let pad = label_w.saturating_sub(label_len);
        let reserved = label_w + pad + tail.chars().count() + 2;
        let dots = line_w.saturating_sub(reserved).max(2);

        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(Span::styled(label_text, Style::default().fg(*color)));
        if pad > 0 {
            spans.push(Span::raw(" ".repeat(pad)));
        }
        spans.push(Span::styled(
            "·".repeat(dots),
            Style::default().fg(dim_color()),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(tail, Style::default().fg(dim_color())));
        lines.push(Line::from(spans));
    }

    lines
}

/// Calculate rainbow color for prompt index with exponential decay to gray.
/// `distance` is how many prompts back from the most recent (0 = most recent).
fn rainbow_prompt_color(distance: usize) -> Color {
    // Rainbow colors (hue progression): red -> orange -> yellow -> green -> cyan -> blue -> violet
    const RAINBOW: [(u8, u8, u8); 7] = [
        (255, 80, 80),   // Red (softened)
        (255, 160, 80),  // Orange
        (255, 230, 80),  // Yellow
        (80, 220, 100),  // Green
        (80, 200, 220),  // Cyan
        (100, 140, 255), // Blue
        (180, 100, 255), // Violet
    ];

    // Gray target (dim_color())
    const GRAY: (u8, u8, u8) = (80, 80, 80);

    // Exponential decay factor - how quickly we fade to gray
    // decay = e^(-distance * rate), rate of ~0.4 gives nice falloff
    let decay = (-0.4 * distance as f32).exp();

    // Select rainbow color based on distance (cycle through)
    let rainbow_idx = distance.min(RAINBOW.len() - 1);
    let (r, g, b) = RAINBOW[rainbow_idx];

    // Blend rainbow color with gray based on decay
    // At distance 0: 100% rainbow, as distance increases: approaches gray
    let blend = |rainbow: u8, gray: u8| -> u8 {
        (rainbow as f32 * decay + gray as f32 * (1.0 - decay)) as u8
    };

    rgb(blend(r, GRAY.0), blend(g, GRAY.1), blend(b, GRAY.2))
}

fn prompt_entry_color(base: Color, t: f32) -> Color {
    let peak = rgb(255, 230, 120);
    // Quick pulse in/out over the animation window.
    let phase = if t < 0.5 { t * 2.0 } else { (1.0 - t) * 2.0 };
    blend_color(base, peak, phase.clamp(0.0, 1.0) * 0.7)
}

fn prompt_entry_bg_color(base: Color, t: f32) -> Color {
    let spotlight = rgb(58, 66, 82);
    let ease_in = 1.0 - (1.0 - t).powi(3);
    let ease_out = (1.0 - t).powi(2);
    let phase = (ease_in * ease_out * 1.65).clamp(0.0, 1.0);
    blend_color(base, spotlight, phase * 0.85)
}

fn prompt_entry_shimmer_color(base: Color, pos: f32, t: f32) -> Color {
    let travel = (t * 1.15).clamp(0.0, 1.0);
    let width = 0.18;
    let dist = (pos - travel).abs();
    let shimmer = (1.0 - (dist / width).clamp(0.0, 1.0)).powf(2.2);
    let pulse = (1.0 - t).powf(0.55);
    let highlight = rgb(255, 248, 210);
    blend_color(base, highlight, shimmer * pulse * 0.7)
}

/// Generate an animated color that pulses between two colors
fn animated_tool_color(elapsed: f32) -> Color {
    if !crate::perf::tui_policy().enable_decorative_animations {
        return tool_color();
    }

    // Cycle period of ~1.5 seconds
    let t = (elapsed * 2.0).sin() * 0.5 + 0.5; // 0.0 to 1.0

    // Interpolate between cyan and purple
    let r = (80.0 + t * 106.0) as u8; // 80 -> 186
    let g = (200.0 - t * 61.0) as u8; // 200 -> 139
    let b = (220.0 + t * 35.0) as u8; // 220 -> 255

    rgb(r, g, b)
}

/// Format seconds as a human-readable age string
fn format_age(secs: i64) -> String {
    if secs < 0 {
        "future?".to_string()
    } else if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Get how long ago the binary was built and when the code was committed
/// Shows both if they differ significantly, otherwise just the build time
fn binary_age() -> Option<String> {
    let git_date = env!("JCODE_GIT_DATE");

    let now = chrono::Utc::now();

    let build_date = crate::build::current_binary_built_at()?;
    let build_secs = now.signed_duration_since(build_date).num_seconds();

    // Parse git commit date
    let git_commit_date = chrono::DateTime::parse_from_str(git_date, "%Y-%m-%d %H:%M:%S %z")
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc));
    let git_secs = git_commit_date.map(|d| now.signed_duration_since(d).num_seconds());

    let build_age = format_age(build_secs);

    // If git date is available and differs significantly (>5 min), show both
    if let Some(git_secs) = git_secs {
        let diff = (git_secs - build_secs).abs();
        if diff > 300 {
            // More than 5 minutes difference
            let git_age = format_age(git_secs);
            return Some(format!("{}, code {}", build_age, git_age));
        }
    }

    Some(build_age)
}

/// Shorten model name for display (e.g., "claude-opus-4-5-20251101" -> "claude4.5opus")
fn shorten_model_name(model: &str) -> String {
    // Handle OpenRouter models (format: provider/model-name)
    // Keep the full identifier for display
    if model.contains('/') {
        return model.to_string();
    }
    // Handle common Claude model patterns
    if model.contains("opus") {
        if model.contains("4-5") || model.contains("4.5") {
            return "claude4.5opus".to_string();
        }
        return "claudeopus".to_string();
    }
    if model.contains("sonnet") {
        if model.contains("3-5") || model.contains("3.5") {
            return "claude3.5sonnet".to_string();
        }
        return "claudesonnet".to_string();
    }
    if model.contains("haiku") {
        return "claudehaiku".to_string();
    }
    // Handle OpenAI models (gpt-5.2-codex -> gpt5.2codex)
    if model.starts_with("gpt-5") {
        // e.g., "gpt-5.2-codex" -> "gpt5.2codex"
        return model.replace("gpt-", "gpt").replace("-", "");
    }
    if model.starts_with("gpt-4") {
        return model.replace("gpt-", "").replace("-", "");
    }
    if model.starts_with("gpt-3") {
        return "gpt3.5".to_string();
    }
    // Fallback: remove common suffixes and dashes
    model.split('-').take(3).collect::<Vec<_>>().join("")
}

/// Calculate the number of visual lines an input string will occupy
/// when wrapped to a given width, accounting for explicit newlines.
fn calculate_input_lines(input: &str, line_width: usize) -> usize {
    use unicode_width::UnicodeWidthChar;

    if line_width == 0 {
        return 1;
    }
    if input.is_empty() {
        return 1;
    }

    let mut total_lines = 0;
    for line in input.split('\n') {
        if line.is_empty() {
            total_lines += 1;
        } else {
            let display_width: usize = line.chars().map(|c| c.width().unwrap_or(0)).sum();
            total_lines += display_width.div_ceil(line_width);
        }
    }
    total_lines.max(1)
}

/// Format status line content for visual debug capture
fn format_status_for_debug(app: &dyn TuiState) -> String {
    match app.status() {
        ProcessingStatus::Idle => {
            if let Some(notice) = app.status_notice() {
                format!("Idle (notice: {})", notice)
            } else if let Some((input, output)) = app.total_session_tokens() {
                format!(
                    "Idle (session: {}k in, {}k out)",
                    input / 1000,
                    output / 1000
                )
            } else if let Some(tip) =
                info_widget::occasional_status_tip(120, app.animation_elapsed() as u64)
            {
                format!("Idle ({})", tip)
            } else {
                "Idle".to_string()
            }
        }
        ProcessingStatus::Sending => "Sending...".to_string(),
        ProcessingStatus::Connecting(ref phase) => format!("{}...", phase),
        ProcessingStatus::Thinking(_start) => {
            let elapsed = app.elapsed().map(|d| d.as_secs_f32()).unwrap_or(0.0);
            format!("Thinking... ({:.1}s)", elapsed)
        }
        ProcessingStatus::Streaming => {
            let (input, output) = app.streaming_tokens();
            format!("Streaming (↑{} ↓{})", input, output)
        }
        ProcessingStatus::RunningTool(ref name) => {
            if name == "batch"
                && let Some(progress) = app.batch_progress()
            {
                let completed = progress.completed;
                let total = progress.total;
                let mut status = format!("Running batch: {}/{} done", completed, total);
                if let Some(running) = summarize_batch_running_tools_compact(&progress.running) {
                    status.push_str(&format!(", running: {}", running));
                }
                if let Some(last) = progress.last_completed.filter(|_| completed < total) {
                    status.push_str(&format!(", last done: {}", last));
                }
                return status;
            }
            format!("Running tool: {}", name)
        }
    }
}

/// Pre-computed image region from line scanning
#[derive(Clone, Copy)]
struct ImageRegion {
    /// Absolute line index in wrapped_lines
    abs_line_idx: usize,
    /// Absolute exclusive end line of the image placeholder region.
    end_line: usize,
    /// Hash of the mermaid content (for cache lookup)
    hash: u64,
    /// Total height of the image placeholder in lines
    height: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CopyTargetKind {
    CodeBlock { language: Option<String> },
    Error,
}

impl CopyTargetKind {
    fn label(&self) -> String {
        match self {
            Self::CodeBlock { language } => language
                .as_deref()
                .filter(|lang| !lang.is_empty())
                .unwrap_or("code")
                .to_string(),
            Self::Error => "error".to_string(),
        }
    }

    fn copied_notice(&self) -> String {
        match self {
            Self::CodeBlock { language } => {
                let label = language
                    .as_deref()
                    .filter(|lang| !lang.is_empty())
                    .unwrap_or("code block");
                format!("Copied {}", label)
            }
            Self::Error => "Copied error".to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RawCopyTarget {
    pub(crate) kind: CopyTargetKind,
    pub(crate) content: String,
    pub(crate) start_raw_line: usize,
    pub(crate) end_raw_line: usize,
    pub(crate) badge_raw_line: usize,
}

#[derive(Clone, Debug)]
struct CopyTarget {
    kind: CopyTargetKind,
    content: String,
    start_line: usize,
    end_line: usize,
    badge_line: usize,
}

#[derive(Clone)]
struct PreparedMessages {
    wrapped_lines: Vec<Line<'static>>,
    wrapped_plain_lines: Arc<Vec<String>>,
    wrapped_copy_offsets: Arc<Vec<usize>>,
    raw_plain_lines: Arc<Vec<String>>,
    wrapped_line_map: Arc<Vec<WrappedLineMap>>,
    wrapped_user_indices: Vec<usize>,
    /// Wrapped line indices where a user prompt line starts
    wrapped_user_prompt_starts: Vec<usize>,
    /// Wrapped line indices where a user prompt line ends (exclusive)
    wrapped_user_prompt_ends: Vec<usize>,
    /// Flattened user prompt text in display order, used by prompt preview without
    /// scanning display_messages on every frame.
    user_prompt_texts: Vec<String>,
    /// Pre-scanned image regions (computed once, not every frame)
    image_regions: Vec<ImageRegion>,
    /// Line ranges for edit tool messages: (msg_index, start_line, end_line)
    /// Used by File diff mode to determine which edit is visible at current scroll
    edit_tool_ranges: Vec<EditToolRange>,
    copy_targets: Vec<CopyTarget>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct WrappedLineMap {
    pub(crate) raw_line: usize,
    pub(crate) start_col: usize,
    pub(crate) end_col: usize,
}

#[derive(Clone, Debug)]
struct EditToolRange {
    edit_index: usize,
    msg_index: usize,
    file_path: String,
    start_line: usize,
    end_line: usize,
}

#[derive(Clone, Debug)]
struct ActiveFileDiffContext {
    edit_index: usize,
    msg_index: usize,
    file_path: String,
    start_line: usize,
    end_line: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct VisibleCopyTarget {
    pub key: char,
    pub kind_label: String,
    pub copied_notice: String,
    pub content: String,
}

const COPY_BADGE_KEYS: [char; 12] = ['s', 'd', 'f', 'g', 'w', 'e', 'r', 't', 'x', 'c', 'v', 'b'];

static VISIBLE_COPY_TARGETS: OnceLock<Mutex<Vec<VisibleCopyTarget>>> = OnceLock::new();

fn visible_copy_targets_state() -> &'static Mutex<Vec<VisibleCopyTarget>> {
    VISIBLE_COPY_TARGETS.get_or_init(|| Mutex::new(Vec::new()))
}

fn set_visible_copy_targets(targets: Vec<VisibleCopyTarget>) {
    #[cfg(test)]
    {
        TEST_VISIBLE_COPY_TARGETS.with(|state| {
            *state.borrow_mut() = targets;
        });
        return;
    }
    #[cfg(not(test))]
    {
        let mut state = match visible_copy_targets_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *state = targets;
    }
}

pub(crate) fn visible_copy_target_for_key(key: char) -> Option<VisibleCopyTarget> {
    #[cfg(test)]
    {
        TEST_VISIBLE_COPY_TARGETS.with(|state| {
            state
                .borrow()
                .iter()
                .find(|target| target.key.eq_ignore_ascii_case(&key))
                .cloned()
        })
    }
    #[cfg(not(test))]
    {
        let state = match visible_copy_targets_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state
            .iter()
            .find(|target| target.key.eq_ignore_ascii_case(&key))
            .cloned()
    }
}

#[derive(Clone, Copy)]
struct PromptViewportAnimation {
    line_idx: usize,
    start_ms: u64,
}

#[derive(Clone, Copy, Default)]
struct PromptViewportState {
    initialized: bool,
    last_visible_start: usize,
    last_visible_end: usize,
    active: Option<PromptViewportAnimation>,
}

const PROMPT_ENTRY_ANIMATION_MS: u64 = 450;

static PROMPT_VIEWPORT_STATE: OnceLock<Mutex<PromptViewportState>> = OnceLock::new();

fn prompt_viewport_state() -> &'static Mutex<PromptViewportState> {
    PROMPT_VIEWPORT_STATE.get_or_init(|| Mutex::new(PromptViewportState::default()))
}

fn active_prompt_entry_animation(now_ms: u64) -> Option<PromptViewportAnimation> {
    #[cfg(test)]
    {
        TEST_PROMPT_VIEWPORT_STATE.with(|state| {
            let mut state = state.borrow_mut();
            if let Some(anim) = state.active {
                if now_ms.saturating_sub(anim.start_ms) <= PROMPT_ENTRY_ANIMATION_MS {
                    return Some(anim);
                }
                state.active = None;
            }
            None
        })
    }
    #[cfg(not(test))]
    {
        let mut state = match prompt_viewport_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        if let Some(anim) = state.active {
            if now_ms.saturating_sub(anim.start_ms) <= PROMPT_ENTRY_ANIMATION_MS {
                return Some(anim);
            }
            state.active = None;
        }
        None
    }
}

fn record_prompt_viewport(visible_start: usize, visible_end: usize) {
    #[cfg(test)]
    {
        TEST_PROMPT_VIEWPORT_STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.initialized = true;
            state.last_visible_start = visible_start;
            state.last_visible_end = visible_end;
            state.active = None;
        });
        return;
    }
    #[cfg(not(test))]
    {
        let mut state = match prompt_viewport_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.initialized = true;
        state.last_visible_start = visible_start;
        state.last_visible_end = visible_end;
        state.active = None;
    }
}

fn update_prompt_entry_animation(
    user_prompt_lines: &[usize],
    visible_start: usize,
    visible_end: usize,
    now_ms: u64,
) {
    #[cfg(test)]
    {
        TEST_PROMPT_VIEWPORT_STATE.with(|state| {
            let mut state = state.borrow_mut();

            if !state.initialized {
                state.initialized = true;
                state.last_visible_start = visible_start;
                state.last_visible_end = visible_end;
                return;
            }

            let prev_visible_start = state.last_visible_start;
            let prev_visible_end = state.last_visible_end;
            let viewport_changed =
                prev_visible_start != visible_start || prev_visible_end != visible_end;

            if let Some(anim) = state.active {
                let still_fresh = now_ms.saturating_sub(anim.start_ms) <= PROMPT_ENTRY_ANIMATION_MS;
                let still_visible = anim.line_idx >= visible_start && anim.line_idx < visible_end;
                if still_fresh && still_visible {
                    state.last_visible_start = visible_start;
                    state.last_visible_end = visible_end;
                    return;
                }
                if !still_fresh || !still_visible {
                    state.active = None;
                }
            }

            if viewport_changed && state.active.is_none() {
                let newly_visible = user_prompt_lines.iter().copied().find(|line| {
                    *line >= visible_start
                        && *line < visible_end
                        && (*line < prev_visible_start || *line >= prev_visible_end)
                });
                if let Some(line_idx) = newly_visible {
                    state.active = Some(PromptViewportAnimation {
                        line_idx,
                        start_ms: now_ms,
                    });
                }
            }

            state.last_visible_start = visible_start;
            state.last_visible_end = visible_end;
        });
        return;
    }
    #[cfg(not(test))]
    {
        let mut state = match prompt_viewport_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        if !state.initialized {
            state.initialized = true;
            state.last_visible_start = visible_start;
            state.last_visible_end = visible_end;
            return;
        }

        let prev_visible_start = state.last_visible_start;
        let prev_visible_end = state.last_visible_end;
        let viewport_changed =
            prev_visible_start != visible_start || prev_visible_end != visible_end;

        if let Some(anim) = state.active {
            let still_fresh = now_ms.saturating_sub(anim.start_ms) <= PROMPT_ENTRY_ANIMATION_MS;
            let still_visible = anim.line_idx >= visible_start && anim.line_idx < visible_end;
            if still_fresh && still_visible {
                state.last_visible_start = visible_start;
                state.last_visible_end = visible_end;
                return;
            }
            if !still_fresh || !still_visible {
                state.active = None;
            }
        }

        if viewport_changed && state.active.is_none() {
            let newly_visible = user_prompt_lines.iter().copied().find(|line| {
                *line >= visible_start
                    && *line < visible_end
                    && (*line < prev_visible_start || *line >= prev_visible_end)
            });
            if let Some(line_idx) = newly_visible {
                state.active = Some(PromptViewportAnimation {
                    line_idx,
                    start_ms: now_ms,
                });
            }
        }

        state.last_visible_start = visible_start;
        state.last_visible_end = visible_end;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BodyCacheKey {
    width: u16,
    diff_mode: crate::config::DiffDisplayMode,
    messages_version: u64,
    diagram_mode: crate::config::DiagramDisplayMode,
    centered: bool,
}

#[derive(Clone)]
struct BodyCacheEntry {
    key: BodyCacheKey,
    prepared: Arc<PreparedMessages>,
    msg_count: usize,
}

const BODY_CACHE_MAX_ENTRIES: usize = 8;

#[derive(Default)]
struct BodyCacheState {
    entries: VecDeque<BodyCacheEntry>,
}

impl BodyCacheState {
    fn get_exact(&mut self, key: &BodyCacheKey) -> Option<Arc<PreparedMessages>> {
        let pos = self.entries.iter().position(|entry| &entry.key == key)?;
        let entry = self.entries.remove(pos)?;
        let prepared = entry.prepared.clone();
        self.entries.push_front(entry);
        Some(prepared)
    }

    fn best_incremental_base(
        &self,
        key: &BodyCacheKey,
        msg_count: usize,
    ) -> Option<(Arc<PreparedMessages>, usize)> {
        self.entries
            .iter()
            .filter(|entry| {
                entry.msg_count > 0
                    && msg_count > entry.msg_count
                    && entry.key.width == key.width
                    && entry.key.diff_mode == key.diff_mode
                    && entry.key.diagram_mode == key.diagram_mode
                    && entry.key.centered == key.centered
            })
            .max_by_key(|entry| entry.msg_count)
            .map(|entry| (entry.prepared.clone(), entry.msg_count))
    }

    fn insert(&mut self, key: BodyCacheKey, prepared: Arc<PreparedMessages>, msg_count: usize) {
        if let Some(pos) = self.entries.iter().position(|entry| entry.key == key) {
            self.entries.remove(pos);
        }
        self.entries.push_front(BodyCacheEntry {
            key,
            prepared,
            msg_count,
        });
        while self.entries.len() > BODY_CACHE_MAX_ENTRIES {
            self.entries.pop_back();
        }
    }
}

static BODY_CACHE: OnceLock<Mutex<BodyCacheState>> = OnceLock::new();

fn body_cache() -> &'static Mutex<BodyCacheState> {
    BODY_CACHE.get_or_init(|| Mutex::new(BodyCacheState::default()))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FullPrepCacheKey {
    width: u16,
    height: u16,
    diff_mode: crate::config::DiffDisplayMode,
    messages_version: u64,
    diagram_mode: crate::config::DiagramDisplayMode,
    centered: bool,
    is_processing: bool,
    streaming_text_len: usize,
    streaming_text_hash: u64,
    batch_progress_hash: u64,
    startup_active: bool,
}

#[derive(Clone)]
struct FullPrepCacheEntry {
    key: FullPrepCacheKey,
    prepared: Arc<PreparedMessages>,
}

const FULL_PREP_CACHE_MAX_ENTRIES: usize = 4;

#[derive(Default)]
struct FullPrepCacheState {
    entries: VecDeque<FullPrepCacheEntry>,
}

impl FullPrepCacheState {
    fn get_exact(&mut self, key: &FullPrepCacheKey) -> Option<Arc<PreparedMessages>> {
        let pos = self.entries.iter().position(|entry| &entry.key == key)?;
        let entry = self.entries.remove(pos)?;
        let prepared = entry.prepared.clone();
        self.entries.push_front(entry);
        Some(prepared)
    }

    fn insert(&mut self, key: FullPrepCacheKey, prepared: Arc<PreparedMessages>) {
        if let Some(pos) = self.entries.iter().position(|entry| entry.key == key) {
            self.entries.remove(pos);
        }
        self.entries
            .push_front(FullPrepCacheEntry { key, prepared });
        while self.entries.len() > FULL_PREP_CACHE_MAX_ENTRIES {
            self.entries.pop_back();
        }
    }
}

static FULL_PREP_CACHE: OnceLock<Mutex<FullPrepCacheState>> = OnceLock::new();

fn full_prep_cache() -> &'static Mutex<FullPrepCacheState> {
    FULL_PREP_CACHE.get_or_init(|| Mutex::new(FullPrepCacheState::default()))
}

#[derive(Default)]
struct RenderProfile {
    frames: u64,
    total: Duration,
    prepare: Duration,
    draw: Duration,
    last_log: Option<Instant>,
}

static PROFILE_STATE: OnceLock<Mutex<RenderProfile>> = OnceLock::new();

fn profile_state() -> &'static Mutex<RenderProfile> {
    PROFILE_STATE.get_or_init(|| Mutex::new(RenderProfile::default()))
}

#[derive(Clone, Copy, Debug)]
pub struct LayoutSnapshot {
    pub messages_area: Rect,
    pub diagram_area: Option<Rect>,
    pub diff_pane_area: Option<Rect>,
    pub input_area: Option<Rect>,
}

static LAST_LAYOUT: OnceLock<Mutex<Option<LayoutSnapshot>>> = OnceLock::new();

fn last_layout_state() -> &'static Mutex<Option<LayoutSnapshot>> {
    LAST_LAYOUT.get_or_init(|| Mutex::new(None))
}

pub fn record_layout_snapshot(
    messages_area: Rect,
    diagram_area: Option<Rect>,
    diff_pane_area: Option<Rect>,
    input_area: Option<Rect>,
) {
    #[cfg(test)]
    {
        TEST_LAST_LAYOUT.with(|snapshot| {
            *snapshot.borrow_mut() = Some(LayoutSnapshot {
                messages_area,
                diagram_area,
                diff_pane_area,
                input_area,
            });
        });
        return;
    }
    #[cfg(not(test))]
    {
        if let Ok(mut snapshot) = last_layout_state().lock() {
            *snapshot = Some(LayoutSnapshot {
                messages_area,
                diagram_area,
                diff_pane_area,
                input_area,
            });
        }
    }
}

pub fn last_layout_snapshot() -> Option<LayoutSnapshot> {
    #[cfg(test)]
    {
        return TEST_LAST_LAYOUT.with(|snapshot| *snapshot.borrow());
    }
    #[cfg(not(test))]
    {
        last_layout_state()
            .lock()
            .ok()
            .and_then(|snapshot| *snapshot)
    }
}

#[cfg(test)]
pub(crate) fn clear_test_render_state_for_tests() {
    set_last_max_scroll(0);
    set_pinned_pane_total_lines(0);
    set_last_diff_pane_effective_scroll(0);
    update_user_prompt_positions(&[]);
    TEST_LAST_LAYOUT.with(|snapshot| {
        *snapshot.borrow_mut() = None;
    });
    set_visible_copy_targets(Vec::new());
    clear_copy_viewport_snapshot();

    TEST_PROMPT_VIEWPORT_STATE.with(|state| {
        *state.borrow_mut() = PromptViewportState::default();
    });
}

#[derive(Clone, Debug)]
struct CopyViewportSnapshot {
    pane: crate::tui::CopySelectionPane,
    wrapped_plain_lines: Arc<Vec<String>>,
    wrapped_copy_offsets: Arc<Vec<usize>>,
    raw_plain_lines: Arc<Vec<String>>,
    wrapped_line_map: Arc<Vec<WrappedLineMap>>,
    scroll: usize,
    visible_end: usize,
    content_area: Rect,
    left_margins: Vec<u16>,
}

#[derive(Clone, Debug, Default)]
struct CopyViewportSnapshots {
    chat: Option<CopyViewportSnapshot>,
    side: Option<CopyViewportSnapshot>,
}

static LAST_COPY_VIEWPORT: OnceLock<Mutex<CopyViewportSnapshots>> = OnceLock::new();
static URL_REGEX: OnceLock<Regex> = OnceLock::new();

fn copy_viewport_state() -> &'static Mutex<CopyViewportSnapshots> {
    LAST_COPY_VIEWPORT.get_or_init(|| Mutex::new(CopyViewportSnapshots::default()))
}

fn url_regex() -> &'static Regex {
    URL_REGEX.get_or_init(|| {
        Regex::new(r#"(?i)(?:https?://|mailto:|file://)[^\s<>'\"]+"#)
            .expect("URL regex should compile")
    })
}

fn copy_snapshot_slot_mut(
    snapshots: &mut CopyViewportSnapshots,
    pane: crate::tui::CopySelectionPane,
) -> &mut Option<CopyViewportSnapshot> {
    match pane {
        crate::tui::CopySelectionPane::Chat => &mut snapshots.chat,
        crate::tui::CopySelectionPane::SidePane => &mut snapshots.side,
    }
}

fn copy_snapshot_for_pane(pane: crate::tui::CopySelectionPane) -> Option<CopyViewportSnapshot> {
    #[cfg(test)]
    {
        TEST_COPY_VIEWPORT.with(|snapshots| {
            let snapshots = snapshots.borrow().clone();
            match pane {
                crate::tui::CopySelectionPane::Chat => snapshots.chat,
                crate::tui::CopySelectionPane::SidePane => snapshots.side,
            }
        })
    }
    #[cfg(not(test))]
    {
        let snapshots = copy_viewport_state().lock().ok()?.clone();
        match pane {
            crate::tui::CopySelectionPane::Chat => snapshots.chat,
            crate::tui::CopySelectionPane::SidePane => snapshots.side,
        }
    }
}

pub(crate) fn clear_copy_viewport_snapshot() {
    #[cfg(test)]
    {
        TEST_COPY_VIEWPORT.with(|state| {
            *state.borrow_mut() = CopyViewportSnapshots::default();
        });
        return;
    }
    #[cfg(not(test))]
    if let Ok(mut state) = copy_viewport_state().lock() {
        *state = CopyViewportSnapshots::default();
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "Viewport snapshot helpers carry explicit render state to avoid hidden globals in call sites"
)]
fn record_copy_pane_snapshot(
    pane: crate::tui::CopySelectionPane,
    wrapped_plain_lines: Arc<Vec<String>>,
    wrapped_copy_offsets: Arc<Vec<usize>>,
    raw_plain_lines: Arc<Vec<String>>,
    wrapped_line_map: Arc<Vec<WrappedLineMap>>,
    scroll: usize,
    visible_end: usize,
    content_area: Rect,
    left_margins: &[u16],
) {
    #[cfg(test)]
    {
        TEST_COPY_VIEWPORT.with(|state| {
            *copy_snapshot_slot_mut(&mut state.borrow_mut(), pane) = Some(CopyViewportSnapshot {
                pane,
                wrapped_plain_lines,
                wrapped_copy_offsets,
                raw_plain_lines,
                wrapped_line_map,
                scroll,
                visible_end,
                content_area,
                left_margins: left_margins.to_vec(),
            });
        });
        return;
    }
    #[cfg(not(test))]
    if let Ok(mut state) = copy_viewport_state().lock() {
        *copy_snapshot_slot_mut(&mut state, pane) = Some(CopyViewportSnapshot {
            pane,
            wrapped_plain_lines,
            wrapped_copy_offsets,
            raw_plain_lines,
            wrapped_line_map,
            scroll,
            visible_end,
            content_area,
            left_margins: left_margins.to_vec(),
        });
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "Viewport snapshot helpers carry explicit render state to avoid hidden globals in call sites"
)]
pub(crate) fn record_side_pane_snapshot_precomputed(
    wrapped_plain_lines: Arc<Vec<String>>,
    wrapped_copy_offsets: Arc<Vec<usize>>,
    raw_plain_lines: Arc<Vec<String>>,
    wrapped_line_map: Arc<Vec<WrappedLineMap>>,
    scroll: usize,
    visible_end: usize,
    content_area: Rect,
    left_margins: &[u16],
) {
    record_copy_pane_snapshot(
        crate::tui::CopySelectionPane::SidePane,
        wrapped_plain_lines,
        wrapped_copy_offsets,
        raw_plain_lines,
        wrapped_line_map,
        scroll,
        visible_end,
        content_area,
        left_margins,
    );
}

#[expect(
    clippy::too_many_arguments,
    reason = "Viewport snapshot helpers carry explicit render state to avoid hidden globals in call sites"
)]
pub(crate) fn record_copy_viewport_snapshot(
    wrapped_plain_lines: Arc<Vec<String>>,
    wrapped_copy_offsets: Arc<Vec<usize>>,
    raw_plain_lines: Arc<Vec<String>>,
    wrapped_line_map: Arc<Vec<WrappedLineMap>>,
    scroll: usize,
    visible_end: usize,
    content_area: Rect,
    left_margins: &[u16],
) {
    record_copy_pane_snapshot(
        crate::tui::CopySelectionPane::Chat,
        wrapped_plain_lines,
        wrapped_copy_offsets,
        raw_plain_lines,
        wrapped_line_map,
        scroll,
        visible_end,
        content_area,
        left_margins,
    );
}

pub(crate) fn line_left_margins_for_area(lines: &[Line<'static>], area_width: u16) -> Vec<u16> {
    lines
        .iter()
        .map(|line| {
            let used = line.width().min(area_width as usize) as u16;
            let total_margin = area_width.saturating_sub(used);
            match line.alignment.unwrap_or(Alignment::Left) {
                Alignment::Left => 0,
                Alignment::Center => total_margin / 2,
                Alignment::Right => total_margin,
            }
        })
        .collect()
}

pub(crate) fn record_side_pane_snapshot(
    wrapped_lines: &[Line<'static>],
    scroll: usize,
    visible_end: usize,
    content_area: Rect,
) {
    let left_margins = line_left_margins_for_area(wrapped_lines, content_area.width);
    let raw_plain_lines: Vec<String> = wrapped_lines.iter().map(line_plain_text).collect();
    let wrapped_line_map: Vec<WrappedLineMap> = raw_plain_lines
        .iter()
        .enumerate()
        .map(|(raw_line, text)| WrappedLineMap {
            raw_line,
            start_col: 0,
            end_col: line_display_width(text),
        })
        .collect();
    let visible_left_margins = left_margins
        .get(scroll..visible_end.min(left_margins.len()))
        .unwrap_or(&[]);
    record_side_pane_snapshot_precomputed(
        Arc::new(raw_plain_lines.clone()),
        Arc::new(vec![0; wrapped_lines.len()]),
        Arc::new(raw_plain_lines),
        Arc::new(wrapped_line_map),
        scroll,
        visible_end,
        content_area,
        visible_left_margins,
    );
}

fn line_display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

fn display_col_to_byte_offset(text: &str, display_col: usize) -> usize {
    let mut width = 0usize;
    for (idx, ch) in text.char_indices() {
        if width >= display_col {
            return idx;
        }
        let next_width =
            width.saturating_add(unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0));
        if next_width > display_col {
            return idx;
        }
        width = next_width;
    }
    text.len()
}

fn clamp_display_col(text: &str, display_col: usize) -> usize {
    display_col.min(line_display_width(text))
}

fn copy_point_from_snapshot(
    snapshot: &CopyViewportSnapshot,
    column: u16,
    row: u16,
) -> Option<crate::tui::CopySelectionPoint> {
    let area = snapshot.content_area;
    if row < area.y
        || row >= area.y.saturating_add(area.height)
        || column < area.x
        || column >= area.x.saturating_add(area.width)
    {
        return None;
    }

    let rel_row = row.saturating_sub(area.y) as usize;
    let abs_line = snapshot.scroll.saturating_add(rel_row);
    if abs_line >= snapshot.visible_end || abs_line >= snapshot.wrapped_plain_lines.len() {
        return None;
    }

    let left_margin = snapshot.left_margins.get(rel_row).copied().unwrap_or(0);
    let content_x = area.x.saturating_add(left_margin);
    let rel_col = column.saturating_sub(content_x) as usize;
    let text = &snapshot.wrapped_plain_lines[abs_line];
    let copy_start = snapshot
        .wrapped_copy_offsets
        .get(abs_line)
        .copied()
        .unwrap_or(0);
    Some(crate::tui::CopySelectionPoint {
        pane: snapshot.pane,
        abs_line,
        column: clamp_display_col(text, rel_col).max(copy_start),
    })
}

pub(crate) fn copy_point_from_screen(
    column: u16,
    row: u16,
) -> Option<crate::tui::CopySelectionPoint> {
    #[cfg(test)]
    {
        TEST_COPY_VIEWPORT.with(|snapshots| {
            let snapshots = snapshots.borrow().clone();
            snapshots
                .chat
                .as_ref()
                .and_then(|snapshot| copy_point_from_snapshot(snapshot, column, row))
                .or_else(|| {
                    snapshots
                        .side
                        .as_ref()
                        .and_then(|snapshot| copy_point_from_snapshot(snapshot, column, row))
                })
        })
    }
    #[cfg(not(test))]
    {
        let snapshots = copy_viewport_state().lock().ok()?.clone();
        snapshots
            .chat
            .as_ref()
            .and_then(|snapshot| copy_point_from_snapshot(snapshot, column, row))
            .or_else(|| {
                snapshots
                    .side
                    .as_ref()
                    .and_then(|snapshot| copy_point_from_snapshot(snapshot, column, row))
            })
    }
}

pub(crate) fn copy_viewport_point_from_screen(
    column: u16,
    row: u16,
) -> Option<crate::tui::CopySelectionPoint> {
    let point = copy_point_from_screen(column, row)?;
    (point.pane == crate::tui::CopySelectionPane::Chat).then_some(point)
}

pub(crate) fn side_pane_point_from_screen(
    column: u16,
    row: u16,
) -> Option<crate::tui::CopySelectionPoint> {
    let point = copy_point_from_screen(column, row)?;
    (point.pane == crate::tui::CopySelectionPane::SidePane).then_some(point)
}

fn copy_pane_line_text(pane: crate::tui::CopySelectionPane, abs_line: usize) -> Option<String> {
    copy_snapshot_for_pane(pane)?
        .wrapped_plain_lines
        .get(abs_line)
        .cloned()
}

fn copy_pane_line_copy_start(
    pane: crate::tui::CopySelectionPane,
    abs_line: usize,
) -> Option<usize> {
    copy_snapshot_for_pane(pane)?
        .wrapped_copy_offsets
        .get(abs_line)
        .copied()
}

pub(crate) fn copy_viewport_line_text(abs_line: usize) -> Option<String> {
    copy_pane_line_text(crate::tui::CopySelectionPane::Chat, abs_line)
}

pub(crate) fn side_pane_line_text(abs_line: usize) -> Option<String> {
    copy_pane_line_text(crate::tui::CopySelectionPane::SidePane, abs_line)
}

pub(crate) fn copy_viewport_line_copy_start(abs_line: usize) -> Option<usize> {
    copy_pane_line_copy_start(crate::tui::CopySelectionPane::Chat, abs_line)
}

pub(crate) fn side_pane_line_copy_start(abs_line: usize) -> Option<usize> {
    copy_pane_line_copy_start(crate::tui::CopySelectionPane::SidePane, abs_line)
}

fn copy_pane_line_count(pane: crate::tui::CopySelectionPane) -> Option<usize> {
    Some(copy_snapshot_for_pane(pane)?.wrapped_plain_lines.len())
}

pub(crate) fn copy_viewport_line_count() -> Option<usize> {
    copy_pane_line_count(crate::tui::CopySelectionPane::Chat)
}

pub(crate) fn side_pane_line_count() -> Option<usize> {
    copy_pane_line_count(crate::tui::CopySelectionPane::SidePane)
}

pub(crate) fn copy_viewport_visible_range() -> Option<(usize, usize)> {
    let snapshot = copy_snapshot_for_pane(crate::tui::CopySelectionPane::Chat)?;
    Some((snapshot.scroll, snapshot.visible_end))
}

pub(crate) fn side_pane_visible_range() -> Option<(usize, usize)> {
    let snapshot = copy_snapshot_for_pane(crate::tui::CopySelectionPane::SidePane)?;
    Some((snapshot.scroll, snapshot.visible_end))
}

pub(crate) fn copy_pane_first_visible_point(
    pane: crate::tui::CopySelectionPane,
) -> Option<crate::tui::CopySelectionPoint> {
    let snapshot = copy_snapshot_for_pane(pane)?;
    if snapshot.scroll >= snapshot.visible_end
        || snapshot.scroll >= snapshot.wrapped_plain_lines.len()
    {
        return None;
    }
    Some(crate::tui::CopySelectionPoint {
        pane,
        abs_line: snapshot.scroll,
        column: 0,
    })
}

pub(crate) fn copy_viewport_first_visible_point() -> Option<crate::tui::CopySelectionPoint> {
    copy_pane_first_visible_point(crate::tui::CopySelectionPane::Chat)
}

pub(crate) fn copy_selection_text(range: crate::tui::CopySelectionRange) -> Option<String> {
    if range.start.pane != range.end.pane {
        return None;
    }
    let snapshot = copy_snapshot_for_pane(range.start.pane)?;
    let (start, end) =
        if (range.start.abs_line, range.start.column) <= (range.end.abs_line, range.end.column) {
            (range.start, range.end)
        } else {
            (range.end, range.start)
        };

    if start.abs_line >= snapshot.wrapped_plain_lines.len()
        || end.abs_line >= snapshot.wrapped_plain_lines.len()
    {
        return None;
    }

    if let Some(text) = copy_selection_text_from_raw_lines(&snapshot, start, end) {
        return Some(text);
    }

    let mut out = Vec::new();
    for abs_line in start.abs_line..=end.abs_line {
        let text = &snapshot.wrapped_plain_lines[abs_line];
        let line_width = line_display_width(text);
        let copy_start = snapshot
            .wrapped_copy_offsets
            .get(abs_line)
            .copied()
            .unwrap_or(0);
        let start_col = if abs_line == start.abs_line {
            clamp_display_col(text, start.column).max(copy_start)
        } else {
            copy_start
        };
        let end_col = if abs_line == end.abs_line {
            clamp_display_col(text, end.column).max(copy_start)
        } else {
            line_width
        };

        if end_col < start_col {
            out.push(String::new());
            continue;
        }

        let start_byte = display_col_to_byte_offset(text, start_col);
        let end_byte = display_col_to_byte_offset(text, end_col);
        out.push(text[start_byte..end_byte].to_string());
    }

    Some(out.join("\n"))
}

#[derive(Clone, Copy, Debug)]
struct RawSelectionPoint {
    raw_line: usize,
    column: usize,
}

fn copy_selection_text_from_raw_lines(
    snapshot: &CopyViewportSnapshot,
    start: crate::tui::CopySelectionPoint,
    end: crate::tui::CopySelectionPoint,
) -> Option<String> {
    if snapshot.raw_plain_lines.is_empty() || snapshot.wrapped_line_map.is_empty() {
        return None;
    }

    let start = raw_selection_point(snapshot, start)?;
    let end = raw_selection_point(snapshot, end)?;
    if start.raw_line >= snapshot.raw_plain_lines.len()
        || end.raw_line >= snapshot.raw_plain_lines.len()
    {
        return None;
    }

    let mut out = Vec::new();
    for raw_line in start.raw_line..=end.raw_line {
        let text = &snapshot.raw_plain_lines[raw_line];
        let line_width = line_display_width(text);
        let start_col = if raw_line == start.raw_line {
            clamp_display_col(text, start.column)
        } else {
            0
        };
        let end_col = if raw_line == end.raw_line {
            clamp_display_col(text, end.column)
        } else {
            line_width
        };

        if end_col < start_col {
            out.push(String::new());
            continue;
        }

        let start_byte = display_col_to_byte_offset(text, start_col);
        let end_byte = display_col_to_byte_offset(text, end_col);
        out.push(text[start_byte..end_byte].to_string());
    }

    Some(out.join("\n"))
}

fn raw_selection_point(
    snapshot: &CopyViewportSnapshot,
    point: crate::tui::CopySelectionPoint,
) -> Option<RawSelectionPoint> {
    let wrapped_text = snapshot.wrapped_plain_lines.get(point.abs_line)?;
    let map = snapshot.wrapped_line_map.get(point.abs_line)?;
    let display_copy_start = snapshot
        .wrapped_copy_offsets
        .get(point.abs_line)
        .copied()
        .unwrap_or(0)
        .min(wrapped_text.width());
    let local_col = clamp_display_col(wrapped_text, point.column).max(display_copy_start);
    let segment_width = map.end_col.saturating_sub(map.start_col);
    Some(RawSelectionPoint {
        raw_line: map.raw_line,
        column: map.start_col
            + local_col
                .saturating_sub(display_copy_start)
                .min(segment_width),
    })
}

fn trim_url_candidate(candidate: &str) -> &str {
    let mut trimmed = candidate;
    loop {
        let next = if trimmed.ends_with(['.', ',', ';', ':', '!', '?'])
            || (trimmed.ends_with(')')
                && trimmed.matches(')').count() > trimmed.matches('(').count())
            || (trimmed.ends_with(']')
                && trimmed.matches(']').count() > trimmed.matches('[').count())
            || (trimmed.ends_with('}')
                && trimmed.matches('}').count() > trimmed.matches('{').count())
        {
            &trimmed[..trimmed.len() - 1]
        } else {
            trimmed
        };

        if next.len() == trimmed.len() {
            return trimmed;
        }
        trimmed = next;
    }
}

fn link_target_from_snapshot(
    snapshot: &CopyViewportSnapshot,
    point: crate::tui::CopySelectionPoint,
) -> Option<String> {
    let raw_point = raw_selection_point(snapshot, point)?;
    let raw_text = snapshot.raw_plain_lines.get(raw_point.raw_line)?;

    for mat in url_regex().find_iter(raw_text) {
        let matched = &raw_text[mat.start()..mat.end()];
        let trimmed = trim_url_candidate(matched);
        if trimmed.is_empty() {
            continue;
        }

        let start_col = line_display_width(&raw_text[..mat.start()]);
        let end_col = start_col + line_display_width(trimmed);
        if raw_point.column >= start_col
            && raw_point.column < end_col
            && url::Url::parse(trimmed).is_ok()
        {
            return Some(trimmed.to_string());
        }
    }

    None
}

pub(crate) fn link_target_from_screen(column: u16, row: u16) -> Option<String> {
    let point = copy_point_from_screen(column, row)?;
    let snapshot = copy_snapshot_for_pane(point.pane)?;
    link_target_from_snapshot(&snapshot, point)
}

fn profile_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("JCODE_TUI_PROFILE").is_ok())
}

fn record_profile(prepare: Duration, draw: Duration, total: Duration) {
    let mut state = match profile_state().lock() {
        Ok(s) => s,
        Err(poisoned) => poisoned.into_inner(),
    };
    state.frames += 1;
    state.prepare += prepare;
    state.draw += draw;
    state.total += total;

    let now = Instant::now();
    let should_log = match state.last_log {
        Some(last) => now.duration_since(last) >= Duration::from_secs(1),
        None => true,
    };
    if should_log && state.frames > 0 {
        let frames = state.frames as f64;
        let avg_prepare = state.prepare.as_secs_f64() * 1000.0 / frames;
        let avg_draw = state.draw.as_secs_f64() * 1000.0 / frames;
        let avg_total = state.total.as_secs_f64() * 1000.0 / frames;
        crate::logging::info(&format!(
            "TUI perf: {:.1} fps | prepare {:.2}ms | draw {:.2}ms | total {:.2}ms",
            frames, avg_prepare, avg_draw, avg_total
        ));
        state.frames = 0;
        state.prepare = Duration::from_secs(0);
        state.draw = Duration::from_secs(0);
        state.total = Duration::from_secs(0);
        state.last_log = Some(now);
    }
}

pub fn draw(frame: &mut Frame, app: &dyn TuiState) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::tui::markdown::with_deferred_mermaid_render_context(|| draw_inner(frame, app))
    })) {
        Ok(()) => {}
        Err(payload) => {
            let panic_count = DRAW_PANIC_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            let msg = panic_payload_to_string(&payload);
            if panic_count <= 3 || panic_count.is_multiple_of(50) {
                crate::logging::error(&format!(
                    "Recovered TUI draw panic #{}: {}",
                    panic_count, msg
                ));
            }
            let area = frame.area().intersection(*frame.buffer_mut().area());
            if area.width == 0 || area.height == 0 {
                return;
            }
            clear_area(frame, area);
            let lines = vec![
                Line::from(Span::styled(
                    "rendering error recovered",
                    Style::default().fg(Color::Red),
                )),
                Line::from(Span::styled(
                    "continuing with a safe fallback frame",
                    Style::default().fg(dim_color()),
                )),
            ];
            frame.render_widget(Paragraph::new(lines), area);
        }
    }
}

fn panic_payload_to_string(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

fn draw_inner(frame: &mut Frame, app: &dyn TuiState) {
    let area = frame.area().intersection(*frame.buffer_mut().area());
    if area.width == 0 || area.height == 0 {
        return;
    }

    clear_copy_viewport_snapshot();

    // Clear full frame to prevent stale cells from prior layouts.
    // This is critical on macOS terminals where ratatui's diff-based updates
    // can leave outdated content when layout dimensions change between frames
    // (e.g., diagram pane toggling, streaming text clearing, tool calls finishing).
    // Uses Color::Reset (terminal default bg) so text selection highlighting works
    // natively in all terminal emulators.
    clear_area(frame, area);

    if let Some(scroll) = app.changelog_scroll() {
        overlays::draw_changelog_overlay(frame, area, scroll);
        return;
    }

    if let Some(scroll) = app.help_scroll() {
        overlays::draw_help_overlay(frame, area, scroll, app);
        return;
    }

    if let Some(picker_cell) = app.session_picker_overlay() {
        let mut picker = picker_cell.borrow_mut();
        picker.render(frame);
        return;
    }

    if let Some(picker_cell) = app.login_picker_overlay() {
        let picker = picker_cell.borrow();
        picker.render(frame);
        return;
    }

    if let Some(picker_cell) = app.account_picker_overlay() {
        let picker = picker_cell.borrow();
        picker.render(frame);
        return;
    }

    // Initialize visual debug capture if enabled
    let mut debug_capture = if visual_debug::is_enabled() {
        Some(FrameCaptureBuilder::new(area.width, area.height))
    } else {
        None
    };

    // Check diagram display mode and get active diagrams early so we can
    // determine the horizontal split before computing input width etc.
    let diagram_mode = app.diagram_mode();
    let diagrams = super::mermaid::get_active_diagrams();
    let diagram_count = diagrams.len();
    let selected_index = if diagram_count > 0 {
        app.diagram_index().min(diagram_count - 1)
    } else {
        0
    };
    let pane_enabled = app.diagram_pane_enabled();
    let pane_position = app.diagram_pane_position();
    let has_side_panel_content = app.side_panel().focused_page().is_some();
    let suppress_side_diagram =
        has_side_panel_content && pane_position == crate::config::DiagramPanePosition::Side;
    let pinned_diagram = if diagram_mode == crate::config::DiagramDisplayMode::Pinned
        && pane_enabled
        && !suppress_side_diagram
    {
        diagrams.get(selected_index).cloned()
    } else {
        None
    };
    let diagram_focus = app.diagram_focus();
    let (diagram_scroll_x, diagram_scroll_y) = app.diagram_scroll();

    // Compute layout depending on pane position (Side = right column, Top = above chat).
    let (chat_area, diagram_area) = if let Some(diagram) = pinned_diagram.as_ref() {
        match pane_position {
            crate::config::DiagramPanePosition::Side => {
                const MIN_DIAGRAM_WIDTH: u16 = 24;
                const MIN_CHAT_WIDTH: u16 = 20;
                const AUTO_DIAGRAM_WIDTH_CAP_PERCENT: u32 = 50;
                let max_diagram = area.width.saturating_sub(MIN_CHAT_WIDTH);
                if max_diagram >= MIN_DIAGRAM_WIDTH {
                    let ratio = app.diagram_pane_ratio().clamp(25, 100) as u32;
                    let ratio_target = ((area.width as u32 * ratio) / 100) as u16;
                    let auto_cap =
                        ((area.width as u32 * AUTO_DIAGRAM_WIDTH_CAP_PERCENT) / 100) as u16;
                    let needed =
                        estimate_pinned_diagram_pane_width(diagram, area.height, MIN_DIAGRAM_WIDTH);
                    let auto_target = needed.min(max_diagram).min(auto_cap.max(MIN_DIAGRAM_WIDTH));
                    let diagram_width = ratio_target
                        .max(auto_target)
                        .max(MIN_DIAGRAM_WIDTH)
                        .min(max_diagram);
                    let chat_width = area.width.saturating_sub(diagram_width);
                    if diagram_width > 0 && chat_width > 0 {
                        let chat = Rect {
                            x: area.x,
                            y: area.y,
                            width: chat_width,
                            height: area.height,
                        };
                        let diag = Rect {
                            x: area.x + chat_width,
                            y: area.y,
                            width: diagram_width,
                            height: area.height,
                        };
                        (chat, Some(diag))
                    } else {
                        (area, None)
                    }
                } else {
                    (area, None)
                }
            }
            crate::config::DiagramPanePosition::Top => {
                const MIN_DIAGRAM_HEIGHT: u16 = 6;
                const MIN_CHAT_HEIGHT: u16 = 8;
                let max_diagram = area.height.saturating_sub(MIN_CHAT_HEIGHT);
                if max_diagram >= MIN_DIAGRAM_HEIGHT {
                    let ratio = app.diagram_pane_ratio().clamp(20, 100) as u32;
                    let ratio_target = ((area.height as u32 * ratio) / 100) as u16;
                    let needed = estimate_pinned_diagram_pane_height(
                        diagram,
                        area.width,
                        MIN_DIAGRAM_HEIGHT,
                    );
                    let diagram_height = ratio_target
                        .max(needed.min(max_diagram))
                        .max(MIN_DIAGRAM_HEIGHT)
                        .min(max_diagram);
                    let chat_height = area.height.saturating_sub(diagram_height);
                    if diagram_height > 0 && chat_height > 0 {
                        let diag = Rect {
                            x: area.x,
                            y: area.y,
                            width: area.width,
                            height: diagram_height,
                        };
                        let chat = Rect {
                            x: area.x,
                            y: area.y + diagram_height,
                            width: area.width,
                            height: chat_height,
                        };
                        (chat, Some(diag))
                    } else {
                        (area, None)
                    }
                } else {
                    (area, None)
                }
            }
        }
    } else {
        (area, None)
    };

    let diff_mode = app.diff_mode();
    let pin_images = app.pin_images();
    let collect_diffs = diff_mode.is_pinned();
    let has_pinned_content = if collect_diffs || pin_images {
        collect_pinned_content_cached(
            app.display_messages(),
            &app.side_pane_images(),
            collect_diffs,
            pin_images,
            app.display_messages_version(),
        )
    } else {
        false
    };
    let has_file_diff_edits = diff_mode.is_file()
        && app.display_messages().iter().any(|m| {
            m.tool_data
                .as_ref()
                .map(|tc| tools_ui::is_edit_tool_name(&tc.name))
                .unwrap_or(false)
        });

    let needs_side_pane = has_side_panel_content || has_pinned_content || has_file_diff_edits;

    let (chat_area, diff_pane_area) = if needs_side_pane {
        const MIN_DIFF_WIDTH: u16 = 30;
        const MIN_CHAT_WIDTH: u16 = 20;
        let max_diff = chat_area.width.saturating_sub(MIN_CHAT_WIDTH);
        if max_diff >= MIN_DIFF_WIDTH {
            let diff_width = (((chat_area.width as u32
                * app.diagram_pane_ratio().clamp(25, 100) as u32)
                / 100) as u16)
                .max(MIN_DIFF_WIDTH)
                .min(max_diff);
            let new_chat_width = chat_area.width.saturating_sub(diff_width);
            let chat = Rect {
                x: chat_area.x,
                y: chat_area.y,
                width: new_chat_width,
                height: chat_area.height,
            };
            let diff = Rect {
                x: chat_area.x + new_chat_width,
                y: chat_area.y,
                width: diff_width,
                height: chat_area.height,
            };
            (chat, Some(diff))
        } else {
            (chat_area, None)
        }
    } else {
        (chat_area, None)
    };

    // Calculate pending messages (queued + interleave) for numbering and layout
    let pending_count = input_ui::pending_prompt_count(app);
    let queued_height = pending_count.min(3) as u16;

    // Count user messages to show next prompt number
    let user_count = app
        .display_messages()
        .iter()
        .filter(|m| m.role == "user")
        .count();
    let next_prompt = user_count + 1;

    // Calculate input height based on the same wrapping logic used for rendering
    // (max 10 lines visible, scrolls if more).
    let base_input_height =
        input_ui::wrapped_input_line_count(app, chat_area.width, next_prompt).min(10) as u16;
    // Add 1 line for command suggestions, shell mode hints, or the Ctrl+Enter hint.
    let hint_line_height = input_ui::input_hint_line_height(app);
    let inline_block_height: u16 = inline_ui_height(app);
    let inline_ui_gap_height: u16 = if inline_block_height > 0 { 1 } else { 0 };
    let input_height = base_input_height + hint_line_height;

    let total_start = Instant::now();
    if let Some(ref mut capture) = debug_capture {
        capture.render_order.push("prepare_messages".to_string());
    }
    let prep_start = Instant::now();
    let chat_left_inset = left_aligned_content_inset(chat_area.width, app.centered_mode());
    let prepared_full_width = prepare::prepare_messages(
        app,
        chat_area.width.saturating_sub(chat_left_inset),
        chat_area.height,
    );
    let show_donut = super::idle_donut_active(app);
    let donut_height: u16 = if show_donut { 14 } else { 0 };
    let notification_height: u16 = if app.has_notification() { 1 } else { 0 };
    let fixed_height = 1
        + queued_height
        + notification_height
        + inline_block_height
        + inline_ui_gap_height
        + input_height
        + donut_height; // status + queued + notification + inline UI + gap + input + donut
    let available_height = chat_area.height;

    let initial_content_height = prepared_full_width.wrapped_lines.len().max(1) as u16;
    let chat_scrollbar_visible = app.chat_native_scrollbar()
        && chat_area.width > 1
        && initial_content_height + fixed_height > available_height;
    let prepared = if chat_scrollbar_visible {
        prepare::prepare_messages(
            app,
            chat_area
                .width
                .saturating_sub(chat_left_inset)
                .saturating_sub(1),
            chat_area.height,
        )
    } else {
        prepared_full_width
    };
    if let Some(ref mut capture) = debug_capture {
        capture.image_regions = prepared
            .image_regions
            .iter()
            .map(|region| ImageRegionCapture {
                hash: format!("{:016x}", region.hash),
                abs_line_idx: region.abs_line_idx,
                height: region.height,
            })
            .collect();
    }
    let prep_elapsed = prep_start.elapsed();
    let content_height = prepared.wrapped_lines.len().max(1) as u16;

    // Use packed layout when content fits, scrolling layout otherwise
    let use_packed = content_height + fixed_height <= available_height;

    // Layout: messages (includes header), queued, status, notification, inline UI, gap, input, donut
    // All vertical chunks are within the chat_area (left column).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if use_packed {
            vec![
                Constraint::Length(content_height.max(1)), // Messages (exact height)
                Constraint::Length(queued_height),         // Queued messages (above status)
                Constraint::Length(1),                     // Status line
                Constraint::Length(notification_height),   // Notification line
                Constraint::Length(inline_block_height),   // Inline UI
                Constraint::Length(inline_ui_gap_height),  // Inline UI/input spacing
                Constraint::Length(input_height),          // Input
                Constraint::Length(donut_height),          // Donut animation
            ]
        } else {
            vec![
                Constraint::Min(3),                       // Messages (scrollable)
                Constraint::Length(queued_height),        // Queued messages (above status)
                Constraint::Length(1),                    // Status line
                Constraint::Length(notification_height),  // Notification line
                Constraint::Length(inline_block_height),  // Inline UI
                Constraint::Length(inline_ui_gap_height), // Inline UI/input spacing
                Constraint::Length(input_height),         // Input
                Constraint::Length(donut_height),         // Donut animation
            ]
        })
        .split(chat_area);

    // Capture layout info for visual debug
    if let Some(ref mut capture) = debug_capture {
        capture.layout.use_packed = use_packed;
        capture.layout.estimated_content_height = content_height as usize;
        capture.layout.messages_area = Some(chunks[0].into());
        if queued_height > 0 {
            capture.layout.queued_area = Some(chunks[1].into());
        }
        capture.layout.status_area = Some(chunks[2].into());
        capture.layout.input_area = Some(chunks[6].into());
        capture.layout.input_lines_raw = app.input().lines().count().max(1);
        capture.layout.input_lines_wrapped = base_input_height as usize;

        // Capture state snapshot
        capture.state.is_processing = app.is_processing();
        capture.state.input_len = app.input().len();
        capture.state.input_preview = app.input().chars().take(100).collect();
        capture.state.cursor_pos = app.cursor_pos();
        capture.state.scroll_offset = app.scroll_offset();
        capture.state.queued_count = pending_count;
        capture.state.message_count = app.display_messages().len();
        capture.state.streaming_text_len = app.streaming_text().len();
        capture.state.has_suggestions = !app.command_suggestions().is_empty();
        capture.state.status = format!("{:?}", app.status());
        capture.state.diagram_mode = Some(format!("{:?}", diagram_mode));
        capture.state.diagram_focus = diagram_focus;
        capture.state.diagram_index = selected_index;
        capture.state.diagram_count = diagram_count;
        capture.state.diagram_scroll_x = diagram_scroll_x;
        capture.state.diagram_scroll_y = diagram_scroll_y;
        capture.state.diagram_pane_ratio = app.diagram_pane_ratio();
        capture.state.diagram_pane_enabled = app.diagram_pane_enabled();
        capture.state.diagram_pane_position = Some(format!("{:?}", app.diagram_pane_position()));
        capture.state.diagram_zoom = app.diagram_zoom();

        // Capture rendered content
        // Queued messages
        capture.rendered_text.queued_messages = input_ui::pending_queue_preview(app);

        // Recent display messages (last 5 for context)
        capture.rendered_text.recent_messages = app
            .display_messages()
            .iter()
            .rev()
            .take(5)
            .map(|m| MessageCapture {
                role: m.role.clone(),
                content_preview: m.content.chars().take(200).collect(),
                content_len: m.content.len(),
            })
            .collect();

        // Streaming text preview
        let streaming = app.streaming_text();
        if !streaming.is_empty() {
            capture.rendered_text.streaming_text_preview = streaming.chars().take(500).collect();
        }

        // Status line content
        capture.rendered_text.status_line = format_status_for_debug(app);
    }

    if let Some(ref mut capture) = debug_capture {
        capture.render_order.push("draw_messages".to_string());
    }
    let draw_start = Instant::now();

    // Messages area is chunks[0] within the chat column (already excludes diagram).
    let messages_area = chunks[0];

    if let Some(ref mut capture) = debug_capture {
        capture.layout.messages_area = Some(messages_area.into());
        capture.layout.diagram_area = diagram_area.map(|r| r.into());
    }
    record_layout_snapshot(messages_area, diagram_area, diff_pane_area, Some(chunks[6]));

    let margins = draw_messages(frame, app, messages_area, &prepared, chat_scrollbar_visible);

    // Render pinned diagram if we have one
    if let (Some(diagram_info), Some(area)) = (&pinned_diagram, diagram_area) {
        if let Some(ref mut capture) = debug_capture {
            capture.render_order.push("draw_pinned_diagram".to_string());
        }
        draw_pinned_diagram(
            frame,
            diagram_info,
            area,
            selected_index,
            diagram_count,
            diagram_focus,
            diagram_scroll_x,
            diagram_scroll_y,
            app.diagram_zoom(),
            pane_position,
            app.diagram_pane_animating(),
        );
    }

    if let Some(diff_area) = diff_pane_area {
        if has_side_panel_content {
            if let Some(ref mut capture) = debug_capture {
                capture
                    .render_order
                    .push("draw_side_panel_markdown".to_string());
            }
            draw_side_panel_markdown(
                frame,
                diff_area,
                app,
                app.side_panel(),
                app.diff_pane_scroll(),
                app.diff_pane_focus(),
                app.centered_mode(),
            );
        } else if has_file_diff_edits {
            if let Some(ref mut capture) = debug_capture {
                capture.render_order.push("draw_file_diff_view".to_string());
            }
            draw_file_diff_view(
                frame,
                diff_area,
                app,
                &prepared,
                app.diff_pane_scroll(),
                app.diff_pane_focus(),
            );
        } else if has_pinned_content {
            if let Some(ref mut capture) = debug_capture {
                capture.render_order.push("draw_pinned_content".to_string());
            }
            draw_pinned_content_cached(
                frame,
                diff_area,
                app,
                app.diff_pane_scroll(),
                app.diff_line_wrap(),
                app.diff_pane_focus(),
            );
        }
    }

    let messages_draw = draw_start.elapsed();

    if let Some(ref mut capture) = debug_capture {
        capture.layout.margins = Some(MarginsCapture {
            left_widths: margins.left_widths.clone(),
            right_widths: margins.right_widths.clone(),
            centered: margins.centered,
        });
    }
    if queued_height > 0 {
        if let Some(ref mut capture) = debug_capture {
            capture.render_order.push("draw_queued".to_string());
        }
        input_ui::draw_queued(frame, app, chunks[1], user_count + 1);
    }
    if let Some(ref mut capture) = debug_capture {
        capture.render_order.push("draw_status".to_string());
    }
    input_ui::draw_status(frame, app, chunks[2], pending_count);
    if notification_height > 0 {
        input_ui::draw_notification(frame, app, chunks[3]);
    }
    if let Some(ref mut capture) = debug_capture {
        capture.render_order.push("draw_input".to_string());
    }
    // Draw inline UI if active
    if inline_block_height > 0 {
        draw_inline_ui(frame, app, chunks[4]);
    }

    input_ui::draw_input(
        frame,
        app,
        chunks[6],
        user_count + pending_count + 1,
        &mut debug_capture,
    );

    if donut_height > 0 {
        animations::draw_idle_animation(frame, app, chunks[7]);
    }

    // Draw info widget overlays (skip during idle animation - they look out of place)
    let widget_data = app.info_widget_data();
    let mut widget_render_ms: Option<f32> = None;
    let mut placements: Vec<info_widget::WidgetPlacement> = Vec::new();
    let widget_bounds = messages_area;
    if !widget_data.is_empty() && !show_donut {
        if let Some(ref mut capture) = debug_capture {
            capture.render_order.push("render_info_widgets".to_string());
        }
        placements = info_widget::calculate_placements(widget_bounds, &margins, &widget_data);

        if let Some(ref mut capture) = debug_capture {
            let placement_captures = capture_widget_placements(&placements);
            capture.layout.widget_placements = placement_captures.clone();
            capture.info_widgets = Some(InfoWidgetCapture {
                summary: build_info_widget_summary(&widget_data),
                placements: placement_captures,
            });

            // Detect overlaps with message area
            for placement in &placements {
                if rects_overlap(placement.rect, widget_bounds) {
                    capture.anomaly(format!(
                        "Info widget {:?} overlaps messages area",
                        placement.kind
                    ));
                }
                if !rect_within_bounds(placement.rect, area) {
                    capture.anomaly(format!(
                        "Info widget {:?} out of bounds {:?}",
                        placement.kind, placement.rect
                    ));
                }
                if let Some(diagram_area) = diagram_area
                    && rects_overlap(placement.rect, diagram_area)
                {
                    capture.anomaly(format!(
                        "Info widget {:?} overlaps diagram area",
                        placement.kind
                    ));
                }
            }
            for i in 0..placements.len() {
                for j in (i + 1)..placements.len() {
                    if rects_overlap(placements[i].rect, placements[j].rect) {
                        capture.anomaly(format!(
                            "Info widgets overlap: {:?} and {:?}",
                            placements[i].kind, placements[j].kind
                        ));
                    }
                }
            }
        }

        let widget_start = Instant::now();
        info_widget::render_all(frame, &placements, &widget_data);
        widget_render_ms = Some(widget_start.elapsed().as_secs_f32() * 1000.0);

        // Optional visual overlay for placements
    } else if let Some(ref mut capture) = debug_capture {
        capture.info_widgets = Some(InfoWidgetCapture {
            summary: build_info_widget_summary(&widget_data),
            placements: Vec::new(),
        });
    }

    if visual_debug::overlay_enabled() {
        overlays::draw_debug_overlay(frame, &placements, &chunks);
    }

    // Record the frame capture if enabled
    if let Some(capture) = debug_capture {
        let total_draw = draw_start.elapsed();
        let render_timing = RenderTimingCapture {
            prepare_ms: prep_elapsed.as_secs_f32() * 1000.0,
            draw_ms: total_draw.as_secs_f32() * 1000.0,
            total_ms: total_start.elapsed().as_secs_f32() * 1000.0,
            messages_ms: Some(messages_draw.as_secs_f32() * 1000.0),
            widgets_ms: widget_render_ms,
        };

        let mut capture = capture;
        capture.render_timing = Some(render_timing);
        capture.mermaid = crate::tui::mermaid::debug_stats_json();
        capture.markdown = crate::tui::markdown::debug_stats_json();
        capture.theme = overlays::debug_palette_json();
        visual_debug::record_frame(capture.build());
    }

    if profile_enabled() {
        let total_draw = draw_start.elapsed();
        record_profile(prep_elapsed, total_draw, total_start.elapsed());
    }
}

fn inline_ui_gap_height(app: &dyn TuiState) -> u16 {
    if app.inline_ui_state().is_some() {
        1
    } else {
        0
    }
}

fn extract_line_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn extract_line_styled_chars(line: &Line) -> Vec<(char, Style)> {
    let mut chars = Vec::new();
    for span in &line.spans {
        for ch in span.content.chars() {
            chars.push((ch, span.style));
        }
    }
    chars
}

fn morph_lines_to_header(
    anim_lines: &[Line<'static>],
    header_lines: &[Line<'static>],
    morph_t: f32,
    width: u16,
) -> Vec<Line<'static>> {
    let blend = ((morph_t - 0.6) / 0.35).clamp(0.0, 1.0);
    let max_rows = anim_lines.len().max(header_lines.len());
    let w = width as usize;

    let mut result = Vec::with_capacity(max_rows);

    let anim_row_count = anim_lines.len();
    let header_row_count = header_lines.len();
    let row_blend = blend * blend;
    let target_rows =
        anim_row_count as f32 + (header_row_count as f32 - anim_row_count as f32) * row_blend;
    let output_rows = target_rows.round() as usize;

    for out_row in 0..output_rows {
        let anim_row_f = if output_rows > 1 {
            out_row as f32 / (output_rows - 1) as f32 * (anim_row_count.max(1) - 1) as f32
        } else {
            0.0
        };
        let header_row_f = if output_rows > 1 {
            out_row as f32 / (output_rows - 1) as f32 * (header_row_count.max(1) - 1) as f32
        } else {
            0.0
        };

        let anim_idx = (anim_row_f.round() as usize).min(anim_row_count.saturating_sub(1));
        let header_idx = (header_row_f.round() as usize).min(header_row_count.saturating_sub(1));

        let anim_chars: Vec<(char, Style)> = if anim_idx < anim_row_count {
            extract_line_styled_chars(&anim_lines[anim_idx])
        } else {
            Vec::new()
        };
        let header_chars: Vec<(char, Style)> = if header_idx < header_row_count {
            extract_line_styled_chars(&header_lines[header_idx])
        } else {
            Vec::new()
        };

        let anim_text: String = anim_chars.iter().map(|(c, _)| *c).collect();
        let header_text: String = header_chars.iter().map(|(c, _)| *c).collect();
        let anim_trimmed = anim_text.trim();
        let header_trimmed = header_text.trim();

        let anim_start = anim_text.find(anim_trimmed).unwrap_or(0);
        let header_start = header_text.find(header_trimmed).unwrap_or(0);

        let anim_center = if !anim_trimmed.is_empty() {
            anim_start as f32 + anim_trimmed.len() as f32 / 2.0
        } else {
            w as f32 / 2.0
        };
        let header_center = if !header_trimmed.is_empty() {
            header_start as f32 + header_trimmed.len() as f32 / 2.0
        } else {
            w as f32 / 2.0
        };

        let center = anim_center + (header_center - anim_center) * blend;
        let max_col = anim_chars.len().max(header_chars.len()).max(w);

        let mut spans: Vec<Span<'static>> = Vec::new();

        for col in 0..max_col {
            let anim_ch = anim_chars.get(col).map(|(c, _)| *c).unwrap_or(' ');
            let anim_style = anim_chars.get(col).map(|(_, s)| *s).unwrap_or_default();
            let header_ch = header_chars.get(col).map(|(c, _)| *c).unwrap_or(' ');
            let header_style = header_chars.get(col).map(|(_, s)| *s).unwrap_or_default();

            let dist_from_center = ((col as f32) - center).abs() / (w as f32 / 2.0).max(1.0);
            let flip_hash = {
                let mut h = DefaultHasher::new();
                out_row.hash(&mut h);
                col.hash(&mut h);
                (std::hash::Hasher::finish(&h) % 1000) as f32 / 1000.0
            };
            let flip_threshold = (0.3 + dist_from_center * 0.4 + flip_hash * 0.3).clamp(0.0, 1.0);

            let (ch, style) = if blend >= flip_threshold {
                let style_blend = ((blend - flip_threshold) / 0.15).clamp(0.0, 1.0);
                if style_blend < 0.3 {
                    let glitch_chars = b"@#$%&*!?~=+<>";
                    let gi = {
                        let mut h = DefaultHasher::new();
                        out_row.hash(&mut h);
                        col.hash(&mut h);
                        ((blend * 100.0) as u32).hash(&mut h);
                        (std::hash::Hasher::finish(&h) % glitch_chars.len() as u64) as usize
                    };
                    let gc = glitch_chars[gi] as char;
                    (gc, lerp_style(anim_style, header_style, style_blend))
                } else {
                    (header_ch, lerp_style(anim_style, header_style, style_blend))
                }
            } else {
                let fade = (1.0 - blend / flip_threshold.max(0.01)).clamp(0.0, 1.0);
                let mut s = anim_style;
                if let Some(fg) = s.fg {
                    let (r, g, b) = color_to_floats(fg, (80.0, 80.0, 80.0));
                    s.fg = Some(rgb((r * fade) as u8, (g * fade) as u8, (b * fade) as u8));
                }
                (anim_ch, s)
            };

            spans.push(Span::styled(ch.to_string(), style));
        }

        let align = header_lines
            .get(header_idx)
            .and_then(|l| l.alignment)
            .or_else(|| anim_lines.get(anim_idx).and_then(|l| l.alignment))
            .unwrap_or(ratatui::layout::Alignment::Center);

        result.push(Line::from(spans).alignment(align));
    }

    result
}

fn lerp_style(from: Style, to: Style, t: f32) -> Style {
    let fg = match (from.fg, to.fg) {
        (Some(f), Some(toc)) => {
            let (r1, g1, b1) = color_to_floats(f, (80.0, 80.0, 80.0));
            let (r2, g2, b2) = color_to_floats(toc, (200.0, 200.0, 200.0));
            Some(rgb(
                (r1 + (r2 - r1) * t).clamp(0.0, 255.0) as u8,
                (g1 + (g2 - g1) * t).clamp(0.0, 255.0) as u8,
                (b1 + (b2 - b1) * t).clamp(0.0, 255.0) as u8,
            ))
        }
        (Some(f), _) => {
            let (r, g, b) = color_to_floats(f, (80.0, 80.0, 80.0));
            let dim = 1.0 - t;
            Some(rgb((r * dim) as u8, (g * dim) as u8, (b * dim) as u8))
        }
        (_, Some(toc)) => {
            let (r, g, b) = color_to_floats(toc, (200.0, 200.0, 200.0));
            Some(rgb((r * t) as u8, (g * t) as u8, (b * t) as u8))
        }
        (_, to_fg) => to_fg,
    };
    let mut s = to;
    s.fg = fg;
    s
}

pub(crate) fn split_native_scrollbar_area(area: Rect, enabled: bool) -> (Rect, Option<Rect>) {
    if !enabled || area.width <= 1 {
        return (area, None);
    }

    let content = Rect {
        width: area.width.saturating_sub(1),
        ..area
    };
    let scrollbar = Rect {
        x: area.x.saturating_add(area.width.saturating_sub(1)),
        y: area.y,
        width: 1,
        height: area.height,
    };
    (content, Some(scrollbar))
}

pub(crate) fn native_scrollbar_visible(
    enabled: bool,
    total_lines: usize,
    visible_height: usize,
) -> bool {
    enabled && visible_height > 0 && total_lines > visible_height
}

pub(crate) fn render_native_scrollbar(
    frame: &mut Frame,
    area: Rect,
    scroll: usize,
    total_lines: usize,
    visible_height: usize,
    focused: bool,
) {
    if area.width == 0
        || area.height == 0
        || !native_scrollbar_visible(true, total_lines, visible_height)
    {
        return;
    }

    let track_height = area.height as usize;
    let thumb_height = if visible_height == 0 || total_lines == 0 {
        1
    } else if total_lines <= visible_height {
        track_height
    } else {
        ((visible_height * track_height).div_ceil(total_lines)).clamp(1, track_height)
    };
    let max_thumb_offset = track_height.saturating_sub(thumb_height);
    let max_scroll = total_lines.saturating_sub(visible_height);
    let thumb_offset = if max_scroll == 0 {
        0
    } else {
        scroll.min(max_scroll) * max_thumb_offset / max_scroll
    };

    let thumb_color = if focused {
        rgb(188, 208, 240)
    } else {
        rgb(136, 148, 172)
    };

    let mut lines = Vec::with_capacity(track_height);
    for row in 0..track_height {
        let (glyph, color) = if row >= thumb_offset && row < thumb_offset + thumb_height {
            let glyph = if thumb_height == 1 {
                "•"
            } else if row == thumb_offset {
                "╷"
            } else if row + 1 == thumb_offset + thumb_height {
                "╵"
            } else {
                "│"
            };
            (glyph, thumb_color)
        } else {
            (" ", Color::Reset)
        };
        lines.push(Line::from(Span::styled(glyph, Style::default().fg(color))));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
#[path = "ui_tests/mod.rs"]
mod tests;

pub(crate) fn format_inline_interactive_elapsed(secs: f32) -> String {
    inline_interactive_ui::format_elapsed(secs)
}
