use super::*;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputShellResult {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub failed_to_start: bool,
}

fn sanitize_fenced_block(text: &str) -> String {
    text.replace("```", "``\u{200b}`")
}

pub fn format_input_shell_result_markdown(shell: &InputShellResult) -> String {
    let status = if shell.failed_to_start {
        "✗ failed to start".to_string()
    } else if shell.exit_code == Some(0) {
        "✓ exit 0".to_string()
    } else if let Some(code) = shell.exit_code {
        format!("✗ exit {}", code)
    } else {
        "✗ terminated".to_string()
    };

    let mut meta = vec![status, Message::format_duration(shell.duration_ms)];
    if let Some(cwd) = shell.cwd.as_deref() {
        meta.push(format!("cwd `{}`", cwd));
    }
    if shell.truncated {
        meta.push("truncated".to_string());
    }

    let mut message = format!(
        "**Shell command** · {}\n\n```bash\n{}\n```",
        meta.join(" · "),
        sanitize_fenced_block(&shell.command)
    );

    if shell.output.trim().is_empty() {
        message.push_str("\n\n_No output._");
    } else {
        message.push_str(&format!(
            "\n\n```text\n{}\n```",
            sanitize_fenced_block(shell.output.trim_end())
        ));
    }

    message
}

pub fn input_shell_status_notice(shell: &InputShellResult) -> String {
    if shell.failed_to_start {
        "Shell command failed to start".to_string()
    } else if shell.exit_code == Some(0) {
        "Shell command completed".to_string()
    } else if let Some(code) = shell.exit_code {
        format!("Shell command failed (exit {})", code)
    } else {
        "Shell command terminated".to_string()
    }
}

fn format_background_task_status(status: &BackgroundTaskStatus) -> &'static str {
    match status {
        BackgroundTaskStatus::Completed => "✓ completed",
        BackgroundTaskStatus::Superseded => "↻ superseded",
        BackgroundTaskStatus::Failed => "✗ failed",
        BackgroundTaskStatus::Running => "running",
    }
}

fn normalize_background_task_preview(preview: &str) -> Option<String> {
    let normalized = preview.replace("\r\n", "\n").replace('\r', "\n");
    let trimmed = normalized.trim_end();
    if trimmed.trim().is_empty() {
        None
    } else {
        Some(sanitize_fenced_block(trimmed))
    }
}

pub fn format_background_task_notification_markdown(task: &BackgroundTaskCompleted) -> String {
    let exit_code = task
        .exit_code
        .map(|code| format!("exit {}", code))
        .unwrap_or_else(|| "exit n/a".to_string());

    let mut message = format!(
        "**Background task** `{}` · `{}` · {} · {:.1}s · {}",
        task.task_id,
        task.tool_name,
        format_background_task_status(&task.status),
        task.duration_secs,
        exit_code,
    );

    if let Some(preview) = normalize_background_task_preview(&task.output_preview) {
        message.push_str(&format!("\n\n```text\n{}\n```", preview));
    } else {
        message.push_str("\n\n_No output captured._");
    }

    message.push_str(&format!(
        "\n\n_Full output:_ `bg action=\"output\" task_id=\"{}\"`",
        task.task_id
    ));

    message
}

pub fn format_background_task_progress_markdown(task: &BackgroundTaskProgressEvent) -> String {
    format!(
        "**Background task progress** `{}` · `{}`\n\n{}",
        task.task_id,
        task.tool_name,
        crate::background::format_progress_display(&task.progress, 12)
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedBackgroundTaskProgressNotification {
    pub task_id: String,
    pub tool_name: String,
    pub detail: String,
    pub summary: String,
    pub source: Option<String>,
    pub percent: Option<f32>,
}

fn split_progress_source(detail: &str) -> (String, Option<String>) {
    for source in ["reported", "parsed", "estimated"] {
        let suffix = format!(" ({source})");
        if let Some(summary) = detail.strip_suffix(&suffix) {
            return (summary.trim().to_string(), Some(source.to_string()));
        }
    }
    (detail.trim().to_string(), None)
}

fn strip_progress_bar_prefix(summary: &str) -> &str {
    if summary.starts_with('[')
        && let Some((bar, rest)) = summary.split_once("] ")
        && bar.chars().all(|ch| matches!(ch, '[' | '#' | '-'))
    {
        return rest.trim();
    }
    summary.trim()
}

fn parse_progress_percent(summary: &str) -> Option<f32> {
    static PERCENT_RE: OnceLock<Option<Regex>> = OnceLock::new();
    let percent_re = PERCENT_RE
        .get_or_init(|| compile_static_regex(r"(?P<percent>[0-9]+(?:\.[0-9]+)?)%"))
        .as_ref()?;
    let captures = percent_re.captures(summary)?;
    captures["percent"].parse::<f32>().ok()
}

pub fn parse_background_task_progress_notification_markdown(
    content: &str,
) -> Option<ParsedBackgroundTaskProgressNotification> {
    static HEADER_RE: OnceLock<Option<Regex>> = OnceLock::new();
    static INLINE_RE: OnceLock<Option<Regex>> = OnceLock::new();

    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let trimmed = normalized.trim();

    let header_re = HEADER_RE
        .get_or_init(|| {
            compile_static_regex(
                r"^\*\*Background task progress\*\* `(?P<task_id>[^`]+)` · `(?P<tool_name>[^`]+)`$",
            )
        })
        .as_ref()?;
    let inline_re = INLINE_RE
        .get_or_init(|| {
            compile_static_regex(
                r"^\*\*Background task progress\*\* `(?P<task_id>[^`]+)` · `(?P<tool_name>[^`]+)` · (?P<detail>.+)$",
            )
        })
        .as_ref()?;

    let (task_id, tool_name, detail) = if let Some(captures) = inline_re.captures(trimmed) {
        (
            captures["task_id"].to_string(),
            captures["tool_name"].to_string(),
            captures["detail"].trim().to_string(),
        )
    } else {
        let mut lines = trimmed.lines();
        let header = lines.next()?.trim();
        let captures = header_re.captures(header)?;
        let detail = lines
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if detail.is_empty() {
            return None;
        }
        (
            captures["task_id"].to_string(),
            captures["tool_name"].to_string(),
            detail,
        )
    };

    let (summary_with_bar, source) = split_progress_source(&detail);
    let summary = strip_progress_bar_prefix(&summary_with_bar).to_string();
    let percent = parse_progress_percent(&summary);

    Some(ParsedBackgroundTaskProgressNotification {
        task_id,
        tool_name,
        detail,
        summary,
        source,
        percent,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedBackgroundTaskNotification {
    pub task_id: String,
    pub tool_name: String,
    pub status: String,
    pub duration: String,
    pub exit_label: String,
    pub preview: Option<String>,
    pub full_output_command: String,
}

pub fn parse_background_task_notification_markdown(
    content: &str,
) -> Option<ParsedBackgroundTaskNotification> {
    static HEADER_RE: OnceLock<Option<Regex>> = OnceLock::new();
    static FULL_OUTPUT_RE: OnceLock<Option<Regex>> = OnceLock::new();

    let header_re = HEADER_RE
        .get_or_init(|| {
            compile_static_regex(
                r"^\*\*Background task\*\* `(?P<task_id>[^`]+)` · `(?P<tool_name>[^`]+)` · (?P<status>.+?) · (?P<duration>[0-9]+(?:\.[0-9]+)?s) · (?P<exit_label>.+)$",
            )
        })
        .as_ref()?;
    let full_output_re = FULL_OUTPUT_RE
        .get_or_init(|| compile_static_regex(r#"^_Full output:_ `(?P<command>[^`]+)`$"#))
        .as_ref()?;

    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let mut sections = normalized.split("\n\n");
    let header = sections.next()?.trim();
    let captures = header_re.captures(header)?;

    let mut preview: Option<String> = None;
    let mut full_output_command: Option<String> = None;

    for section in sections {
        let trimmed = section.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(captures) = full_output_re.captures(trimmed) {
            full_output_command = Some(captures["command"].to_string());
            continue;
        }

        if trimmed == "_No output captured._" {
            preview = None;
            continue;
        }

        if let Some(fenced) = trimmed
            .strip_prefix("```text\n")
            .and_then(|body| body.strip_suffix("\n```"))
        {
            preview = Some(fenced.to_string());
        }
    }

    Some(ParsedBackgroundTaskNotification {
        task_id: captures["task_id"].to_string(),
        tool_name: captures["tool_name"].to_string(),
        status: captures["status"].to_string(),
        duration: captures["duration"].to_string(),
        exit_label: captures["exit_label"].to_string(),
        preview,
        full_output_command: full_output_command?,
    })
}

pub fn background_task_status_notice(task: &BackgroundTaskCompleted) -> String {
    match task.status {
        BackgroundTaskStatus::Completed => {
            format!("Background task completed · {}", task.tool_name)
        }
        BackgroundTaskStatus::Superseded => {
            format!("Background task superseded · {}", task.tool_name)
        }
        BackgroundTaskStatus::Failed => match task.exit_code {
            Some(code) => format!(
                "Background task failed · {} · exit {}",
                task.tool_name, code
            ),
            None => format!("Background task failed · {}", task.tool_name),
        },
        BackgroundTaskStatus::Running => format!("Background task running · {}", task.tool_name),
    }
}
