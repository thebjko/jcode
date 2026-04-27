use super::animation::{FOCUS_PULSE_DURATION, VIEWPORT_ANIMATION_DURATION};
use super::single_session::*;
use super::*;

#[test]
fn quarter_size_preset_follows_quarter_screen_width_steps() {
    let monitor_width = Some(2000);

    assert_eq!(inferred_visible_column_count(500, monitor_width, 0.25), 1);
    assert_eq!(inferred_visible_column_count(1000, monitor_width, 0.25), 2);
    assert_eq!(inferred_visible_column_count(1500, monitor_width, 0.25), 3);
    assert_eq!(inferred_visible_column_count(2000, monitor_width, 0.25), 4);
}

#[test]
fn preferred_panel_size_limits_visible_column_count() {
    let monitor_width = Some(2000);

    assert_eq!(inferred_visible_column_count(2000, monitor_width, 0.25), 4);
    assert_eq!(inferred_visible_column_count(2000, monitor_width, 0.50), 2);
    assert_eq!(inferred_visible_column_count(2000, monitor_width, 0.75), 1);
    assert_eq!(inferred_visible_column_count(2000, monitor_width, 1.00), 1);

    assert_eq!(inferred_visible_column_count(500, monitor_width, 0.25), 1);
    assert_eq!(inferred_visible_column_count(500, monitor_width, 1.00), 1);
}

#[test]
fn visible_column_count_tolerates_window_manager_gaps() {
    let monitor_width = Some(2000);

    assert_eq!(inferred_visible_column_count(1940, monitor_width, 0.25), 4);
    assert_eq!(inferred_visible_column_count(970, monitor_width, 0.25), 2);
    assert_eq!(inferred_visible_column_count(1940, monitor_width, 0.50), 2);
}

#[test]
fn visible_column_count_is_clamped_and_safe_without_monitor() {
    assert_eq!(inferred_visible_column_count(1, Some(2000), 0.25), 1);
    assert_eq!(inferred_visible_column_count(3000, Some(2000), 0.25), 4);
    assert_eq!(inferred_visible_column_count(1000, Some(0), 0.25), 1);
    assert_eq!(inferred_visible_column_count(1000, None, 0.25), 1);
}

#[test]
fn viewport_animation_interpolates_to_new_layout_target() {
    let mut animation = AnimatedViewport::default();
    let now = Instant::now();
    let visible = VisibleColumnLayout {
        visible_columns: 2,
        first_visible_column: 0,
    };
    let start = WorkspaceRenderLayout {
        visible,
        column_width: 200.0,
        scroll_offset: 0.0,
        vertical_scroll_offset: 0.0,
    };
    let target = WorkspaceRenderLayout {
        visible: VisibleColumnLayout {
            visible_columns: 2,
            first_visible_column: 2,
        },
        column_width: 300.0,
        scroll_offset: 600.0,
        vertical_scroll_offset: 800.0,
    };

    let first_frame = animation.frame(start, now);
    assert_eq!(first_frame.column_width, 200.0);
    assert_eq!(first_frame.scroll_offset, 0.0);
    assert_eq!(first_frame.vertical_scroll_offset, 0.0);
    assert!(!animation.is_animating());

    let transition_start = animation.frame(target, now);
    assert_eq!(transition_start.column_width, 200.0);
    assert_eq!(transition_start.scroll_offset, 0.0);
    assert_eq!(transition_start.vertical_scroll_offset, 0.0);
    assert!(animation.is_animating());

    let middle = animation.frame(target, now + VIEWPORT_ANIMATION_DURATION / 2);
    assert!(middle.column_width > 200.0);
    assert!(middle.column_width < 300.0);
    assert!(middle.scroll_offset > 0.0);
    assert!(middle.scroll_offset < 600.0);
    assert!(middle.vertical_scroll_offset > 0.0);
    assert!(middle.vertical_scroll_offset < 800.0);

    let final_frame = animation.frame(target, now + VIEWPORT_ANIMATION_DURATION);
    assert_eq!(final_frame.column_width, 300.0);
    assert_eq!(final_frame.scroll_offset, 600.0);
    assert_eq!(final_frame.vertical_scroll_offset, 800.0);
    assert!(!animation.is_animating());
}

#[test]
fn focus_pulse_runs_when_focused_surface_changes() {
    let mut pulse = FocusPulse::default();
    let now = Instant::now();

    assert_eq!(pulse.frame(1, now), 0.0);
    assert!(!pulse.is_animating());

    let start = pulse.frame(2, now);
    assert!(start > 0.0);
    assert!(pulse.is_animating());

    let middle = pulse.frame(2, now + FOCUS_PULSE_DURATION / 2);
    assert!(middle > 0.0);
    assert!(middle < start);

    let end = pulse.frame(2, now + FOCUS_PULSE_DURATION);
    assert_eq!(end, 0.0);
    assert!(!pulse.is_animating());
}

#[test]
fn bitmap_text_normalization_sanitizes_panel_titles() {
    assert_eq!(
        normalize_bitmap_text("fox · coordinator"),
        "FOX COORDINATOR"
    );
    assert_eq!(normalize_bitmap_text("agent-12"), "AGENT-12");
    assert_eq!(bitmap_text_width("NAV", 2.0), 34.0);
}

#[test]
fn bitmap_text_wrapping_breaks_on_words() {
    assert_eq!(
        wrap_bitmap_text("ONE TWO THREE", 1.0, bitmap_char_advance(1.0) * 7.0),
        vec!["ONE TWO", "THREE"]
    );
}

#[test]
fn bitmap_text_wrapping_splits_long_words() {
    assert_eq!(
        wrap_bitmap_text("ABCDEFGHI", 1.0, bitmap_char_advance(1.0) * 4.0),
        vec!["ABCD", "EFGH", "I"]
    );
}

#[test]
fn single_session_typography_targets_jetbrains_mono_light_nerd() {
    assert_eq!(SINGLE_SESSION_FONT_FAMILY, "JetBrainsMono Nerd Font");
    assert_eq!(SINGLE_SESSION_FONT_WEIGHT, "Light");
    assert!(SINGLE_SESSION_FONT_FALLBACKS.contains(&"monospace"));
    assert!(SINGLE_SESSION_TITLE_FONT_SIZE >= 28.0);
    assert!(SINGLE_SESSION_BODY_FONT_SIZE >= 22.0);
    assert!(SINGLE_SESSION_CODE_FONT_SIZE >= 21.0);
    assert!(SINGLE_SESSION_TITLE_FONT_SIZE > SINGLE_SESSION_BODY_FONT_SIZE);
    assert!(SINGLE_SESSION_BODY_FONT_SIZE > SINGLE_SESSION_META_FONT_SIZE);
    assert!(SINGLE_SESSION_CODE_FONT_SIZE <= SINGLE_SESSION_BODY_FONT_SIZE);
    assert!(SINGLE_SESSION_BODY_LINE_HEIGHT > SINGLE_SESSION_CODE_LINE_HEIGHT);
    assert!(SINGLE_SESSION_CODE_LINE_HEIGHT > SINGLE_SESSION_META_LINE_HEIGHT);
}

#[test]
fn single_session_vertices_include_a_draft_caret() {
    let mut app = SingleSessionApp::new(None);
    let empty_vertices = build_single_session_vertices(&app, PhysicalSize::new(640, 480), 0.0);
    app.handle_key(KeyInput::Character("abc".to_string()));
    let typed_vertices = build_single_session_vertices(&app, PhysicalSize::new(640, 480), 0.0);

    assert!(typed_vertices.len() >= empty_vertices.len());
    assert!(
        typed_vertices
            .iter()
            .any(|vertex| vertex.color == SINGLE_SESSION_CARET_COLOR)
    );
}

#[test]
fn single_session_ctrl_backspace_deletes_previous_word() {
    let mut app = SingleSessionApp::new(None);
    app.handle_key(KeyInput::Character("hello desktop world".to_string()));

    assert_eq!(
        app.handle_key(KeyInput::DeletePreviousWord),
        KeyOutcome::Redraw
    );
    assert_eq!(app.draft, "hello desktop ");
}

#[test]
fn single_session_without_session_is_native_fresh_draft() {
    let mut app = SingleSessionApp::new(None);

    assert!(app.status_title().contains("single session"));
    assert_eq!(
        app.handle_key(KeyInput::SpawnPanel),
        KeyOutcome::SpawnSession
    );
    assert!(
        single_session_lines(None)
            .iter()
            .any(|line| line.contains("desktop-native"))
    );
}

#[test]
fn fresh_single_session_submit_requests_backend_session() {
    let mut app = SingleSessionApp::new(None);
    app.handle_key(KeyInput::Character("hello desktop".to_string()));

    assert_eq!(
        app.handle_key(KeyInput::SubmitDraft),
        KeyOutcome::StartFreshSession {
            message: "hello desktop".to_string()
        }
    );
    assert!(app.draft.is_empty());
}

#[test]
fn default_single_session_app_starts_without_attaching_recent_session() {
    let DesktopApp::SingleSession(mut app) = fresh_single_session_app() else {
        panic!("default desktop app should be single-session mode");
    };

    assert!(app.session.is_none());
    assert_eq!(
        app.handle_key(KeyInput::SpawnPanel),
        KeyOutcome::SpawnSession
    );
}

#[test]
fn single_session_spawn_resets_to_fresh_native_draft() {
    let card = workspace::SessionCard {
        session_id: "session_alpha".to_string(),
        title: "alpha".to_string(),
        subtitle: "active".to_string(),
        detail: "3 msgs".to_string(),
        preview_lines: Vec::new(),
        detail_lines: Vec::new(),
    };
    let mut app = SingleSessionApp::new(Some(card));
    app.handle_key(KeyInput::Character("draft".to_string()));

    app.reset_fresh_session();

    assert!(app.session.is_none());
    assert!(app.draft.is_empty());
    assert_eq!(app.detail_scroll, 0);
    assert!(app.status_title().contains("fresh session"));
}

#[test]
fn single_session_wraps_one_session_card() {
    let card = workspace::SessionCard {
        session_id: "session_alpha".to_string(),
        title: "alpha".to_string(),
        subtitle: "active".to_string(),
        detail: "3 msgs".to_string(),
        preview_lines: vec!["user hello".to_string()],
        detail_lines: vec!["assistant hi".to_string()],
    };
    let mut app = SingleSessionApp::new(Some(card));

    assert_eq!(app.handle_key(KeyInput::Enter), KeyOutcome::Redraw);
    assert_eq!(app.draft, "\n");
    app.handle_key(KeyInput::Character("draft".to_string()));
    assert_eq!(
        app.handle_key(KeyInput::SubmitDraft),
        KeyOutcome::SendDraft {
            session_id: "session_alpha".to_string(),
            title: "alpha".to_string(),
            message: "draft".to_string(),
        }
    );
}

#[test]
fn single_session_surface_is_the_panel_primitive() {
    let card = workspace::SessionCard {
        session_id: "session_alpha".to_string(),
        title: "alpha".to_string(),
        subtitle: "active".to_string(),
        detail: "3 msgs".to_string(),
        preview_lines: Vec::new(),
        detail_lines: Vec::new(),
    };

    let surface = single_session_surface(Some(&card));

    assert_eq!(surface.id, 1);
    assert_eq!(surface.title, "alpha");
    assert_eq!(surface.session_id.as_deref(), Some("session_alpha"));
    assert_eq!((surface.lane, surface.column), (0, 0));
    assert!(
        surface
            .body_lines
            .contains(&"single session mode".to_string())
    );
}

#[test]
fn focused_panel_draft_only_shows_for_focused_insert_panel() {
    let mut workspace = Workspace::from_session_cards(vec![workspace::SessionCard {
        session_id: "a".to_string(),
        title: "alpha".to_string(),
        subtitle: "active".to_string(),
        detail: "1 msg".to_string(),
        preview_lines: Vec::new(),
        detail_lines: Vec::new(),
    }]);
    workspace.handle_key(KeyInput::Character("i".to_string()));
    workspace.handle_key(KeyInput::Character("draft text".to_string()));

    assert_eq!(
        focused_panel_draft(&workspace, workspace.focused_id),
        Some("draft text".to_string())
    );
    assert_eq!(
        focused_panel_draft(&workspace, workspace.focused_id + 1),
        None
    );
}
