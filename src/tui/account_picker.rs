use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph, Wrap},
};

const PANEL_BG: Color = Color::Rgb(24, 28, 40);
const PANEL_BORDER: Color = Color::Rgb(90, 95, 110);
const PANEL_BORDER_ACTIVE: Color = Color::Rgb(120, 140, 190);

#[derive(Debug, Clone)]
pub enum AccountProviderKind {
    Anthropic,
    OpenAi,
}

#[derive(Debug, Clone)]
pub enum AccountPickerCommand {
    SubmitInput(String),
    PromptValue {
        prompt: String,
        command_prefix: String,
        empty_value: Option<String>,
        status_notice: String,
    },
    Switch {
        provider: AccountProviderKind,
        label: String,
    },
    Login {
        provider: AccountProviderKind,
        label: String,
    },
    Remove {
        provider: AccountProviderKind,
        label: String,
    },
    PromptNew {
        provider: AccountProviderKind,
    },
}

#[derive(Debug, Clone)]
pub struct AccountPickerItem {
    pub provider_id: String,
    pub provider_label: String,
    pub title: String,
    pub subtitle: String,
    pub command: AccountPickerCommand,
}

impl AccountPickerItem {
    pub fn action(
        provider_id: impl Into<String>,
        provider_label: impl Into<String>,
        title: impl Into<String>,
        subtitle: impl Into<String>,
        command: AccountPickerCommand,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            provider_label: provider_label.into(),
            title: title.into(),
            subtitle: subtitle.into(),
            command,
        }
    }

    fn matches_filter(&self, filter: &str) -> bool {
        if filter.is_empty() {
            return true;
        }
        let haystack = format!(
            "{} {} {} {}",
            self.provider_id, self.provider_label, self.title, self.subtitle
        )
        .to_lowercase();
        filter
            .split_whitespace()
            .all(|needle| haystack.contains(&needle.to_lowercase()))
    }
}

#[derive(Debug, Clone)]
pub struct AccountPicker {
    title: String,
    items: Vec<AccountPickerItem>,
    filtered: Vec<usize>,
    selected: usize,
    filter: String,
}

pub enum OverlayAction {
    Continue,
    Close,
    Execute(AccountPickerCommand),
}

impl AccountPicker {
    pub fn new(title: impl Into<String>, items: Vec<AccountPickerItem>) -> Self {
        let mut picker = Self {
            title: title.into(),
            items,
            filtered: Vec::new(),
            selected: 0,
            filter: String::new(),
        };
        picker.apply_filter();
        picker
    }

    fn selected_item(&self) -> Option<&AccountPickerItem> {
        self.filtered
            .get(self.selected)
            .and_then(|idx| self.items.get(*idx))
    }

    fn apply_filter(&mut self) {
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(idx, item)| item.matches_filter(&self.filter).then_some(idx))
            .collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }

    pub fn handle_overlay_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Result<OverlayAction> {
        match code {
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.apply_filter();
                    return Ok(OverlayAction::Continue);
                }
                return Ok(OverlayAction::Close);
            }
            KeyCode::Char('q') if !modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(OverlayAction::Close);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let max = self.filtered.len().saturating_sub(1);
                self.selected = (self.selected + 1).min(max);
            }
            KeyCode::PageUp | KeyCode::Char('K') => {
                self.selected = self.selected.saturating_sub(8);
            }
            KeyCode::PageDown | KeyCode::Char('J') => {
                let max = self.filtered.len().saturating_sub(1);
                self.selected = (self.selected + 8).min(max);
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.selected = 0;
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.selected = self.filtered.len().saturating_sub(1);
            }
            KeyCode::Backspace => {
                if self.filter.pop().is_some() {
                    self.apply_filter();
                }
            }
            KeyCode::Enter => {
                if let Some(item) = self.selected_item() {
                    return Ok(OverlayAction::Execute(item.command.clone()));
                }
                return Ok(OverlayAction::Close);
            }
            KeyCode::Char(c)
                if !modifiers.contains(KeyModifiers::CONTROL)
                    && !modifiers.contains(KeyModifiers::ALT) =>
            {
                self.filter.push(c);
                self.apply_filter();
            }
            _ => {}
        }
        Ok(OverlayAction::Continue)
    }

    pub fn render(&self, frame: &mut Frame) {
        let area = centered_rect(84, 68, frame.area());

        let block = Block::default()
            .title(format!(" {} ", self.title))
            .title_bottom(Line::from(vec![
                Span::styled(
                    " Enter ",
                    Style::default().fg(Color::White).bg(Color::DarkGray),
                ),
                Span::styled(" run  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    " / ",
                    Style::default().fg(Color::White).bg(Color::DarkGray),
                ),
                Span::styled(" type to filter  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    " Esc ",
                    Style::default().fg(Color::White).bg(Color::DarkGray),
                ),
                Span::styled(" close/clear filter ", Style::default().fg(Color::DarkGray)),
            ]))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(PANEL_BORDER));
        frame.render_widget(block, area);

        let inner = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(10), Constraint::Length(2)])
            .split(inner);

        let filter_line = vec![
            Span::styled("Search ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                if self.filter.is_empty() {
                    "type provider, account, or setting".to_string()
                } else {
                    self.filter.clone()
                },
                if self.filter.is_empty() {
                    Style::default().fg(Color::Gray).italic()
                } else {
                    Style::default().fg(Color::White)
                },
            ),
            Span::styled(
                format!("  {} results", self.filtered.len()),
                Style::default().fg(Color::DarkGray),
            ),
        ];
        frame.render_widget(Paragraph::new(Line::from(filter_line)), rows[0]);

        let list_block = Block::default()
            .title(Span::styled(
                " Accounts & Provider Settings ",
                Style::default().fg(Color::White).bold(),
            ))
            .borders(Borders::ALL)
            .style(Style::default().bg(PANEL_BG))
            .border_style(Style::default().fg(PANEL_BORDER_ACTIVE));
        let list_inner = list_block.inner(rows[1]);
        frame.render_widget(list_block, rows[1]);

        let available_rows = list_inner.height.max(1) as usize;
        let start = self
            .selected
            .saturating_sub(available_rows.saturating_sub(1).min(available_rows / 2));
        let end = (start + available_rows).min(self.filtered.len());

        let mut lines = Vec::new();
        if self.filtered.is_empty() {
            lines.push(Line::from(Span::styled(
                "No matching account or provider actions",
                Style::default().fg(Color::Gray).italic(),
            )));
        } else {
            for visible_idx in start..end {
                let idx = self.filtered[visible_idx];
                let item = &self.items[idx];
                let selected = visible_idx == self.selected;
                let row_style = if selected {
                    Style::default().bg(Color::Rgb(38, 42, 56))
                } else {
                    Style::default()
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        if selected { "▸ " } else { "  " },
                        row_style.fg(Color::White),
                    ),
                    Span::styled(
                        format!("{:<18}", item.provider_label),
                        row_style.patch(provider_style(&item.provider_id)),
                    ),
                    Span::styled(format!(" {}", item.title), row_style.fg(Color::White)),
                ]));
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        truncate_with_ellipsis(&item.subtitle, list_inner.width.saturating_sub(2) as usize),
                        row_style.fg(Color::Gray),
                    ),
                ]));
            }
        }

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), list_inner);

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("Tip ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                "Use `/account <provider> settings` for a text view or edit defaults directly here.",
                Style::default().fg(Color::Gray),
            ),
        ]));
        frame.render_widget(footer, rows[2]);
    }
}

fn provider_style(provider_id: &str) -> Style {
    let color = match provider_id {
        "claude" => Color::Rgb(229, 187, 111),
        "openai" => Color::Rgb(111, 214, 181),
        "gemini" | "google" => Color::Rgb(129, 184, 255),
        "copilot" => Color::Rgb(182, 154, 255),
        "cursor" => Color::Rgb(131, 215, 255),
        "openrouter" | "openai-compatible" | "opencode" | "opencode-go" | "zai"
        | "chutes" | "cerebras" | "alibaba-coding-plan" | "jcode" => {
            Color::Rgb(189, 200, 255)
        }
        _ => Color::Rgb(180, 190, 220),
    };
    Style::default().fg(color).bold()
}

fn truncate_with_ellipsis(input: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let chars: Vec<char> = input.chars().collect();
    if chars.len() <= width {
        return input.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let mut out: String = chars.into_iter().take(width - 1).collect();
    out.push('…');
    out
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, widgets::Paragraph};

    #[test]
    fn test_account_picker_preserves_underlying_background_outside_panels() {
        let picker = AccountPicker::new(
            " Accounts ",
            vec![AccountPickerItem::action(
                "openai",
                "OpenAI",
                "Add account",
                "Start login flow",
                AccountPickerCommand::SubmitInput("/account openai add default".to_string()),
            )],
        );

        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).expect("failed to create terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                let fill = vec![Line::from("X".repeat(area.width as usize)); area.height as usize];
                frame.render_widget(Paragraph::new(fill), area);
                picker.render(frame);
            })
            .expect("draw failed");

        let overlay = centered_rect(84, 68, Rect::new(0, 0, 40, 12));
        let probe = &terminal.backend().buffer()[(overlay.x + overlay.width - 3, overlay.y + 2)];
        assert_eq!(probe.symbol(), "X");
        assert_ne!(probe.bg, Color::Rgb(18, 21, 30));
    }
}
