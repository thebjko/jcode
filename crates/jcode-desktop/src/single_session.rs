use crate::workspace;
use workspace::{KeyInput, KeyOutcome};

pub(crate) const SINGLE_SESSION_FONT_FAMILY: &str = "JetBrainsMono Nerd Font";
pub(crate) const SINGLE_SESSION_FONT_WEIGHT: &str = "Light";
pub(crate) const SINGLE_SESSION_FONT_FALLBACKS: &[&str] = &[
    "JetBrainsMono Nerd Font Mono",
    "JetBrains Mono",
    "monospace",
];
pub(crate) const SINGLE_SESSION_TITLE_FONT_SIZE: f32 = 18.0;
pub(crate) const SINGLE_SESSION_BODY_FONT_SIZE: f32 = 15.0;
pub(crate) const SINGLE_SESSION_META_FONT_SIZE: f32 = 12.0;
pub(crate) const SINGLE_SESSION_CODE_FONT_SIZE: f32 = 14.0;
pub(crate) const SINGLE_SESSION_BODY_LINE_HEIGHT: f32 = 1.45;
pub(crate) const SINGLE_SESSION_CODE_LINE_HEIGHT: f32 = 1.35;
pub(crate) const SINGLE_SESSION_META_LINE_HEIGHT: f32 = 1.25;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct SingleSessionTypography {
    pub(crate) family: &'static str,
    pub(crate) weight: &'static str,
    pub(crate) fallbacks: &'static [&'static str],
    pub(crate) title_size: f32,
    pub(crate) body_size: f32,
    pub(crate) meta_size: f32,
    pub(crate) code_size: f32,
    pub(crate) body_line_height: f32,
    pub(crate) code_line_height: f32,
    pub(crate) meta_line_height: f32,
}

pub(crate) const fn single_session_typography() -> SingleSessionTypography {
    SingleSessionTypography {
        family: SINGLE_SESSION_FONT_FAMILY,
        weight: SINGLE_SESSION_FONT_WEIGHT,
        fallbacks: SINGLE_SESSION_FONT_FALLBACKS,
        title_size: SINGLE_SESSION_TITLE_FONT_SIZE,
        body_size: SINGLE_SESSION_BODY_FONT_SIZE,
        meta_size: SINGLE_SESSION_META_FONT_SIZE,
        code_size: SINGLE_SESSION_CODE_FONT_SIZE,
        body_line_height: SINGLE_SESSION_BODY_LINE_HEIGHT,
        code_line_height: SINGLE_SESSION_CODE_LINE_HEIGHT,
        meta_line_height: SINGLE_SESSION_META_LINE_HEIGHT,
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SingleSessionApp {
    pub(crate) session: Option<workspace::SessionCard>,
    pub(crate) draft: String,
    pub(crate) detail_scroll: usize,
}

impl SingleSessionApp {
    pub(crate) fn new(session: Option<workspace::SessionCard>) -> Self {
        Self {
            session,
            draft: String::new(),
            detail_scroll: 0,
        }
    }

    pub(crate) fn replace_session(&mut self, session: Option<workspace::SessionCard>) {
        self.session = session;
        self.detail_scroll = 0;
    }

    pub(crate) fn reset_fresh_session(&mut self) {
        self.session = None;
        self.draft.clear();
        self.detail_scroll = 0;
    }

    pub(crate) fn status_title(&self) -> String {
        let title = self
            .session
            .as_ref()
            .map(|session| session.title.as_str())
            .unwrap_or("fresh session");
        format!(
            "Jcode Desktop · single session · {title} · Ctrl+Enter send · Enter newline · Ctrl+; spawn · Ctrl+R refresh · Esc quit · --workspace for Niri layout"
        )
    }

    pub(crate) fn handle_key(&mut self, key: KeyInput) -> KeyOutcome {
        match key {
            KeyInput::SpawnPanel => KeyOutcome::SpawnSession,
            KeyInput::RefreshSessions => KeyOutcome::Redraw,
            KeyInput::SubmitDraft => self.submit_draft(),
            KeyInput::Escape => KeyOutcome::Exit,
            KeyInput::Enter => {
                self.draft.push('\n');
                KeyOutcome::Redraw
            }
            KeyInput::Backspace => {
                self.draft.pop();
                KeyOutcome::Redraw
            }
            KeyInput::Character(text) => {
                self.draft.push_str(&text);
                KeyOutcome::Redraw
            }
            _ => KeyOutcome::None,
        }
    }

    fn submit_draft(&mut self) -> KeyOutcome {
        let message = self.draft.trim().to_string();
        if message.is_empty() {
            return KeyOutcome::None;
        }
        let Some(session) = &self.session else {
            self.draft.clear();
            return KeyOutcome::StartFreshSession { message };
        };
        let session_id = session.session_id.clone();
        let title = session.title.clone();
        self.draft.clear();
        KeyOutcome::SendDraft {
            session_id,
            title,
            message,
        }
    }
}

pub(crate) fn single_session_surface(
    session: Option<&workspace::SessionCard>,
) -> workspace::Surface {
    let lines = single_session_lines(session);
    workspace::Surface {
        id: 1,
        title: session
            .map(|session| session.title.clone())
            .unwrap_or_else(|| "new jcode session".to_string()),
        body_lines: lines.clone(),
        detail_lines: lines,
        session_id: session.map(|session| session.session_id.clone()),
        lane: 0,
        column: 0,
        color_index: 0,
    }
}

pub(crate) fn single_session_lines(session: Option<&workspace::SessionCard>) -> Vec<String> {
    let Some(session) = session else {
        return vec![
            "single session mode".to_string(),
            "fresh desktop-native session draft".to_string(),
            "type here without nav or insert modes".to_string(),
            "ctrl+enter will send once desktop-native execution is connected".to_string(),
            "ctrl+; clears this draft and starts another fresh desktop session".to_string(),
            "run with --workspace for the niri layout wrapper".to_string(),
        ];
    };

    let mut lines = vec![
        "single session mode".to_string(),
        session.subtitle.clone(),
        session.detail.clone(),
    ];
    if !session.preview_lines.is_empty() {
        lines.push("recent transcript".to_string());
        lines.extend(session.preview_lines.clone());
    }
    if !session.detail_lines.is_empty() {
        lines.push("expanded transcript".to_string());
        lines.extend(session.detail_lines.clone());
    }
    lines
}
